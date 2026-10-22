// -----------------------------------------------------------------------------
// webOS 25 Histogram Capture Backend (direct V4L2 ioctls on /dev/video60)
// -----------------------------------------------------------------------------

use anyhow::{Context, Result, anyhow, ensure};
use std::ffi::c_void;
use tracing::{error, info, warn};

use super::histogram_probe::{u8_weighted_mean, u16_weighted_mean};
use super::histogram_types::{
    CTRL_HISTOGRAM_DATA, CTRL_HISTOGRAM_ENABLE, CTRL_HISTOGRAM_PRESET, ENTRY_SIZE,
    HISTOGRAM_V4L2_DEVICE, HistogramBuffer, V4l2ExtControl, V4l2ExtControls, VIDIOC_G_EXT_CTRLS,
    VIDIOC_S_EXT_CTRLS, VtHistogramEntry, ZONE_RECTS, ZONES_PER_PRESET, build_ext_controls,
};
use super::{CaptureError, CapturedFrame, NativeCaptureError, ScreenCapture};

/// Interpretation of each 512-byte channel block in a histogram entry.
///
/// Settled on-device by the C5 probe campaign (analysis/probe-results.md):
/// each block is 256 little-endian `u16` counts, bin index = level 0-255;
/// the u8 reading is a misparse artifact of the same bytes. `U8x512` remains
/// supported as the documented failure mode — `zone_to_rgb` stays total over
/// the enum and the u8-vs-u16 tests keep locking the distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinLayout {
    /// 512 `u8` bins per channel block. Refuted on-device; constructed only
    /// by the tests documenting the misparse.
    #[allow(dead_code)]
    U8x512,
    /// 256 little-endian `u16` bins per channel block.
    U16x256,
}

/// Bin layout confirmed on-device (analysis/probe-results.md): 256
/// little-endian u16 counts per 512-byte channel block.
pub const BIN_LAYOUT: BinLayout = BinLayout::U16x256;

/// Channel-block index to (R, G, B) mapping, confirmed on-device against a
/// known dark-orange screen (u16 peaks 240/70/21 → RGB(240, 70, 21)):
/// ch0=R, ch1=G, ch2=B.
pub const CHANNEL_ORDER: (usize, usize, usize) = (0, 1, 2);

/// Converts one histogram zone into an 8-bit (R, G, B) colour from each
/// channel block's count-weighted mean bin level, saturating at 255.
/// `order` maps channel blocks to (R, G, B).
///
/// The statistic is the *level* a zone's energy sits at —
/// `sum(i·count[i]) / sum(count[i])`, 0.0 for an all-zero block, with the
/// one shared implementation in `histogram_probe` — not a sum of counts: a
/// sum measures how many pixels reported, which grows with scene brightness
/// only incidentally and says nothing about where in the 0..bins range the
/// counts landed. The mean level maps to 0-255 per layout: `U8x512` levels
/// span 0..=511 and scale by 255/511 (dividing by the top level before
/// scaling keeps the endpoints exact), `U16x256` levels already span
/// 0..=255 and pass through unscaled. Example for `U8x512`: ten counts in
/// bin 5 and five in bin 100 give a mean of (5·10 + 100·5)/15 = 36.67,
/// rendered as 36.67·(255/511) = 18.3 → 18.
pub fn zone_to_rgb(
    entry: &VtHistogramEntry,
    layout: BinLayout,
    order: (usize, usize, usize),
) -> (u8, u8, u8) {
    let blocks = (
        &entry.channels[0..512],
        &entry.channels[512..1024],
        &entry.channels[1024..1536],
    );
    let levels = match layout {
        BinLayout::U8x512 => [
            u8_weighted_mean(blocks.0),
            u8_weighted_mean(blocks.1),
            u8_weighted_mean(blocks.2),
        ],
        BinLayout::U16x256 => [
            u16_weighted_mean(blocks.0),
            u16_weighted_mean(blocks.1),
            u16_weighted_mean(blocks.2),
        ],
    };
    let channels = levels.map(|level| match layout {
        BinLayout::U8x512 => (level / 511.0 * 255.0).min(255.0) as u8,
        BinLayout::U16x256 => level.min(255.0) as u8,
    });
    (channels[order.0], channels[order.1], channels[order.2])
}

// -----------------------------------------------------------------------------
// Capture seam (unsafe confined to the direct V4L2 implementation)
// -----------------------------------------------------------------------------

/// Largest errno the kernel reports to userspace (Linux `MAX_ERRNO`); bounds
/// the sentinel pointer window.
const MAX_ERRNO: i32 = 4095;

/// Encodes a failed device open as a distinguishable [`HistogramLib::init`]
/// result: the returned pointer's value is `errno + 1`. The trait's `init`
/// has no error channel and NULL already means generic failure, so the open
/// errno — EBUSY in particular, which must surface as `device_busy` — rides
/// in the handle itself. The value is never dereferenced, only decoded by
/// [`errno_from_sentinel`].
fn errno_sentinel(errno: i32) -> *mut c_void {
    debug_assert!(
        (1..=MAX_ERRNO).contains(&errno),
        "sentinel errnos are real kernel errnos"
    );
    (errno as usize + 1) as *mut c_void
}

