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

use super::histogram_types::{
    CTRL_HISTOGRAM_DATA, CTRL_HISTOGRAM_ENABLE, CTRL_HISTOGRAM_PRESET, ENTRY_SIZE,
    HISTOGRAM_V4L2_DEVICE, HistogramBuffer, LIBVTCAPTURE_PATH, V4l2ExtControl, V4l2ExtControls,
    VIDIOC_G_EXT_CTRLS, VIDIOC_S_EXT_CTRLS, VtHistogramEntry, build_ext_controls,
};

/// Reinitialization request raised by the policy callback.
///
/// The recovered API passes a bare `bool (*)(void)` with no user-data pointer
/// (decompiled as `std::function<bool()>`), so the trampoline has nowhere to
/// store per-probe state and must signal through a process-global.
pub static REINIT: AtomicBool = AtomicBool::new(false);

/// Policy-callback trampoline: records the "video pipeline changed" event and
/// reports it unhandled, mirroring the official app's callback; the next
/// capture call re-initializes the histogram session.
pub(crate) unsafe extern "C" fn policy_trampoline() -> bool {
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
/// little-endian `u16` bins. Diagnostic counterpart of `zone_summary` for
/// the u8-vs-u16 bin disambiguation; its former consumer `zone_to_rgb` now
/// uses the weighted-mean helpers instead, and u8-vs-u16 is settled by the
/// on-device probe (Task 7).
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
    u16_bins(block).sum()
}

/// The block's 256 little-endian `u16` bin counts.
fn u16_bins(block: &[u8]) -> impl Iterator<Item = u64> + '_ {
    block
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u64::from(u16::from_le_bytes(*pair)))
}

/// Highest-count bin index and its count. Ties keep the lowest bin index; an
/// all-zero block reports bin 0 with count 0.
fn bins_argmax(bins: impl Iterator<Item = u64>) -> (usize, u64) {
    let mut best = (0, 0);
    for (index, count) in bins.enumerate() {
        if count > best.1 {
            best = (index, count);
        }
    }
    best
}

/// Count-weighted mean bin level: `sum(i * count[i]) / sum(count[i])`, 0.0
/// when every count is zero. The one implementation of the statistic, shared
/// by the probe diagnostics and `zone_to_rgb` through the two adapters below.
fn bins_weighted_mean(bins: impl Iterator<Item = u64>) -> f64 {
    let mut weighted = 0u64;
    let mut total = 0u64;
    for (index, count) in bins.enumerate() {
        weighted += index as u64 * count;
        total += count;
    }
    if total == 0 {
        0.0
    } else {
        weighted as f64 / total as f64
    }
}

/// `bins_argmax` over the block's 256 u8 bins.
fn u8_argmax(block: &[u8]) -> (usize, u64) {
    bins_argmax(block.iter().map(|&byte| u64::from(byte)))
}

/// `bins_weighted_mean` over the block's 256 u8 bins.
pub(crate) fn u8_weighted_mean(block: &[u8]) -> f64 {
    bins_weighted_mean(block.iter().map(|&byte| u64::from(byte)))
}

/// `bins_argmax` over the block's 256 little-endian u16 bins.
fn u16_argmax(block: &[u8]) -> (usize, u64) {
    bins_argmax(u16_bins(block))
}

/// `bins_weighted_mean` over the block's 256 little-endian u16 bins.
pub(crate) fn u16_weighted_mean(block: &[u8]) -> f64 {
    bins_weighted_mean(u16_bins(block))
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
                log_zone_summaries(&buffer);
            }
            std::thread::sleep(period);
        }
    }
}

