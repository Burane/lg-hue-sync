// -----------------------------------------------------------------------------
// webOS 24/25 Histogram Capture Backend (reverse-engineered libvtcapture API)
// -----------------------------------------------------------------------------

use anyhow::{Context, Result, anyhow, ensure};
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

use super::histogram_probe::{REINIT, policy_trampoline, u8_weighted_mean, u16_weighted_mean};
use super::histogram_types::{
    HistogramBuffer, LIBVTCAPTURE_PATH, VtHistogramEntry, ZONE_GRID, ZONES_PER_PRESET,
};
use super::{CaptureError, CapturedFrame, NativeCaptureError, ScreenCapture};

/// Interpretation of each 512-byte channel block in a histogram entry.
///
/// Evidence (`analysis/findings.md` §3.2) does not settle u8-vs-u16 bin
/// semantics; both are supported and the choice is a compile-time constant
/// until the on-device probe finalizes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinLayout {
    /// 512 `u8` bins per channel block.
    U8x512,
    /// 256 little-endian `u16` bins per channel block; constructed only by the
    /// on-device probe once u8-vs-u16 semantics are settled (Task 7).
    #[allow(dead_code)]
    U16x256,
}

/// Bin layout used until the on-device probe settles u8 vs u16 (Task 7 finalizes).
pub const BIN_LAYOUT: BinLayout = BinLayout::U8x512;

/// Channel-block index to (R, G, B) mapping (Task 7 finalizes).
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
// FFI seam (unsafe confined to the dlopen implementation)
// -----------------------------------------------------------------------------

/// `bool (*)(void)` policy callback handed to `vtCapture_initHistogram`.
type HistogramPolicyCallback = unsafe extern "C" fn() -> bool;
type FnVtCaptureInitHistogram = unsafe extern "C" fn(HistogramPolicyCallback) -> *mut c_void;
type FnVtCaptureSetPresetHistogram = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type FnVtCaptureGetHistogram = unsafe extern "C" fn(*mut c_void, *mut VtHistogramEntry) -> i32;
type FnVtCaptureFinalizeHistogram = unsafe extern "C" fn(*mut c_void) -> i32;

/// Injectable library seam so host tests can drive the backend without a TV.
///
/// `pub` for visibility-lint coherence within this binary crate; nothing in a
/// bin target is exported, so the seam never leaves the crate.
pub trait HistogramLib {
    /// Arms a histogram session with the policy trampoline; NULL on failure.
    fn init(&mut self) -> *mut c_void;
    /// Applies a preset (0..3); native result code.
    fn set_preset(&mut self, handle: *mut c_void, preset: i32) -> i32;
    /// Requests one histogram dump; 0 = ok, 2 = null handle/not active, 3 = null buffer.
    fn get(&mut self, handle: *mut c_void, buf: *mut VtHistogramEntry) -> i32;
    /// Tears the session down; native result code.
    fn finalize(&mut self, handle: *mut c_void) -> i32;
}

/// Real seam: the dlopen'd `/usr/lib/libvtcapture.so.1` histogram API.
pub struct DlopenHistogramLib {
    fn_init_histogram: FnVtCaptureInitHistogram,
    fn_set_preset_histogram: FnVtCaptureSetPresetHistogram,
    fn_get_histogram: FnVtCaptureGetHistogram,
    fn_finalize_histogram: FnVtCaptureFinalizeHistogram,
    _lib: Library,
}

impl DlopenHistogramLib {
    /// Loads the histogram capture API, mirroring `HistogramProbe::try_new`.
    fn open() -> Result<Self> {
        if !Path::new(LIBVTCAPTURE_PATH).exists() {
            return Err(anyhow!(
                "histogram capture: libvtcapture driver library not found at {}",
                LIBVTCAPTURE_PATH
            ));
        }
        info!(
            "Loading webOS histogram capture API from {}...",
            LIBVTCAPTURE_PATH
        );
        // SAFETY: dlopen of the system capture library; failure is reported by
        // libloading and mapped to an error below.
        let lib = unsafe { Library::new(LIBVTCAPTURE_PATH) }
            .map_err(|error| anyhow!("Failed to dlopen {}: {}", LIBVTCAPTURE_PATH, error))?;

        // SAFETY: symbol resolution against the just-loaded library; the stored
        // function pointers are only called with the signatures recovered in
        // `analysis/findings.md` §3.2.
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

            Ok(Self {
                fn_init_histogram: *fn_init_histogram,
                fn_set_preset_histogram: *fn_set_preset_histogram,
                fn_get_histogram: *fn_get_histogram,
                fn_finalize_histogram: *fn_finalize_histogram,
                _lib: lib,
            })
        }
    }
}