/// Decodes an [`errno_sentinel`] failure: `Some(errno)` for encoded open
/// failures, `None` for NULL, real session handles (`Box::into_raw` results,
/// never inside the tiny window below a page), and the test fake's dangling
/// address-1 token.
fn errno_from_sentinel(handle: *mut c_void) -> Option<i32> {
    let value = handle as usize;
    if (2..=MAX_ERRNO as usize + 1).contains(&value) {
        Some((value - 1) as i32)
    } else {
        None
    }
}

/// Maps a V4L2 ioctl errno onto the native result-code taxonomy the error
/// handling already speaks — the mapping the C5 probe campaign recorded
/// (analysis/probe-results.md): `EAGAIN`→11 (frame not ready yet),
/// `EACCES`→17 (access conflict), `ENODATA`→13 (no data yet), anything
/// else→1 (undocumented transient). Codes 2 and 3 stay reserved for the
/// seam's own null-handle / null-buffer results.
fn map_errno_to_native_code(errno: i32) -> i32 {
    match errno {
        libc::EAGAIN => 11,
        libc::EACCES => 17,
        libc::ENODATA => 13,
        _ => 1,
    }
}

/// Injectable device seam so host tests can drive the backend without a TV.
///
/// `pub` for visibility-lint coherence within this binary crate; nothing in a
/// bin target is exported, so the seam never leaves the crate.
pub trait HistogramLib {
    /// Opens and arms a histogram session: open `/dev/video60`, then the
    /// enable control (combining the library's ctor and start). Returns the
    /// opaque session handle; NULL on a failed arm, or an [`errno_sentinel`]
    /// when the open itself failed (EBUSY is decoded into `device_busy` by
    /// the construction path).
    fn init(&mut self) -> *mut c_void;
    /// Applies a preset (0..3); native result code.
    fn set_preset(&mut self, handle: *mut c_void, preset: i32) -> i32;
    /// Requests one histogram dump; 0 = ok, 2 = null handle/not active, 3 = null buffer.
    fn get(&mut self, handle: *mut c_void, buf: *mut VtHistogramEntry) -> i32;
    /// Tears the session down; native result code.
    fn finalize(&mut self, handle: *mut c_void) -> i32;
}

/// Per-session state carried in the opaque [`HistogramLib`] handle: the open
/// device fd plus the zone count the applied preset selected (it sizes the
/// `G_EXT_CTRLS` payload). Lives entirely in the handle, so the seam itself
/// stays stateless and `finalize` closes exactly what `init` opened.
struct V4l2Session {
    fd: libc::c_int,
    zones: u32,
}

/// Real seam: direct V4L2 ext-controls against `/dev/video60`, bypassing
/// libvtcapture — whose init is caller-cmdline-whitelisted and whose Luna
/// resource manager aborts outside a real LS2 service context — while the
/// kernel serves the controls to a plain root process, confirmed live
/// including during DRM playback (analysis/probe-results.md).
pub struct V4l2HistogramLib;

impl V4l2HistogramLib {
    /// Builds the stateless seam. Infallible: the node is opened per `init`.
    fn new() -> Self {
        Self
    }
}

/// Reclaims the session state from a live handle, or `None` for NULL and
/// failure sentinels (which must never reach the ioctl helpers).
///
/// # Safety
/// `handle` must be a live handle returned by [`HistogramLib::init`] —
/// non-null, non-sentinel — owned by exactly one capture; the `&mut self`
/// receiver on every trait method guarantees that exclusivity.
unsafe fn session(handle: *mut c_void) -> Option<&'static mut V4l2Session> {
    if handle.is_null() || errno_from_sentinel(handle).is_some() {
        return None;
    }
    // SAFETY: guaranteed by the caller contract above.
    Some(unsafe { &mut *(handle as *mut V4l2Session) })
}

/// Opens [`HISTOGRAM_V4L2_DEVICE`] `O_RDWR|O_CLOEXEC`; `Err(errno)` on failure.
fn open_histogram_device() -> Result<libc::c_int, i32> {
    let path =
        std::ffi::CString::new(HISTOGRAM_V4L2_DEVICE).expect("device path is a NUL-free constant");
    // SAFETY: `path` outlives the call and open() only reads it.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        // Never 0: errno_sentinel(0) would encode as a live-looking handle.
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    Ok(fd)
}

/// Runs one `VIDIOC_S_EXT_CTRLS` with the single given control.
fn set_ctrl(fd: libc::c_int, ctrl: &mut V4l2ExtControl) -> Result<(), i32> {
    let mut ctrls = build_ext_controls(ctrl);
    // SAFETY: `ctrls.controls` targets the live `ctrl` for the call's
    // duration; the kernel only writes back `error_idx`.
    if unsafe { libc::ioctl(fd, VIDIOC_S_EXT_CTRLS, &mut ctrls as *mut V4l2ExtControls) } < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(())
}