/// Logs, per zone: the channel sums; for each of the three 512-byte channel
/// blocks the argmax bin (index + count) and the count-weighted mean bin
/// level under BOTH bin interpretations (u8 and u16 little-endian); the raw
/// 8-byte hex dump of the first channel for zones 0 and last
/// (u8-vs-u16 disambiguation); and, for zone 0, the FULL first channel block
/// (512 bytes) as hex, 32 bytes per line. Shared verbatim by the libvtcapture
/// probe and the direct V4L2 probe.
fn log_zone_summaries(buffer: &HistogramBuffer) {
    let last = buffer.zones().saturating_sub(1);
    for zone in 0..buffer.zones() {
        let entry = buffer.entry(zone);
        let (channel0, channel1, channel2) = zone_summary(entry);
        info!("  zone {zone:2}: sums=({channel0}, {channel1}, {channel2})");
        for (channel, block) in entry.channels.as_chunks::<512>().0.iter().enumerate() {
            let (u8_bin, u8_count) = u8_argmax(block);
            let u8_mean = u8_weighted_mean(block);
            let (u16_bin, u16_count) = u16_argmax(block);
            let u16_mean = u16_weighted_mean(block);
            info!(
                "  zone {zone:2}: ch{channel}: u8 argmax=bin {u8_bin} (count {u8_count}) mean={u8_mean:.1}; u16 argmax=bin {u16_bin} (count {u16_count}) mean={u16_mean:.1}"
            );
        }
        if zone == 0 || zone == last {
            let raw = entry.channels[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            info!("  zone {zone:2}: first channel bytes: {raw}");
        }
        if zone == 0 {
            for (line, chunk) in entry.channels[..512].as_chunks::<32>().0.iter().enumerate() {
                let hex = chunk
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                info!("  zone  0: ch0 hex @{:03x}: {hex}", line * 32);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Direct V4L2 probe: /dev/video60 without libvtcapture, without the Luna
// resource manager (both block a plain root process; the kernel node does not).
// The ABI-correct structs, control IDs, and ioctl numbers live in
// `histogram_types` (shared with the production backend).
// -----------------------------------------------------------------------------

/// Runs one `VIDIOC_*_EXT_CTRLS` call; a -1 return becomes an error naming
/// `step`, the errno, and the driver's `error_idx` when it set one.
fn ext_ctrls(
    fd: libc::c_int,
    request: libc::c_ulong,
    ctrls: &mut V4l2ExtControls,
    step: &str,
) -> Result<()> {
    // SAFETY: `ctrls` is a valid, exclusive borrow of an initialized
    // V4l2ExtControls whose `controls` pointer targets a live control for the
    // call's duration; the kernel only writes back through those addresses.
    let rc = unsafe { libc::ioctl(fd, request, ctrls as *mut V4l2ExtControls) };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        let errno = error.raw_os_error().unwrap_or(0);
        let error_idx = ctrls.error_idx;
        return Err(anyhow!(
            "direct histogram probe failed at step \"{step}\": {error} (errno {errno}, error_idx {error_idx})"
        ));
    }
    Ok(())
}

/// Runs the reverse-engineered control sequence against `fd`: enable, preset,
/// then `iterations` histogram reads `period` apart, logging each dump with
/// the same per-zone summaries as [`HistogramProbe::run`].
fn direct_sequence(
    fd: libc::c_int,
    iterations: u32,
    period: Duration,
    preset: i32,
    zones: usize,
) -> Result<()> {
    // Mirrors VtHistogram::start: value 0 enables the histogram engine.
    let mut enable = V4l2ExtControl {
        id: CTRL_HISTOGRAM_ENABLE,
        size: 0,
        reserved2: 0,
        value_or_ptr: 0,
        reserved3: 0,
    };
    let mut ctrls = build_ext_controls(&mut enable);
    ext_ctrls(fd, VIDIOC_S_EXT_CTRLS, &mut ctrls, "enable")?;

    // Mirrors VtHistogram::setPresetMode.
    let mut preset_ctrl = V4l2ExtControl {
        id: CTRL_HISTOGRAM_PRESET,
        size: 0,
        reserved2: 0,
        value_or_ptr: preset as u32,
        reserved3: 0,
    };
    let mut ctrls = build_ext_controls(&mut preset_ctrl);
    ext_ctrls(fd, VIDIOC_S_EXT_CTRLS, &mut ctrls, "preset")?;

    // Mirrors VtHistogram::getHistogramData: one payload of `zones` entries.
    for iteration in 1..=iterations {
        let mut buffer = HistogramBuffer::new(zones);
        buffer.prefill_indices();
        let mut data = V4l2ExtControl {
            id: CTRL_HISTOGRAM_DATA,
            size: (zones * ENTRY_SIZE) as u32,
            reserved2: 0,
            value_or_ptr: buffer.as_mut_ptr() as usize as u32,
            reserved3: 0,
        };
        let mut ctrls = build_ext_controls(&mut data);
        ext_ctrls(fd, VIDIOC_G_EXT_CTRLS, &mut ctrls, "get")?;
        info!("histogram iteration {iteration}/{iterations} (direct V4L2):");
        log_zone_summaries(&buffer);
        std::thread::sleep(period);
    }
    Ok(())
}

/// Raw V4L2 histogram probe: talks straight to `/dev/video60` with the
/// reverse-engineered control sequence, bypassing libvtcapture (whose init is
/// caller-whitelisted) and its Luna ResourceManagerClient (which aborts from a
/// plain root process). The minimal discriminating experiment for the DRM
/// hypothesis: if the kernel serves the controls here, the block is purely
/// userspace policy. Never writes frames to disk and never drives light
/// output.
pub fn probe_direct(iterations: u32, period: Duration, preset: i32, zones: usize) -> Result<()> {
    info!(
        "direct V4L2 histogram probe on {} (no libvtcapture, no resource manager)",
        HISTOGRAM_V4L2_DEVICE
    );
    let path = std::ffi::CString::new(HISTOGRAM_V4L2_DEVICE).map_err(|error| {
        anyhow!("invalid histogram device path {HISTOGRAM_V4L2_DEVICE:?}: {error}")
    })?;
    // SAFETY: `path` outlives the call and open() only reads it.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        // A missing node is itself a discriminating result; report the errno
        // without scanning other video devices.
        let error = std::io::Error::last_os_error();
        let errno = error.raw_os_error().unwrap_or(0);
        return Err(anyhow!(
            "direct histogram probe failed at step \"open {HISTOGRAM_V4L2_DEVICE}\": {error} (errno {errno})"
        ));
    }
    info!(
        "no disable/teardown control is known for the raw V4L2 path; the node is only closed, never disabled (harmless for a probe)"
    );
    let outcome = direct_sequence(fd, iterations, period, preset, zones);
    // SAFETY: `fd` is the open descriptor from above, closed exactly once.
    if unsafe { libc::close(fd) } < 0 {
        warn!(
            "close({}) failed: {}",
            HISTOGRAM_V4L2_DEVICE,
            std::io::Error::last_os_error()
        );
    }
    outcome
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
    fn u8_bin_helpers_argmax_and_weighted_mean() {
        let mut block = [0u8; 512];
        assert_eq!(
            u8_argmax(&block),
            (0, 0),
            "all-zero block reports bin 0 count 0"
        );
        assert_eq!(u8_weighted_mean(&block), 0.0, "zero total count means 0.0");

        // Review example: count 10 at bin 5, count 5 at bin 100.
        block[5] = 10;
        block[100] = 5;
        assert_eq!(u8_argmax(&block), (5, 10));
        let mean = u8_weighted_mean(&block);
        assert_eq!(mean, (5.0 * 10.0 + 100.0 * 5.0) / 15.0);
        assert_eq!(format!("{mean:.1}"), "36.7", "mean prints with 1 decimal");

        // Ties keep the lowest bin index.
        block[5] = 5;
        assert_eq!(u8_argmax(&block), (5, 5));
    }

    #[test]
    fn u16_bin_helpers_argmax_and_weighted_mean_are_little_endian() {
        let mut block = [0u8; 512];
        assert_eq!(u16_argmax(&block), (0, 0));
        assert_eq!(u16_weighted_mean(&block), 0.0);

        // bin 2 count 300 = bytes 2c 01 little-endian; bin 200 count 7.
        block[4..6].copy_from_slice(&300u16.to_le_bytes());
        block[400..402].copy_from_slice(&7u16.to_le_bytes());
        // A big-endian read of bin 2 would report 0x2c01 = 11265, so the
        // count assertion locks the byte order.
        assert_eq!(u16_argmax(&block), (2, 300));
        let mean = u16_weighted_mean(&block);
        assert_eq!(mean, (2.0 * 300.0 + 200.0 * 7.0) / 307.0);
        assert_eq!(format!("{mean:.1}"), "6.5");
    }

    #[test]
    fn try_new_fails_cleanly_without_libvtcapture() {
        if Path::new(LIBVTCAPTURE_PATH).exists() {
            // On a TV the real driver is present and try_new would dlopen it;
            // this test only asserts the clean host-side failure path.
            return;
        }
        let error = HistogramProbe::try_new().expect_err("host has no /usr/lib/libvtcapture.so.1");
        let message = format!("{error:#}");
        assert!(
            message.contains("libvtcapture"),
            "error must name the library: {message}"
        );
    }
}
