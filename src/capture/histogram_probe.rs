// -----------------------------------------------------------------------------
// webOS 24/25 Histogram Capture Probe (reverse-engineered libvtcapture API)
// -----------------------------------------------------------------------------

use anyhow::{Result, anyhow};
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{error, info, warn};

use super::histogram_types::{HistogramBuffer, LIBVTCAPTURE_PATH, VtHistogramEntry};

/// Reinitialization request raised by the policy callback.
///
/// The recovered API passes a bare `bool (*)(void)` with no user-data pointer
/// (decompiled as `std::function<bool()>`), so the trampoline has nowhere to
/// store per-probe state and must signal through a process-global.
pub static REINIT: AtomicBool = AtomicBool::new(false);

/// Policy-callback trampoline: records the "video pipeline changed" event and
/// reports it unhandled, mirroring the official app's callback; the next
/// capture call re-initializes the histogram session.
unsafe extern "C" fn policy_trampoline() -> bool {
    REINIT.store(true, Ordering::SeqCst);
    false
}

/// `bool (*)(void)` policy callback handed to `vtCapture_initHistogram`.
type HistogramPolicyCallback = unsafe extern "C" fn() -> bool;
type FnVtCaptureInitHistogram = unsafe extern "C" fn(HistogramPolicyCallback) -> *mut c_void;
type FnVtCaptureSetPresetHistogram = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type FnVtCaptureGetHistogram = unsafe extern "C" fn(*mut c_void, *mut VtHistogramEntry) -> i32;
type FnVtCaptureFinalizeHistogram = unsafe extern "C" fn(*mut c_void) -> i32;

/// Per-channel sums of one zone under the u8 interpretation: every byte of
/// each 512-byte block contributes to its channel total.
pub fn zone_summary(entry: &VtHistogramEntry) -> (u64, u64, u64) {
    (
        u8_block_sum(&entry.channels[0..512]),
        u8_block_sum(&entry.channels[512..1024]),
        u8_block_sum(&entry.channels[1024..1536]),
    )
}

/// Same as `zone_summary` under the u16 interpretation: each block is 256
/// little-endian `u16` bins. Kept for on-device cross-checks of the bin
/// semantics (u8 vs u16 is settled by the probe's raw first-byte dumps).
#[allow(dead_code)]
pub fn zone_summary_u16(entry: &VtHistogramEntry) -> (u64, u64, u64) {
    (
        u16_block_sum(&entry.channels[0..512]),
        u16_block_sum(&entry.channels[512..1024]),
        u16_block_sum(&entry.channels[1024..1536]),
    )
}

fn u8_block_sum(block: &[u8]) -> u64 {
    block.iter().map(|&byte| u64::from(byte)).sum()
}

fn u16_block_sum(block: &[u8]) -> u64 {
    block
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u64::from(u16::from_le_bytes(*pair)))
        .sum()
}

/// On-device diagnostic probe for the histogram capture API. Never writes
/// frames to disk and never drives light output.
#[derive(Debug)]
pub struct HistogramProbe {
    handle: *mut c_void,
    fn_init_histogram: FnVtCaptureInitHistogram,
    fn_set_preset_histogram: FnVtCaptureSetPresetHistogram,
    fn_get_histogram: FnVtCaptureGetHistogram,
    fn_finalize_histogram: FnVtCaptureFinalizeHistogram,
    _lib: Library,
}

impl HistogramProbe {
    /// Loads the histogram capture API from `libvtcapture.so.1` and initializes
    /// a session armed with the policy trampoline.
    pub fn try_new() -> Result<Self> {
        if !Path::new(LIBVTCAPTURE_PATH).exists() {
            return Err(anyhow!(
                "histogram probe: libvtcapture driver library not found at {}",
                LIBVTCAPTURE_PATH
            ));
        }
        info!(
            "Loading webOS histogram capture API from {}...",
            LIBVTCAPTURE_PATH
        );
        let lib = unsafe { Library::new(LIBVTCAPTURE_PATH) }
            .map_err(|error| anyhow!("Failed to dlopen {}: {}", LIBVTCAPTURE_PATH, error))?;

        unsafe {
            let fn_init_histogram: Symbol<FnVtCaptureInitHistogram> = lib
                .get(b"vtCapture_initHistogram\0")
                .map_err(|error| anyhow!("Symbol vtCapture_initHistogram not found: {}", error))?;
            let fn_set_preset_histogram: Symbol<FnVtCaptureSetPresetHistogram> = lib
                .get(b"vtCapture_setPresetHistogram\0")
                .map_err(|error| {
                    anyhow!("Symbol vtCapture_setPresetHistogram not found: {}", error)
                })?;
            let fn_get_histogram: Symbol<FnVtCaptureGetHistogram> = lib
                .get(b"vtCapture_getHistogram\0")
                .map_err(|error| anyhow!("Symbol vtCapture_getHistogram not found: {}", error))?;
            let fn_finalize_histogram: Symbol<FnVtCaptureFinalizeHistogram> =
                lib.get(b"vtCapture_finalizeHistogram\0").map_err(|error| {
                    anyhow!("Symbol vtCapture_finalizeHistogram not found: {}", error)
                })?;

            let mut probe = Self {
                handle: std::ptr::null_mut(),
                fn_init_histogram: *fn_init_histogram,
                fn_set_preset_histogram: *fn_set_preset_histogram,
                fn_get_histogram: *fn_get_histogram,
                fn_finalize_histogram: *fn_finalize_histogram,
                _lib: lib,
            };
            probe.init_session()?;
            info!("[+] histogram capture session initialized (policy callback armed)");
            Ok(probe)
        }
    }