/// Runs one `VIDIOC_G_EXT_CTRLS` reading the `zones`-entry histogram payload
/// into `buf`.
fn get_histogram_data(
    fd: libc::c_int,
    zones: usize,
    buf: *mut VtHistogramEntry,
) -> Result<(), i32> {
    let mut data = V4l2ExtControl {
        id: CTRL_HISTOGRAM_DATA,
        size: (zones * ENTRY_SIZE) as u32,
        reserved2: 0,
        // 32-bit ARM target: the pointer fits losslessly; on a 64-bit host
        // this path is unreachable (the open in init fails first).
        value_or_ptr: buf as usize as u32,
        reserved3: 0,
    };
    let mut ctrls = build_ext_controls(&mut data);
    // SAFETY: `ctrls.controls` targets the live payload control; the kernel
    // writes the histogram into `buf` and `error_idx` only.
    if unsafe { libc::ioctl(fd, VIDIOC_G_EXT_CTRLS, &mut ctrls as *mut V4l2ExtControls) } < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(())
}

impl HistogramLib for V4l2HistogramLib {
    fn init(&mut self) -> *mut c_void {
        let fd = match open_histogram_device() {
            Ok(fd) => fd,
            Err(errno) => return errno_sentinel(errno),
        };
        // Mirrors VtHistogram::start: value 0 enables the histogram engine.
        // This single control combines the library's ctor and start.
        let mut enable = V4l2ExtControl {
            id: CTRL_HISTOGRAM_ENABLE,
            size: 0,
            reserved2: 0,
            value_or_ptr: 0,
            reserved3: 0,
        };
        if let Err(errno) = set_ctrl(fd, &mut enable) {
            warn!(
                "histogram enable control failed (errno {errno}); closing {}",
                HISTOGRAM_V4L2_DEVICE
            );
            // SAFETY: `fd` is the descriptor just opened above.
            unsafe { libc::close(fd) };
            return std::ptr::null_mut();
        }
        // Preset-0's zone count is the safe default until set_preset runs;
        // the construction flow always calls it immediately after init.
        Box::into_raw(Box::new(V4l2Session {
            fd,
            zones: ZONES_PER_PRESET[0] as u32,
        })) as *mut c_void
    }

    fn set_preset(&mut self, handle: *mut c_void, preset: i32) -> i32 {
        // SAFETY: `handle` originates from init() and outlives the call.
        let Some(session) = (unsafe { session(handle) }) else {
            return 2; // null handle / not active, native taxonomy
        };
        if !(0..ZONES_PER_PRESET.len() as i32).contains(&preset) {
            return 3; // bad preset, native taxonomy
        }
        let mut ctrl = V4l2ExtControl {
            id: CTRL_HISTOGRAM_PRESET,
            size: 0,
            reserved2: 0,
            value_or_ptr: preset as u32,
            reserved3: 0,
        };
        match set_ctrl(session.fd, &mut ctrl) {
            Ok(()) => {
                session.zones = ZONES_PER_PRESET[preset as usize] as u32;
                0
            }
            Err(errno) => map_errno_to_native_code(errno),
        }
    }

    fn get(&mut self, handle: *mut c_void, buf: *mut VtHistogramEntry) -> i32 {
        // SAFETY: `handle` originates from init() and outlives the call.
        let Some(session) = (unsafe { session(handle) }) else {
            return 2; // null handle / not active, native taxonomy
        };
        if buf.is_null() {
            return 3; // null buffer, native taxonomy
        }
        match get_histogram_data(session.fd, session.zones as usize, buf) {
            Ok(()) => 0,
            Err(errno) => map_errno_to_native_code(errno),
        }
    }

    fn finalize(&mut self, handle: *mut c_void) -> i32 {
        if handle.is_null() {
            return 0; // idempotent: nothing was ever armed
        }
        // SAFETY: a non-null, non-sentinel handle is the `Box::into_raw`
        // pointer from init(); the caller nulls its stored handle first, so
        // each session is reclaimed exactly once.
        let session = unsafe { Box::from_raw(handle as *mut V4l2Session) };
        // SAFETY: `session.fd` is the descriptor opened in init().
        if unsafe { libc::close(session.fd) } < 0 {
            return std::io::Error::last_os_error().raw_os_error().unwrap_or(1);
        }
        0
    }
}

// -----------------------------------------------------------------------------
// Backend: screen capture over the histogram device
// -----------------------------------------------------------------------------

/// Maps a failing preset result code to a message.
fn preset_error_context(rc: i32) -> &'static str {
    match rc {
        2 => "histogram mode not activated",
        3 => "bad preset",
        _ => "histogram set_preset failed",
    }
}

/// Production capture backend rendering the reverse-engineered webOS
/// 25 histogram device into a BGRA canvas laid out by [`ZONE_RECTS`].
///
/// Generic over the device seam; [`HistogramCapture`] is the production
/// alias bound to [`V4l2HistogramLib`].
pub struct HistogramCaptureImpl<L: HistogramLib> {
    lib: L,
    /// Live session handle; NULL whenever the session is not armed.
    handle: *mut c_void,
    width: u32,
    height: u32,
    buffer: HistogramBuffer,
    canvas: Vec<u8>,
    /// Zone painted at each canvas pixel, from [`zone_owner_map`].
    owners: Vec<u8>,
}

