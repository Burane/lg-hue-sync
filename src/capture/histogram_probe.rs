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

use super::histogram_types::{ENTRY_SIZE, HistogramBuffer, LIBVTCAPTURE_PATH, VtHistogramEntry};

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
/// little-endian `u16` bins. Consumed by `zone_to_rgb` while u8-vs-u16 bin
/// semantics await on-device confirmation (u8 vs u16 is settled by the probe's
/// raw first-byte dumps).
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
                log_zone_summaries(&buffer);
            }
            std::thread::sleep(period);
        }
    }
}

/// Logs the per-zone channel sums, with a raw 8-byte hex dump of the first
/// channel for zones 0 and last (u8-vs-u16 disambiguation). Shared verbatim by
/// the libvtcapture probe and the direct V4L2 probe.
fn log_zone_summaries(buffer: &HistogramBuffer) {
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

// -----------------------------------------------------------------------------
// Direct V4L2 probe: /dev/video60 without libvtcapture, without the Luna
// resource manager (both block a plain root process; the kernel node does not).
// -----------------------------------------------------------------------------

/// Kernel node serving the histogram controls directly (webOS 25).
pub const HISTOGRAM_V4L2_DEVICE: &str = "/dev/video60";

/// V4L2 control IDs reverse-engineered from libvtcapture's VtHistogram
/// (V4L2_CTRL_CLASS_USER, 0x0098 << 16 | 0x2a1x).
const CTRL_HISTOGRAM_ENABLE: u32 = 0x0098_2a14;
const CTRL_HISTOGRAM_PRESET: u32 = 0x0098_2a15;
const CTRL_HISTOGRAM_DATA: u32 = 0x0098_2a16;

/// `VIDIOC_S_EXT_CTRLS` as issued by the TV's 32-bit ARM userspace
/// (0x18 = sizeof v4l2_ext_controls on that ABI).
const VIDIOC_S_EXT_CTRLS: libc::c_ulong = 0xc018_5648;
/// `VIDIOC_G_EXT_CTRLS`, same ABI.
const VIDIOC_G_EXT_CTRLS: libc::c_ulong = 0xc018_5647;

/// `struct v4l2_ext_control` exactly as laid out for the TV's 32-bit ARM ABI
/// (20 bytes). The trailing union member is 4 bytes on ARM32 (a raw value for
/// the value controls, or a buffer pointer for the payload control); it is
/// stored as `u32` so the layout stays fixed-width on 64-bit hosts, where the
/// size unit tests run. Pointer payloads are cast `as usize as u32`, lossless
/// on the ARM32 target that is the only caller reaching the ioctl.
#[repr(C)]
struct V4l2ExtControl {
    id: u32,
    size: u32,
    reserved: [u32; 2],
    value_or_ptr: u32,
}

/// `struct v4l2_ext_controls` (24 bytes, 32-bit ARM ABI; `controls` uses the
/// same fixed-width pointer storage as [`V4l2ExtControl`]).
#[repr(C)]
struct V4l2ExtControls {
    which: u32,
    count: u32,
    error_idx: u32,
    request_fd: i32,
    reserved: u32,
    controls: u32,
}

/// Builds the single-control `VIDIOC_*_EXT_CTRLS` wrapper: old-style USER
/// class `which`, exactly one control, no request API, `controls` pointing at
/// `ctrl`.
fn build_ext_controls(ctrl: &mut V4l2ExtControl) -> V4l2ExtControls {
    V4l2ExtControls {
        which: 0x0098_0000,
        count: 1,
        error_idx: 0,
        request_fd: -1,
        reserved: 0,
        controls: ctrl as *mut V4l2ExtControl as usize as u32,
    }
}

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
        reserved: [0; 2],
        value_or_ptr: 0,
    };
    let mut ctrls = build_ext_controls(&mut enable);
    ext_ctrls(fd, VIDIOC_S_EXT_CTRLS, &mut ctrls, "enable")?;

    // Mirrors VtHistogram::setPresetMode.
    let mut preset_ctrl = V4l2ExtControl {
        id: CTRL_HISTOGRAM_PRESET,
        size: 0,
        reserved: [0; 2],
        value_or_ptr: preset as u32,
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
            reserved: [0; 2],
            value_or_ptr: buffer.as_mut_ptr() as usize as u32,
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
    use crate::capture::histogram_types::ZONES_PER_PRESET;
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
    fn direct_ext_ctrl_struct_sizes_match_32bit_arm_abi() {
        assert_eq!(std::mem::size_of::<V4l2ExtControl>(), 20);
        assert_eq!(std::mem::size_of::<V4l2ExtControls>(), 24);
    }

    #[test]
    fn build_ext_controls_wraps_single_user_class_control() {
        let mut ctrl = V4l2ExtControl {
            id: CTRL_HISTOGRAM_DATA,
            size: (ZONES_PER_PRESET[0] * ENTRY_SIZE) as u32,
            reserved: [0; 2],
            value_or_ptr: 0,
        };
        // The payload control requests all 16 preset-0 zones in one ioctl.
        assert_eq!(ctrl.size, 0x6040);
        let ctrls = build_ext_controls(&mut ctrl);
        assert_eq!(ctrls.which, 0x0098_0000);
        assert_eq!(ctrls.count, 1);
        assert_eq!(ctrls.error_idx, 0);
        assert_eq!(ctrls.request_fd, -1);
        assert_eq!(ctrls.reserved, 0);
        // On the 32-bit ARM TV this is full pointer identity; on a 64-bit host
        // the fixed-width `controls` field carries the low 32 bits.
        assert_eq!(
            ctrls.controls as usize,
            std::ptr::addr_of_mut!(ctrl) as usize & u32::MAX as usize,
            "wrapper must carry the control's 32-bit ARM address"
        );
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