    /// Calls `vtCapture_initHistogram` with the policy trampoline and stores
    /// the returned handle (NULL means failure).
    fn init_session(&mut self) -> Result<()> {
        let handle = unsafe { (self.fn_init_histogram)(policy_trampoline) };
        if handle.is_null() {
            return Err(anyhow!("vtCapture_initHistogram returned NULL"));
        }
        self.handle = handle;
        Ok(())
    }

    /// Applies a histogram preset (0..3) and returns the native result code.
    pub fn set_preset(&mut self, preset: i32) -> i32 {
        unsafe { (self.fn_set_preset_histogram)(self.handle, preset) }
    }

    /// Requests one histogram dump into `buf` and returns the native result
    /// code (0 = ok; 2 = null handle/not active; 3 = null buffer/bad preset).
    pub fn get(&mut self, buf: &mut HistogramBuffer) -> i32 {
        unsafe { (self.fn_get_histogram)(self.handle, buf.as_mut_ptr()) }
    }

    /// Finalizes the histogram session and clears the handle. Idempotent: a
    /// cleared (or never-acquired) handle reports success without an FFI call.
    pub fn finalize(&mut self) -> i32 {
        let handle = self.handle;
        self.handle = std::ptr::null_mut();
        if handle.is_null() {
            return 0;
        }
        unsafe { (self.fn_finalize_histogram)(handle) }
    }

    /// Runs the diagnostic loop: `iterations` polls `period` apart at `preset`
    /// with `zones` entries, logging per-zone channel sums and the raw first
    /// channel bytes of the first and last zone (u8-vs-u16 disambiguation).
    pub fn run(&mut self, iterations: u32, period: Duration, preset: i32, zones: usize) {
        let rc = self.set_preset(preset);
        if rc != 0 {
            warn!("vtCapture_setPresetHistogram({preset}) returned code {rc}");
        }

        for iteration in 1..=iterations {
            if REINIT.swap(false, Ordering::SeqCst) {
                info!("policy change: reinitializing");
                let rc = self.finalize();
                if rc != 0 {
                    warn!("vtCapture_finalizeHistogram returned code {rc} during re-init");
                }
                if let Err(error) = self.init_session() {
                    error!("histogram re-init failed: {error:#}");
                    std::thread::sleep(period);
                    continue;
                }
                let rc = self.set_preset(preset);
                if rc != 0 {
                    warn!(
                        "vtCapture_setPresetHistogram({preset}) returned code {rc} after re-init"
                    );
                }
            }

            let mut buffer = HistogramBuffer::new(zones);
            buffer.prefill_indices();
            let rc = self.get(&mut buffer);
            if rc != 0 {
                warn!(
                    "vtCapture_getHistogram returned code {rc} (iteration {iteration}/{iterations})"
                );
            } else {
                info!("histogram iteration {iteration}/{iterations}:");
                let last = buffer.zones().saturating_sub(1);
                for zone in 0..buffer.zones() {
                    let (channel0, channel1, channel2) = zone_summary(buffer.entry(zone));
                    info!("  zone {zone:2}: sums=({channel0}, {channel1}, {channel2})");
                    if zone == 0 || zone == last {
                        let raw = buffer.entry(zone).channels[..8]
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        info!("  zone {zone:2}: first channel bytes: {raw}");
                    }
                }
            }
            std::thread::sleep(period);
        }
    }
}

impl Drop for HistogramProbe {
    fn drop(&mut self) {
        // finalize() guards against a double finalization by clearing the
        // handle before (and instead of) the FFI call.
        let rc = self.finalize();
        if rc != 0 {
            warn!("vtCapture_finalizeHistogram returned code {rc} during drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn zone_summary_sums_each_512_byte_block_as_bytes() {
        let mut entry = VtHistogramEntry {
            index: 0,
            channels: [0; 1536],
        };
        entry.channels[..512].fill(1);
        assert_eq!(zone_summary(&entry), (512, 0, 0));

        entry.channels[600] = 7;
        assert_eq!(zone_summary(&entry), (512, 7, 0));

        entry.channels[1400] = 200;
        assert_eq!(zone_summary(&entry), (512, 7, 200));
    }

    #[test]
    fn zone_summary_u16_sums_little_endian_pairs() {
        let mut entry = VtHistogramEntry {
            index: 0,
            channels: [0; 1536],
        };
        for pair in entry.channels[..512].chunks_exact_mut(2) {
            pair[0] = 0x01; // little-endian 0x0001
        }
        assert_eq!(zone_summary_u16(&entry), (256, 0, 0));

        // Locks the byte order: bytes 02 01 decode as 0x0102 (258), not 0x0201.
        entry.channels[0] = 0x02;
        entry.channels[1] = 0x01;
        assert_eq!(zone_summary_u16(&entry), (256 - 1 + 0x0102, 0, 0));
    }

    #[test]
    fn policy_trampoline_sets_reinit_and_returns_false() {
        REINIT.store(false, Ordering::SeqCst);
        // SAFETY: the trampoline is a plain function with no preconditions.
        let handled = unsafe { policy_trampoline() };
        assert!(
            !handled,
            "trampoline must return false (caller re-initializes)"
        );
        assert!(
            REINIT.swap(false, Ordering::SeqCst),
            "trampoline must set REINIT"
        );
    }

    #[test]
    fn try_new_fails_cleanly_without_libvtcapture() {
        let error = HistogramProbe::try_new().expect_err("host has no /usr/lib/libvtcapture.so.1");
        let message = format!("{error:#}");
        assert!(
            message.contains("libvtcapture"),
            "error must name the library: {message}"
        );
    }
}