/// For every pixel of a `width`x`height` canvas, the [`ZONE_RECTS`] zone it
/// shows: the smallest rect covering the pixel centre (the most local
/// statistic), ties going to the nearer rect centre. Zones 4 and 8 tile the
/// screen, so every pixel has an owner.
fn zone_owner_map(width: u32, height: u32) -> Vec<u8> {
    let mut owners = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        let py = (y as f32 + 0.5) / height as f32;
        for x in 0..width {
            let px = (x as f32 + 0.5) / width as f32;
            let score = |&(x0, y0, x1, y1): &(f32, f32, f32, f32)| {
                let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                ((x1 - x0) * (y1 - y0), (px - cx).powi(2) + (py - cy).powi(2))
            };
            let owner = (0..ZONE_RECTS.len())
                .filter(|&zone| {
                    let (x0, y0, x1, y1) = ZONE_RECTS[zone];
                    (x0..x1).contains(&px) && (y0..y1).contains(&py)
                })
                .min_by(|&a, &b| {
                    score(&ZONE_RECTS[a])
                        .partial_cmp(&score(&ZONE_RECTS[b]))
                        .unwrap()
                })
                .expect("zones 4 and 8 tile the screen");
            owners.push(owner as u8);
        }
    }
    owners
}

impl<L: HistogramLib> HistogramCaptureImpl<L> {
    /// Arms a session over an already-open seam: init + set_preset.
    fn with_lib(mut lib: L, width: u32, height: u32, preset: i32) -> Result<Self> {
        // ponytail: only preset 0's geometry is wired; add the other
        // presets' rects from the app's zone table if they are ever used.
        ensure!(
            preset == 0,
            "bad preset {preset}: only preset 0 (video) has a known zone geometry"
        );
        let handle = lib.init();
        if handle.is_null() {
            return Err(anyhow::Error::new(NativeCaptureError {
                backend: "histogram",
                operation: "init",
                code: 0,
            })
            .context("histogram capture init failed"));
        }
        // A failure sentinel means the open itself failed; EBUSY gets its own
        // distinguishable error because the fix is operator action (freeing
        // the node), not a restart.
        if let Some(errno) = errno_from_sentinel(handle) {
            if errno == libc::EBUSY {
                error!(
                    "another process holds {HISTOGRAM_V4L2_DEVICE} (e.g. the official Hue service) — stop it or declare Conflicts in the service unit"
                );
                return Err(anyhow::Error::new(NativeCaptureError {
                    backend: "histogram",
                    operation: "device_busy",
                    code: errno,
                }));
            }
            return Err(anyhow::Error::new(NativeCaptureError {
                backend: "histogram",
                operation: "init",
                code: errno,
            })
            .context("histogram device open failed"));
        }
        let rc = lib.set_preset(handle, preset);
        if rc != 0 {
            // Self is never built, so Drop won't close the session: release
            // it here or the single-open device stays busy until restart.
            lib.finalize(handle);
            return Err(anyhow::Error::new(NativeCaptureError {
                backend: "histogram",
                operation: "set_preset",
                code: rc,
            })
            .context(preset_error_context(rc)));
        }
        let zones = ZONES_PER_PRESET[preset as usize];
        info!(
            "[+] histogram capture session armed: preset {preset}, {zones} zones, canvas {width}x{height}"
        );
        Ok(Self {
            lib,
            handle,
            width,
            height,
            buffer: HistogramBuffer::new(zones),
            canvas: vec![0; width as usize * height as usize * 4],
            owners: zone_owner_map(width, height),
        })
    }

    /// Renders one histogram buffer into the BGRA zone canvas.
    fn render(&mut self) {
        let colors: Vec<(u8, u8, u8)> = (0..self.buffer.zones())
            .map(|zone| zone_to_rgb(self.buffer.entry(zone), BIN_LAYOUT, CHANNEL_ORDER))
            .collect();
        for (pixel, &owner) in self
            .canvas
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(&self.owners)
        {
            let (r, g, b) = colors[owner as usize];
            *pixel = [b, g, r, 255];
        }
    }
}

/// Production constructor over the direct V4L2 seam.
impl HistogramCaptureImpl<V4l2HistogramLib> {
    /// Opens the histogram device and arms a session at `preset` (0..3).
    pub fn try_new(width: u32, height: u32, preset: i32) -> Result<Self> {
        let lib = V4l2HistogramLib::new();
        Self::with_lib(lib, width, height, preset).context("histogram capture backend unavailable")
    }
}