impl HistogramLib for DlopenHistogramLib {
    fn init(&mut self) -> *mut c_void {
        // SAFETY: the library is loaded and the trampoline is a plain function
        // with no preconditions.
        unsafe { (self.fn_init_histogram)(policy_trampoline) }
    }

    fn set_preset(&mut self, handle: *mut c_void, preset: i32) -> i32 {
        // SAFETY: `handle` originates from init() and outlives the call.
        unsafe { (self.fn_set_preset_histogram)(handle, preset) }
    }

    fn get(&mut self, handle: *mut c_void, buf: *mut VtHistogramEntry) -> i32 {
        // SAFETY: `buf` points at a caller-owned HistogramBuffer holding one
        // entry per zone of the active preset.
        unsafe { (self.fn_get_histogram)(handle, buf) }
    }

    fn finalize(&mut self, handle: *mut c_void) -> i32 {
        // SAFETY: `handle` originates from init(); the caller clears its stored
        // handle first so each session is finalized exactly once.
        unsafe { (self.fn_finalize_histogram)(handle) }
    }
}

// -----------------------------------------------------------------------------
// Backend: screen capture over the histogram API with a reinit state machine
// -----------------------------------------------------------------------------

/// Reinit attempts made inline per `acquire_frame` before giving up.
const REINIT_ATTEMPTS: u32 = 3;

/// Maps a failing `vtCapture_setPresetHistogram` result code to a message.
fn preset_error_context(rc: i32) -> &'static str {
    match rc {
        2 => "histogram mode not activated",
        3 => "bad preset",
        _ => "vtCapture_setPresetHistogram failed",
    }
}

/// Production capture backend rendering the reverse-engineered webOS
/// 24/25 histogram API into a `ZONE_GRID` BGRA canvas, one rect per zone.
///
/// Generic over the library seam; [`HistogramCapture`] is the production
/// alias bound to [`DlopenHistogramLib`].
pub struct HistogramCaptureImpl<L: HistogramLib> {
    lib: L,
    /// Live session handle; NULL whenever the session is not armed.
    handle: *mut c_void,
    width: u32,
    height: u32,
    preset: i32,
    buffer: HistogramBuffer,
    canvas: Vec<u8>,
}

impl<L: HistogramLib> HistogramCaptureImpl<L> {
    /// Arms a session over an already-open seam: init + set_preset.
    fn with_lib(mut lib: L, width: u32, height: u32, preset: i32) -> Result<Self> {
        ensure!(
            (0..ZONES_PER_PRESET.len() as i32).contains(&preset),
            "bad preset {preset}: histogram presets are 0..{}",
            ZONES_PER_PRESET.len()
        );
        // A dropped session may have left an unconsumed policy request; a
        // freshly armed session must not inherit it, or the first
        // acquire_frame would tear down the session just created. Cleared
        // before init so a policy event arriving during arming is preserved.
        REINIT.store(false, Ordering::SeqCst);
        let handle = lib.init();
        if handle.is_null() {
            return Err(anyhow::Error::new(NativeCaptureError {
                backend: "histogram",
                operation: "init",
                code: 0,
            })
            .context("vtCapture_initHistogram returned NULL"));
        }
        let rc = lib.set_preset(handle, preset);
        if rc != 0 {
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
            preset,
            buffer: HistogramBuffer::new(zones),
            canvas: vec![0; width as usize * height as usize * 4],
        })
    }

    /// Teardown + init + set_preset, returning whether the session is armed again.
    fn reinit_once(&mut self) -> bool {
        let old = self.handle;
        self.handle = std::ptr::null_mut();
        if !old.is_null() {
            let rc = self.lib.finalize(old);
            if rc != 0 {
                warn!("vtCapture_finalizeHistogram returned code {rc} during reinit");
            }
        }
        let handle = self.lib.init();
        if handle.is_null() {
            warn!("vtCapture_initHistogram returned NULL during reinit");
            return false;
        }
        self.handle = handle;
        let rc = self.lib.set_preset(handle, self.preset);
        if rc != 0 {
            warn!("vtCapture_setPresetHistogram returned code {rc} during reinit");
            return false;
        }
        true
    }

    /// Consumes the policy reinit request, retrying inline before giving up.
    fn handle_reinit(&mut self) -> std::result::Result<(), CaptureError> {
        for attempt in 1..=REINIT_ATTEMPTS {
            if self.reinit_once() {
                info!("histogram session reinitialized (attempt {attempt}/{REINIT_ATTEMPTS})");
                return Ok(());
            }
        }
        Err(CaptureError::RestartRequired {
            backend: "histogram",
            reason: "reinit failed",
        })
    }

    /// Renders one histogram buffer into the BGRA zone-grid canvas.
    fn render(&mut self) {
        for pixel in self.canvas.as_chunks_mut::<4>().0 {
            *pixel = [0, 0, 0, 255];
        }
        let (grid_w, grid_h) = ZONE_GRID;
        let (width, height) = (self.width, self.height);
        for zone in 0..self.buffer.zones() {
            let (r, g, b) = zone_to_rgb(self.buffer.entry(zone), BIN_LAYOUT, CHANNEL_ORDER);
            let row = (zone as u32) / grid_w;
            let col = (zone as u32) % grid_w;
            let x0 = col * width / grid_w;
            let x1 = (col + 1) * width / grid_w;
            let y0 = row * height / grid_h;
            let y1 = (row + 1) * height / grid_h;
            for y in y0..y1 {
                for x in x0..x1 {
                    let offset = ((y * width + x) * 4) as usize;
                    let pixel = &mut self.canvas[offset..offset + 4];
                    pixel[0] = b;
                    pixel[1] = g;
                    pixel[2] = r;
                    pixel[3] = 255;
                }
            }
        }
    }

    /// Non-consuming peek at the pending policy reinit request.
    /// Still unconsumed by production code (Task 5's AutoCapture may use it).
    #[allow(dead_code)]
    pub fn reinit_requested(&self) -> bool {
        REINIT.load(Ordering::SeqCst)
    }
}

/// Production constructor over the real dlopen seam.
impl HistogramCaptureImpl<DlopenHistogramLib> {
    /// Loads the native histogram API and arms a session at `preset` (0..3).
    pub fn try_new(width: u32, height: u32, preset: i32) -> Result<Self> {
        let lib = DlopenHistogramLib::open().context("histogram capture backend unavailable")?;
        Self::with_lib(lib, width, height, preset)
    }
}

impl<L: HistogramLib> ScreenCapture for HistogramCaptureImpl<L> {
    fn acquire_frame(&mut self) -> std::result::Result<CapturedFrame<'_>, CaptureError> {
        if REINIT.swap(false, Ordering::SeqCst) {
            self.handle_reinit()?;
        }

        self.buffer.prefill_indices();
        let rc = self.lib.get(self.handle, self.buffer.as_mut_ptr());
        if rc == 3 {
            return Err(CaptureError::InvalidFrame(anyhow!(
                "vtCapture_getHistogram returned code 3 (null buffer)"
            )));
        }
        if rc != 0 {
            // rc 2 is "null handle / not active"; any other code is
            // undocumented and treated the same way: a transient outage, never
            // fabricated data.
            return Err(CaptureError::Unavailable(NativeCaptureError {
                backend: "histogram",
                operation: "get_histogram",
                code: rc,
            }));
        }
        self.buffer.validate_indices()?;
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
            warn!("vtCapture_finalizeHistogram returned code {rc} during drop");
        }
    }
}