impl<L: HistogramLib> ScreenCapture for HistogramCaptureImpl<L> {
    fn acquire_frame(&mut self) -> std::result::Result<CapturedFrame<'_>, CaptureError> {
        self.buffer.prefill_indices();
        let rc = self.lib.get(self.handle, self.buffer.as_mut_ptr());
        if rc == 3 {
            return Err(CaptureError::InvalidFrame(anyhow!(
                "histogram get returned code 3 (null buffer)"
            )));
        }
        if rc != 0 {
            // rc 2 is "null handle / not active"; the rest are the native
            // codes the seam maps errnos onto (11 not ready, 13 no data,
            // 17 access conflict, 1 undocumented) — transient outages all,
            // never fabricated data.
            return Err(CaptureError::Unavailable(NativeCaptureError {
                backend: "histogram",
                operation: "get_histogram",
                code: rc,
            }));
        }
        // A shuffled or foreign write is a transient driver fault, not a corrupt
        // frame: Unavailable lets CaptureRecovery retry instead of pausing.
        if let Err(error) = self.buffer.validate_indices() {
            warn!("histogram zone validation failed: {error:#}");
            return Err(CaptureError::Unavailable(NativeCaptureError {
                backend: "histogram",
                operation: "zone_index_mismatch",
                code: 13,
            }));
        }
        if self.buffer.is_wholly_absent() {
            return Err(CaptureError::Unavailable(NativeCaptureError {
                backend: "histogram",
                operation: "empty_histogram",
                code: 2,
            }));
        }
        self.render();
        Ok(CapturedFrame {
            data: self.canvas.as_slice(),
            width: self.width,
            height: self.height,
            is_bgra: true,
        })
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn active_kind(&self) -> &'static str {
        "histogram"
    }

    fn is_real_hardware(&self) -> bool {
        true
    }
}

impl<L: HistogramLib> Drop for HistogramCaptureImpl<L> {
    fn drop(&mut self) {
        // Clear the handle before the call so a session is never finalized twice.
        let handle = self.handle;
        self.handle = std::ptr::null_mut();
        if handle.is_null() {
            return;
        }
        let rc = self.lib.finalize(handle);
        if rc != 0 {
            warn!("histogram finalize returned code {rc} during drop");
        }
    }
}