/// Production alias consumed by the runtime wiring.
pub type HistogramCapture = HistogramCaptureImpl<DlopenHistogramLib>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ptr::null_mut;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;

    const TEST_WIDTH: u32 = 320;
    const TEST_HEIGHT: u32 = 180;
    const TEST_ZONES: usize = ZONES_PER_PRESET[0];

    /// Programmable seam: records call order, returns canned codes, and fills
    /// the returned buffer through an injectable closure. Never touches real FFI.
    struct FakeHistogramLib {
        calls: Rc<RefCell<Vec<&'static str>>>,
        /// Token handle returned by `init` when the sequence is exhausted; NULL
        /// simulates init failure.
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

    // 2. REINIT set -> finalize -> init -> set_preset, and still a frame.
    #[test]
    fn reinit_request_reinitializes_and_still_returns_frame() {
        let lib = FakeHistogramLib::new().with_fill(|entry| entry.channels[1535] = 3);
        let calls = lib.calls();
        let mut capture = capture(lib);

        REINIT.store(true, Ordering::SeqCst);
        let frame = capture.acquire_frame().expect("frame after reinit");
        assert_eq!(frame.data.len(), (TEST_WIDTH * TEST_HEIGHT * 4) as usize);
        assert_eq!(
            *calls.borrow(),
            [
                "init",
                "set_preset",
                "finalize",
                "init",
                "set_preset",
                "get"
            ]
        );
        assert!(
            !capture.reinit_requested(),
            "acquire_frame must consume the reinit request"
        );
    }

    // 3. Three consecutive failed reinits -> RestartRequired.
    #[test]
    fn three_failed_reinits_report_restart_required() {
        let token = std::ptr::NonNull::<u8>::dangling().as_ptr().cast();
        // Construction arms the session; all three inline reinit attempts fail.
        let lib = FakeHistogramLib::new().with_init_handles(vec![
            token,
            null_mut(),
            null_mut(),
            null_mut(),
        ]);
        let calls = lib.calls();
        let mut capture = capture(lib);

        REINIT.store(true, Ordering::SeqCst);
        let error = frame_error(&mut capture);
        match error {
            CaptureError::RestartRequired { backend, reason } => {
                assert_eq!(backend, "histogram");
                assert_eq!(reason, "reinit failed");
            }
            other => panic!("expected RestartRequired, got {other:?}"),
        }
        // Construction arms the session (init + set_preset). Each reinit
        // attempt finalizes only when a handle is live: attempt 1 finalizes,
        // its init returns NULL, and attempts 2-3 skip straight to init.
        assert_eq!(
            *calls.borrow(),
            ["init", "set_preset", "finalize", "init", "init", "init"]
        );
    }

    // 4. get returns rc 2 -> Unavailable("get_histogram").
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

    // 5. Wholly-absent buffer -> Unavailable("empty_histogram"), never a black frame.
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

    // 6. Mismatched index in the returned buffer -> InvalidFrame.
    #[test]
    fn mismatched_zone_index_is_invalid_frame() {
        let lib = FakeHistogramLib::new().with_fill(|entry| entry.index = 99);
        let mut capture = capture(lib);

        let error = frame_error(&mut capture);
        assert!(
            matches!(error, CaptureError::InvalidFrame(_)),
            "expected InvalidFrame, got {error:?}"
        );
    }

    // 7. Canvas geometry: zone 5 (row 1, col 1) pure red in exactly its rect.
    #[test]
    fn canvas_geometry_maps_zone_five_to_its_grid_rect() {
        let lib = FakeHistogramLib::new().with_fill(|entry| {
            if entry.index == 5 {
                // All counts at the top bin: mean level 511 -> 255, pure red.
                entry.channels[511] = 0xFF;
            }
        });
        let mut capture = capture(lib);

        let frame = capture.acquire_frame().expect("geometry frame");
        assert_eq!(frame.data.len(), (TEST_WIDTH * TEST_HEIGHT * 4) as usize);

        let width = TEST_WIDTH as usize;
        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * width + x) * 4;
            frame.data[offset..offset + 4].try_into().unwrap()
        };
        // Rect for zone 5: x in [80, 160), y in [45, 90). Corners and centre.
        for y in [45, 67, 89] {
            for x in [80, 120, 159] {
                assert_eq!(pixel(x, y), [0, 0, 255, 255], "inside rect ({x},{y})");
            }
        }

        let mut red = 0;
        for (i, px) in frame.data.chunks_exact(4).enumerate() {
            let x = (i as u32) % TEST_WIDTH;
            let y = (i as u32) / TEST_WIDTH;
            if px[2] == 255 {
                assert!(
                    (80..160).contains(&x) && (45..90).contains(&y),
                    "red pixel outside the rect at ({x},{y})"
                );
                red += 1;
            } else {
                assert_eq!(px, [0, 0, 0, 255], "pixel ({x},{y}) must be opaque black");
            }
        }
        assert_eq!(red, 80 * 45, "exactly the rect's pixels may be red");
    }

    // 8. zone_to_rgb: count-weighted mean level, scaling, and saturation.
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

    // 9. A failed get (rc != 0) must leave no stale colours behind.
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

    // 10. Construction clears a stale reinit request (session hygiene).
    #[test]
    fn construction_clears_a_stale_reinit_request() {
        REINIT.store(true, Ordering::SeqCst);
        let lib = FakeHistogramLib::new().with_fill(|entry| entry.channels[0] = 1);
        let calls = lib.calls();
        let mut capture = capture(lib);
        assert!(
            !capture.reinit_requested(),
            "a fresh session must not inherit a dropped session's reinit request"
        );
        capture.acquire_frame().expect("frame");
        assert_eq!(
            *calls.borrow(),
            ["init", "set_preset", "get"],
            "no reinit may run on the first acquire"
        );
    }

    // try_new error mapping (rc from set_preset).
    #[test]
    fn set_preset_failure_maps_to_native_error_context() {
        let lib = FakeHistogramLib::new().with_preset_rc(2);
        let error = with_lib_error(lib, 0);
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