/// Production alias consumed by the runtime wiring.
pub type HistogramCapture = HistogramCaptureImpl<V4l2HistogramLib>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ptr::null_mut;
    use std::rc::Rc;

    const TEST_WIDTH: u32 = 320;
    const TEST_HEIGHT: u32 = 180;
    const TEST_ZONES: usize = ZONES_PER_PRESET[0];

    /// Programmable seam: records call order, returns canned codes, and fills
    /// the returned buffer through an injectable closure. Never touches real FFI.
    struct FakeHistogramLib {
        calls: Rc<RefCell<Vec<&'static str>>>,
        /// Token handle returned by `init` when the sequence is exhausted; NULL
        /// simulates init failure, a `errno_sentinel` value an open failure.
        handle: *mut c_void,
        /// Per-call `init` results, consumed front to back.
        init_handles: VecDeque<*mut c_void>,
        preset_rc: i32,
        get_rc: i32,
        /// Per-call `get` result codes, consumed front to back; exhausted
        /// scripts fall back to `get_rc`.
        get_rcs: VecDeque<i32>,
        /// Buffer fills for successive successful `get` calls, consumed front
        /// to back; exhausted scripts fall back to `fill`.
        fills: VecDeque<Box<dyn FnMut(&mut VtHistogramEntry)>>,
        zones: usize,
        fill: Option<Box<dyn FnMut(&mut VtHistogramEntry)>>,
    }

    impl FakeHistogramLib {
        fn new() -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                // Non-null, never dereferenced token handle.
                handle: std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
                init_handles: VecDeque::new(),
                preset_rc: 0,
                get_rc: 0,
                get_rcs: VecDeque::new(),
                fills: VecDeque::new(),
                zones: TEST_ZONES,
                fill: None,
            }
        }

        fn calls(&self) -> Rc<RefCell<Vec<&'static str>>> {
            self.calls.clone()
        }

        /// Handles returned by successive `init` calls; exhausted sequences
        /// fall back to the default token handle.
        fn with_init_handles(mut self, handles: Vec<*mut c_void>) -> Self {
            self.init_handles = handles.into();
            self
        }

        fn with_preset_rc(mut self, rc: i32) -> Self {
            self.preset_rc = rc;
            self
        }

        fn with_get_rc(mut self, rc: i32) -> Self {
            self.get_rc = rc;
            self
        }

        /// Result codes for successive `get` calls; exhausted scripts fall
        /// back to `get_rc`.
        fn with_get_rcs(mut self, rcs: Vec<i32>) -> Self {
            self.get_rcs = rcs.into();
            self
        }

        /// Buffer fills for successive successful `get` calls; exhausted
        /// scripts fall back to `fill`.
        fn with_fills(mut self, fills: Vec<Box<dyn FnMut(&mut VtHistogramEntry)>>) -> Self {
            self.fills = fills.into();
            self
        }

        fn with_fill(mut self, fill: impl FnMut(&mut VtHistogramEntry) + 'static) -> Self {
            self.fill = Some(Box::new(fill));
            self
        }
    }

    impl HistogramLib for FakeHistogramLib {
        fn init(&mut self) -> *mut c_void {
            self.calls.borrow_mut().push("init");
            if let Some(handle) = self.init_handles.pop_front() {
                handle
            } else {
                self.handle
            }
        }

        fn set_preset(&mut self, _handle: *mut c_void, _preset: i32) -> i32 {
            self.calls.borrow_mut().push("set_preset");
            self.preset_rc
        }

        fn get(&mut self, _handle: *mut c_void, buf: *mut VtHistogramEntry) -> i32 {
            self.calls.borrow_mut().push("get");
            let rc = self.get_rcs.pop_front().unwrap_or(self.get_rc);
            if rc == 0 {
                let mut scripted = self.fills.pop_front();
                if let Some(fill) = scripted.as_mut() {
                    run_fill(fill, buf, self.zones);
                } else if let Some(fill) = self.fill.as_mut() {
                    run_fill(fill, buf, self.zones);
                }
            }
            rc
        }

        fn finalize(&mut self, _handle: *mut c_void) -> i32 {
            self.calls.borrow_mut().push("finalize");
            0
        }
    }

    /// Runs one scripted fill against the capture's buffer, mirroring a
    /// driver that writes `zones` entries.
    fn run_fill(
        fill: &mut dyn FnMut(&mut VtHistogramEntry),
        buf: *mut VtHistogramEntry,
        zones: usize,
    ) {
        // SAFETY: `buf` points at the capture's HistogramBuffer, allocated
        // with exactly `zones` entries and valid for the duration of this
        // single-threaded test call.
        let entries = unsafe { std::slice::from_raw_parts_mut(buf, zones) };
        for entry in entries {
            fill(entry);
        }
    }

    fn capture(lib: FakeHistogramLib) -> HistogramCaptureImpl<FakeHistogramLib> {
        HistogramCaptureImpl::with_lib(lib, TEST_WIDTH, TEST_HEIGHT, 0)
            .expect("fake capture construction")
    }

    /// `expect_err` without requiring `Debug` on the success type.
    fn frame_error(capture: &mut HistogramCaptureImpl<FakeHistogramLib>) -> CaptureError {
        match capture.acquire_frame() {
            Ok(_) => panic!("acquire_frame must fail"),
            Err(error) => error,
        }
    }

    /// `expect_err` without requiring `Debug` on the constructed capture.
    fn with_lib_error(lib: FakeHistogramLib, preset: i32) -> anyhow::Error {
        match HistogramCaptureImpl::with_lib(lib, TEST_WIDTH, TEST_HEIGHT, preset) {
            Ok(_) => panic!("with_lib must fail"),
            Err(error) => error,
        }
    }

    // 1. Healthy path.
    #[test]
    fn healthy_path_returns_exact_size_bgra_canvas() {
        let lib = FakeHistogramLib::new().with_fill(|entry| entry.channels[0] = 1);
        let calls = lib.calls();
        let mut capture = capture(lib);

        let frame = capture.acquire_frame().expect("healthy frame");
        assert_eq!(frame.data.len(), (TEST_WIDTH * TEST_HEIGHT * 4) as usize);
        assert!(frame.is_bgra);
        assert_eq!(frame.width, TEST_WIDTH);
        assert_eq!(frame.height, TEST_HEIGHT);
        assert_eq!(*calls.borrow(), ["init", "set_preset", "get"]);
    }

    // 2. Constants finalized from the C5 probe campaign.
    #[test]
    fn constants_are_finalized_from_probe_evidence() {
        assert_eq!(
            BIN_LAYOUT,
            BinLayout::U16x256,
            "probe settled u8-vs-u16 (analysis/probe-results.md): u16 little-endian"
        );
        assert_eq!(
            CHANNEL_ORDER,
            (0, 1, 2),
            "probe settled the channel order: ch0=R, ch1=G, ch2=B"
        );
    }

    // 3. init EBUSY (another process holds /dev/video60) must surface as the
    //    distinguishable device_busy error, not a generic init failure.
    #[test]
    fn init_ebusy_maps_to_device_busy() {
        let lib = FakeHistogramLib::new().with_init_handles(vec![errno_sentinel(libc::EBUSY)]);
        let error = with_lib_error(lib, 0);
        let native = error
            .downcast_ref::<NativeCaptureError>()
            .expect("device_busy must carry a NativeCaptureError");
        assert_eq!(native.backend, "histogram");
        assert_eq!(native.operation, "device_busy");
        assert_eq!(native.code, 16, "code is the EBUSY errno");
        assert!(native.is_busy(), "EBUSY must read as busy to callers");
    }

    // 4. Other open failures keep the generic init operation but carry the errno.
    #[test]
    fn init_open_failure_other_than_ebusy_reports_init_with_errno() {
        let lib = FakeHistogramLib::new().with_init_handles(vec![errno_sentinel(libc::ENOENT)]);
        let error = with_lib_error(lib, 0);
        let native = error
            .downcast_ref::<NativeCaptureError>()
            .expect("open failure must carry a NativeCaptureError");
        assert_eq!(native.operation, "init");
        assert_eq!(native.code, libc::ENOENT);
    }

    // 5. Sentinel encoding: round-trips errnos, never aliases real handles.
    #[test]
    fn errno_sentinels_round_trip_and_never_alias_real_handles() {
        for errno in [
            libc::EACCES,
            libc::EAGAIN,
            libc::EBUSY,
            libc::ENODATA,
            libc::EIO,
        ] {
            assert_eq!(
                errno_from_sentinel(errno_sentinel(errno)),
                Some(errno),
                "sentinel for errno {errno} must round-trip"
            );
        }
        assert_eq!(
            errno_from_sentinel(null_mut()),
            None,
            "NULL is the generic init failure, not a sentinel"
        );
        let token = std::ptr::NonNull::<u8>::dangling().as_ptr().cast();
        assert_eq!(
            errno_from_sentinel(token),
            None,
            "the fake's dangling token handle is not a sentinel"
        );
        let boxed = Box::into_raw(Box::new(7i32)) as *mut c_void;
        assert_eq!(
            errno_from_sentinel(boxed),
            None,
            "a real heap handle never sits in the tiny sentinel window"
        );
        // SAFETY: reintegrates the allocation boxed above, freeing it once.
        drop(unsafe { Box::from_raw(boxed as *mut i32) });
    }

    // 6. errno → native result-code mapping recorded by the probe campaign.
    #[test]
    fn errno_mapping_matches_the_probe_campaign() {
        assert_eq!(map_errno_to_native_code(libc::EAGAIN), 11, "not ready");
        assert_eq!(
            map_errno_to_native_code(libc::EACCES),
            17,
            "access conflict"
        );
        assert_eq!(map_errno_to_native_code(libc::ENODATA), 13, "no data yet");
        assert_eq!(
            map_errno_to_native_code(libc::EIO),
            1,
            "undocumented transient"
        );
        assert_eq!(map_errno_to_native_code(libc::EINVAL), 1);
    }

    // 7. get returns rc 2 -> Unavailable("get_histogram").
    #[test]
    fn get_rc_two_reports_unavailable_get_histogram() {
        let lib = FakeHistogramLib::new().with_get_rc(2);
        let mut capture = capture(lib);

        let error = frame_error(&mut capture);
        match error {
            CaptureError::Unavailable(native) => {
                assert_eq!(native.backend, "histogram");
                assert_eq!(native.operation, "get_histogram");
                assert_eq!(native.code, 2);
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // 8. Wholly-absent buffer -> Unavailable("empty_histogram"), never a black frame.
    #[test]
    fn wholly_absent_histogram_is_an_error_not_a_black_canvas() {
        let lib = FakeHistogramLib::new(); // no fill: the driver produced nothing
        let mut capture = capture(lib);

        let error = frame_error(&mut capture);
        match error {
            CaptureError::Unavailable(native) => {
                assert_eq!(native.backend, "histogram");
                assert_eq!(native.operation, "empty_histogram");
                assert_eq!(native.code, 2);
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // 9. Mismatched index in the returned buffer -> transient Unavailable.
    #[test]
    fn mismatched_zone_index_is_unavailable() {
        let lib = FakeHistogramLib::new().with_fill(|entry| entry.index = 99);
        let mut capture = capture(lib);

        match frame_error(&mut capture) {
            CaptureError::Unavailable(native) => {
                assert_eq!(native.operation, "zone_index_mismatch")
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // 10. Canvas geometry follows the official app's overlapping zone rects,
    // not a grid: each probe point shows the smallest zone covering it.
    #[test]
    fn canvas_geometry_follows_official_zone_rects() {
        // Zone z reports one count at R level 10*z + 5, so a pixel's red
        // value names the zone painted there.
        let lib = FakeHistogramLib::new().with_fill(|entry| {
            let level = 10 * entry.index as usize + 5;
            entry.channels[2 * level] = 1;
        });
        let mut capture = capture(lib);
        let frame = capture.acquire_frame().expect("geometry frame");

        let (w, h) = (TEST_WIDTH as f32, TEST_HEIGHT as f32);
        let zone_at = |fx: f32, fy: f32| -> u8 {
            let (x, y) = ((fx * w) as usize, (fy * h) as usize);
            let red = frame.data[(y * TEST_WIDTH as usize + x) * 4 + 2];
            (red - 5) / 10
        };
        for (fx, fy, zone) in [
            (0.02, 0.02, 7),  // top-left
            (0.98, 0.02, 13), // top-right
            (0.02, 0.98, 5),  // bottom-left
            (0.98, 0.98, 9),  // bottom-right
            (0.5, 0.5, 2),    // centre
            (0.5, 0.95, 1),   // bottom centre
            (0.5, 0.05, 14),  // top band
            (0.5, 0.15, 11),  // top centre, below the band
            (0.02, 0.5, 15),  // left band
            (0.17, 0.5, 6),   // left middle, inside the band
            (0.9, 0.5, 10),   // right middle
            (0.27, 0.5, 4),   // left half, nearer its centre than zone 0's
            (0.73, 0.5, 8),   // right half
        ] {
            assert_eq!(zone_at(fx, fy), zone, "zone at ({fx}, {fy})");
        }
    }

    // 11. zone_to_rgb: count-weighted mean level, scaling, and saturation.
    #[test]
    fn zone_to_rgb_uses_weighted_mean_level() {
        let mut entry = VtHistogramEntry {
            index: 0,
            channels: [0; 1536],
        };

        // U8x512 review example: ten counts at bin 5 and five at bin 100
        // -> mean (5·10 + 100·5)/15 = 36.67 -> 36.67·255/511 = 18.3 -> 18.
        entry.channels[5] = 10;
        entry.channels[100] = 5;
        assert_eq!(
            zone_to_rgb(&entry, BinLayout::U8x512, CHANNEL_ORDER),
            (18, 0, 0)
        );

        // Blocks with zero counts carry no level: the untouched channels
        // stay at 0. Channel order (2, 1, 0) turns the red zone blue.
        assert_eq!(
            zone_to_rgb(&entry, BinLayout::U8x512, (2, 1, 0)),
            (0, 0, 18)
        );

        // U16x256 means already span 0..=255 and pass through unscaled:
        // bin 100 ×10 and bin 200 ×5 -> mean (100·10 + 200·5)/15 = 133.33.
        let mut u16_entry = VtHistogramEntry {
            index: 0,
            channels: [0; 1536],
        };
        u16_entry.channels[200..202].copy_from_slice(&10u16.to_le_bytes());
        u16_entry.channels[400..402].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(
            zone_to_rgb(&u16_entry, BinLayout::U16x256, CHANNEL_ORDER),
            (133, 0, 0)
        );

        // Saturation: every count at the top bin puts the mean at the top
        // level (511), which scales to exactly 255.
        entry.channels[..512].fill(0);
        entry.channels[511] = 0xFF;
        assert_eq!(
            zone_to_rgb(&entry, BinLayout::U8x512, CHANNEL_ORDER),
            (255, 0, 0)
        );
    }

    // 12. A failed get (rc != 0) must leave no stale colours behind.
    #[test]
    fn failed_get_leaves_no_stale_colors_in_the_next_frame() {
        let lib = FakeHistogramLib::new()
            .with_get_rcs(vec![0, 2, 0, 0])
            .with_fills(vec![
                // Frame 1: a real payload with a high green level everywhere
                // (bin 88 of block 1), so stale bytes would actually show.
                Box::new(|entry: &mut VtHistogramEntry| entry.channels[600] = 0xFF),
                // The failed get in between writes nothing. The next success
                // carries zero-level data (all counts in bin 0): fully black
                // unless a byte of frame 1 survived in the reused buffer.
                Box::new(|entry: &mut VtHistogramEntry| entry.channels[0] = 1),
            ]);
        let mut capture = capture(lib);

        let first = capture.acquire_frame().expect("first frame");
        assert!(
            first.data.chunks_exact(4).any(|px| px[1] != 0),
            "frame 1 must carry colour for stale data to matter"
        );

        match frame_error(&mut capture) {
            CaptureError::Unavailable(native) => {
                assert_eq!(native.operation, "get_histogram");
                assert_eq!(native.code, 2);
            }
            other => panic!("expected Unavailable for rc 2, got {other:?}"),
        }

        let second = capture.acquire_frame().expect("second frame");
        assert_eq!(second.data.len(), (TEST_WIDTH * TEST_HEIGHT * 4) as usize);
        for (i, pixel) in second.data.chunks_exact(4).enumerate() {
            assert_eq!(pixel, [0, 0, 0, 255], "pixel {i} must stay opaque black");
        }

        // A later successful get that writes nothing finds a fully zeroed
        // buffer: wholly-absent fires instead of rendering leftover bytes.
        match frame_error(&mut capture) {
            CaptureError::Unavailable(native) => {
                assert_eq!(native.operation, "empty_histogram");
            }
            other => panic!("expected empty_histogram, got {other:?}"),
        }
    }

    // try_new error mapping (rc from set_preset).
    #[test]
    fn set_preset_failure_maps_to_native_error_context() {
        let lib = FakeHistogramLib::new().with_preset_rc(2);
        let calls = lib.calls();
        let error = with_lib_error(lib, 0);
        assert!(
            calls.borrow().contains(&"finalize"),
            "failed set_preset must release the session"
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("histogram mode not activated"),
            "rc 2 message: {message}"
        );
        assert!(
            message.contains("set_preset"),
            "operation in message: {message}"
        );

        let lib = FakeHistogramLib::new().with_preset_rc(3);
        let error = with_lib_error(lib, 0);
        let message = format!("{error:#}");
        assert!(message.contains("bad preset"), "rc 3 message: {message}");
    }

    #[test]
    fn out_of_range_preset_is_rejected_before_ffi() {
        let lib = FakeHistogramLib::new();
        let calls = lib.calls();
        let error = with_lib_error(lib, 4);
        let message = format!("{error:#}");
        assert!(message.contains("bad preset"), "message: {message}");
        assert_eq!(
            *calls.borrow(),
            [] as [&'static str; 0],
            "no FFI call may run for an out-of-range preset"
        );
    }
}
