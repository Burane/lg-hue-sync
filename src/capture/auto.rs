// -----------------------------------------------------------------------------
// AutoCapture: production `auto` capture mode.
//
// Frame capture preferred while healthy; when frames come back black (DRM
// playback on webOS 24/25), switch to the histogram backend; when the
// histogram reports sustained unavailability, switch back. Switching never
// fabricates frames or errors and never counts as a restart: `CaptureRecovery`
// in main.rs keeps owning retry/restart budgets.
// -----------------------------------------------------------------------------

use tracing::{debug, info, warn};

use super::histogram::HistogramCapture;
use super::{CaptureError, CapturedFrame, MockCapture, ScreenCapture, create_frame_capture};

/// Consecutive black frames tolerated on the frame backend before switching
/// to the histogram backend (roughly a quarter second at 30 fps).
pub const BLACK_FRAMES_BEFORE_SWITCH: u32 = 8;

/// Consecutive `Unavailable` errors tolerated on the histogram backend before
/// returning to the frame backend.
pub const HISTOGRAM_ERRORS_BEFORE_RETURN: u32 = 4;

/// Which backend `acquire_frame` currently serves from.
enum Active {
    Frame,
    Histogram,
    Mock,
}

/// Builds a histogram backend per switch attempt: `Some(backend)` on success,
/// `None` when the platform has no histogram API (e.g. a C1).
type HistogramFactory = Box<dyn FnMut() -> Option<Box<dyn ScreenCapture>>>;

pub struct AutoCapture {
    active: Active,
    /// Frame-chain backend, created at construction and kept alive for the
    /// AutoCapture's lifetime. `None` only when the chain degraded to the
    /// synthetic mock (host/CI or a TV without frame capture).
    frame: Option<Box<dyn ScreenCapture>>,
    /// Histogram backend, lazily constructed from the factory on the first
    /// Frame→Histogram switch and dropped when returning to frame (which
    /// closes the native session).
    histogram: Option<Box<dyn ScreenCapture>>,
    histogram_factory: Option<HistogramFactory>,
    mock: Box<dyn ScreenCapture>,
    /// Owned canvas staging histogram frames. Histogram frames are copied
    /// here so the returned frame never borrows from the `histogram` field —
    /// that is what lets the switch-back path drop the backend while the
    /// borrow checker sees frames returned from this method (the frame-chain
    /// path is a zero-copy pass-through because its field is never written).
    canvas: Vec<u8>,
    width: u32,
    height: u32,
    black_frames: u32,
    histogram_errors: u32,
}

impl AutoCapture {
    /// Builds the production auto capture. The frame chain (VtCapture →
    /// DileVtCapture) is the primary backend when it yields real hardware;
    /// otherwise the histogram backend is tried immediately; when neither
    /// exists (host/CI) the synthetic mock serves. Never fails.
    pub fn try_new(width: u32, height: u32) -> Box<dyn ScreenCapture> {
        let frame = create_frame_capture(width, height);
        let histogram_factory = Self::histogram_factory(width, height);
        if frame.is_real_hardware() {
            info!("auto capture: frame chain selected as the primary backend");
            return Box::new(Self::from_parts(
                Active::Frame,
                Some(frame),
                None,
                histogram_factory,
                Box::new(MockCapture::new(width, height)),
                width,
                height,
            ));
        }
        // The frame chain degraded to its synthetic mock: prefer the histogram
        // backend right away when this platform has one.
        match HistogramCapture::try_new(width, height, 0) {
            Ok(histogram) => {
                info!("auto capture: histogram capture selected as the primary backend");
                Box::new(Self::from_parts(
                    Active::Histogram,
                    None,
                    Some(Box::new(histogram)),
                    histogram_factory,
                    frame,
                    width,
                    height,
                ))
            }
            Err(error) => {
                warn!(
                    "auto capture: histogram unavailable ({error:#}); synthetic mock selected as the primary backend"
                );
                Box::new(Self::from_parts(
                    Active::Mock,
                    None,
                    None,
                    histogram_factory,
                    frame,
                    width,
                    height,
                ))
            }
        }
    }

    /// Single construction path: counters start at zero and the histogram
    /// staging canvas grows on demand.
    fn from_parts(
        active: Active,
        frame: Option<Box<dyn ScreenCapture>>,
        histogram: Option<Box<dyn ScreenCapture>>,
        histogram_factory: HistogramFactory,
        mock: Box<dyn ScreenCapture>,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            active,
            frame,
            histogram,
            histogram_factory: Some(histogram_factory),
            mock,
            canvas: Vec::new(),
            width,
            height,
            black_frames: 0,
            histogram_errors: 0,
        }
    }

    /// Production histogram factory: preset 0 (video), fresh session per
    /// switch attempt.
    fn histogram_factory(width: u32, height: u32) -> HistogramFactory {
        Box::new(move || match HistogramCapture::try_new(width, height, 0) {
            Ok(capture) => Some(Box::new(capture) as Box<dyn ScreenCapture>),
            Err(error) => {
                debug!("histogram capture unavailable for auto fallback: {error:#}");
                None
            }
        })
    }

    /// Test seam: deterministic construction from injected backends. The
    /// factory is only ever called on the first Frame→Histogram switch; the
    /// frame backend, when present, is the active one. `frame: None` starts
    /// on the mock (the production try-histogram-immediately path is not
    /// exercised through this seam).
    #[cfg(test)]
    pub fn with_backends(
        frame: Option<Box<dyn ScreenCapture>>,
        histogram_factory: HistogramFactory,
        mock: Box<dyn ScreenCapture>,
        width: u32,
        height: u32,
    ) -> Self {
        Self::from_parts(
            if frame.is_some() {
                Active::Frame
            } else {
                Active::Mock
            },
            frame,
            None,
            histogram_factory,
            mock,
            width,
            height,
        )
    }
}

impl ScreenCapture for AutoCapture {
    fn acquire_frame(&mut self) -> std::result::Result<CapturedFrame<'_>, CaptureError> {
        match self.active {
            Active::Mock => self.mock.acquire_frame(),
            Active::Frame => {
                let acquired = self
                    .frame
                    .as_mut()
                    .expect("Frame state must hold a frame backend")
                    .acquire_frame();
                // Propagated unchanged: CaptureRecovery in main.rs owns
                // error budgets, so errors never count toward black_frames.
                let frame = acquired?;
                if is_black_frame(&frame) {
                    self.black_frames += 1;
                    if self.black_frames >= BLACK_FRAMES_BEFORE_SWITCH {
                        let factory = self
                            .histogram_factory
                            .as_mut()
                            .expect("AutoCapture must hold a histogram factory");
                        match factory() {
                            Some(histogram) => {
                                info!("switching to histogram capture");
                                self.histogram = Some(histogram);
                                self.active = Active::Histogram;
                                self.black_frames = 0;
                                self.histogram_errors = 0;
                            }
                            None => {
                                warn!("histogram unavailable, staying on frame");
                                self.black_frames = 0;
                            }
                        }
                    }
                } else {
                    self.black_frames = 0;
                }
                // A black frame is still a valid frame — downstream OLED
                // gating handles it.
                Ok(frame)
            }
            Active::Histogram => {
                let acquired = self
                    .histogram
                    .as_mut()
                    .expect("Histogram state must hold a histogram backend")
                    .acquire_frame();
                match acquired {
                    Ok(frame) => {
                        self.histogram_errors = 0;
                        // Stage through the owned canvas so the returned
                        // frame never pins the histogram backend's borrow
                        // (see the `canvas` field docs).
                        if self.canvas.len() != frame.data.len() {
                            self.canvas = frame.data.to_vec();
                        } else {
                            self.canvas.copy_from_slice(frame.data);
                        }
                        let (width, height, is_bgra) = (frame.width, frame.height, frame.is_bgra);
                        Ok(CapturedFrame {
                            data: &self.canvas,
                            width,
                            height,
                            is_bgra,
                        })
                    }
                    Err(error @ CaptureError::Unavailable(_)) => {
                        self.histogram_errors += 1;
                        if self.histogram_errors >= HISTOGRAM_ERRORS_BEFORE_RETURN {
                            if self.frame.is_some() {
                                info!("returning to frame capture");
                                self.active = Active::Frame;
                            } else {
                                info!("returning to mock capture");
                                self.active = Active::Mock;
                            }
                            self.histogram_errors = 0;
                            // Dropping the backend closes the histogram
                            // session. This must stay the LAST use of `self`
                            // on this path: the sibling Ok path returns a
                            // frame borrowed from this backend, so ending the
                            // borrow here is what lets the write type-check.
                            self.histogram = None;
                        }
                        // The original error is returned as-is for THIS call;
                        // the switch takes effect on the next call, keeping
                        // error semantics honest.
                        Err(error)
                    }
                    // Frame errors and restarts are never counted.
                    Err(error) => Err(error),
                }
            }
        }
    }

    fn resolution(&self) -> (u32, u32) {
        match self.active {
            Active::Frame => self
                .frame
                .as_ref()
                .map_or((self.width, self.height), |backend| backend.resolution()),
            Active::Histogram => self
                .histogram
                .as_ref()
                .map_or((self.width, self.height), |backend| backend.resolution()),
            Active::Mock => self.mock.resolution(),
        }
    }

    fn is_real_hardware(&self) -> bool {
        match self.active {
            Active::Frame => self
                .frame
                .as_ref()
                .is_some_and(|backend| backend.is_real_hardware()),
            Active::Histogram => self
                .histogram
                .as_ref()
                .is_some_and(|backend| backend.is_real_hardware()),
            Active::Mock => false,
        }
    }
}

/// True when every color byte of the frame is zero, alpha ignored. Every
/// producer in this crate keeps colors at pixel offsets 0..2 and alpha at
/// offset 3 for both layouts (`is_bgra`: B,G,R,A; RGBA: R,G,B,A — the
/// vtcapture NV12 conversion and `MockCapture` alike), so blackness is
/// layout-independent: offsets 0,1,2 are checked and the fourth byte never
/// is. Empty data, and data whose length is not a multiple of 4, is never a
/// black frame.
///
/// Note: the task brief's aside about checking offsets 1,2,3 for non-BGRA
/// frames conflicts with this crate's actual RGBA layout (and with the
/// brief's own "all color bytes zero, alpha ignored" semantic and its
/// "RGBA same → true" test); the alpha-ignored reading is implemented.
pub(crate) fn is_black_frame(frame: &CapturedFrame<'_>) -> bool {
    if frame.data.is_empty() || !frame.data.len().is_multiple_of(4) {
        return false;
    }
    frame
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[..3].iter().all(|&byte| byte == 0))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::capture::NativeCaptureError;
    use crate::config::CaptureBackendMode;

    const FRAME_W: u32 = 32;
    const FRAME_H: u32 = 18;
    const HIST_W: u32 = 16;
    const HIST_H: u32 = 9;

    /// Scripted outcome for one `acquire_frame` call. When the script is
    /// exhausted, the last step repeats.
    #[derive(Clone, Copy)]
    enum Step {
        Black,
        Colored,
        Unavailable,
    }

    /// Fake `ScreenCapture`: scripted steps, a resolution distinct from the
    /// other fakes, and a shared dropped-counter bumped on `Drop`.
    struct FakeBackend {
        steps: Vec<Step>,
        cursor: usize,
        buffer: Vec<u8>,
        width: u32,
        height: u32,
        real: bool,
        dropped: Arc<AtomicUsize>,
    }

    impl FakeBackend {
        fn new(
            width: u32,
            height: u32,
            steps: Vec<Step>,
            real: bool,
            dropped: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                steps,
                cursor: 0,
                buffer: vec![0; (width * height * 4) as usize],
                width,
                height,
                real,
                dropped,
            }
        }
    }

    impl Drop for FakeBackend {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ScreenCapture for FakeBackend {
        fn acquire_frame(&mut self) -> std::result::Result<CapturedFrame<'_>, CaptureError> {
            let step = self.steps[self.cursor.min(self.steps.len() - 1)];
            self.cursor += 1;
            match step {
                Step::Unavailable => Err(CaptureError::Unavailable(NativeCaptureError {
                    backend: "fake",
                    operation: "acquire_frame",
                    code: 2,
                })),
                Step::Black => {
                    for pixel in self.buffer.as_chunks_mut::<4>().0 {
                        *pixel = [0, 0, 0, 255];
                    }
                    Ok(CapturedFrame {
                        data: &self.buffer,
                        width: self.width,
                        height: self.height,
                        is_bgra: true,
                    })
                }
                Step::Colored => {
                    for pixel in self.buffer.as_chunks_mut::<4>().0 {
                        *pixel = [10, 20, 30, 255];
                    }
                    Ok(CapturedFrame {
                        data: &self.buffer,
                        width: self.width,
                        height: self.height,
                        is_bgra: true,
                    })
                }
            }
        }

        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn is_real_hardware(&self) -> bool {
            self.real
        }
    }

    fn frame_backend(steps: Vec<Step>, real: bool) -> Box<dyn ScreenCapture> {
        Box::new(FakeBackend::new(
            FRAME_W,
            FRAME_H,
            steps,
            real,
            Arc::new(AtomicUsize::new(0)),
        ))
    }

    fn mock_backend() -> Box<dyn ScreenCapture> {
        frame_backend(vec![Step::Colored], false)
    }

    /// Histogram factory yielding a fresh 16x9 fake per call, counting calls
    /// and sharing the dropped counter across every instance it creates.
    fn histogram_factory(
        steps: Vec<Step>,
        real: bool,
        calls: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    ) -> Box<dyn FnMut() -> Option<Box<dyn ScreenCapture>>> {
        Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(FakeBackend::new(
                HIST_W,
                HIST_H,
                steps.clone(),
                real,
                dropped.clone(),
            )) as Box<dyn ScreenCapture>)
        })
    }

    fn none_factory(calls: Arc<AtomicUsize>) -> Box<dyn FnMut() -> Option<Box<dyn ScreenCapture>>> {
        Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        })
    }

    fn black(n: usize) -> Vec<Step> {
        vec![Step::Black; n]
    }

    /// `is_black_frame` without building a full frame by hand.
    fn is_black(data: &[u8], is_bgra: bool) -> bool {
        is_black_frame(&CapturedFrame {
            data,
            width: (data.len() / 4).max(1) as u32,
            height: 1,
            is_bgra,
        })
    }

    // 1. Switch on the 8th consecutive black frame, returning it as-is.
    #[test]
    fn eighth_consecutive_black_frame_switches_to_histogram_and_is_returned_as_is() {
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let factory = histogram_factory(vec![Step::Colored], true, calls.clone(), dropped.clone());
        let mut auto = AutoCapture::with_backends(
            Some(frame_backend(black(9), true)),
            factory,
            mock_backend(),
            FRAME_W,
            FRAME_H,
        );
        assert!(auto.is_real_hardware(), "frame fake reports real hardware");

        // 7 black frames: still on the frame chain (8th not yet reached).
        for i in 0..7 {
            let frame = auto.acquire_frame().expect("black frame");
            assert_eq!(
                frame.width, FRAME_W,
                "frame {i} must come from the frame fake"
            );
            assert!(is_black_frame(&frame));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no switch before the 8th black frame"
        );

        // 8th black frame: returned as-is AND switches.
        let frame = auto.acquire_frame().expect("switching black frame");
        assert_eq!(
            frame.width, FRAME_W,
            "the switching call returns the frame backend's black frame"
        );
        assert!(is_black_frame(&frame));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "factory called exactly once"
        );
        assert_eq!(
            auto.resolution(),
            (HIST_W, HIST_H),
            "histogram is now the active backend"
        );

        // The next call is served by the histogram fake.
        let frame = auto.acquire_frame().expect("histogram frame");
        assert_eq!(frame.width, HIST_W);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "factory not called again");
    }

    // 2. 4th consecutive histogram Unavailable propagates, drops the box,
    //    returns to frame.
    #[test]
    fn fourth_histogram_unavailable_propagates_drops_histogram_and_returns_to_frame() {
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let factory = histogram_factory(
            vec![
                Step::Unavailable,
                Step::Unavailable,
                Step::Unavailable,
                Step::Unavailable,
                Step::Colored,
            ],
            true,
            calls.clone(),
            dropped.clone(),
        );
        let mut frame_steps = black(8);
        frame_steps.push(Step::Colored);
        let mut auto = AutoCapture::with_backends(
            Some(frame_backend(frame_steps, true)),
            factory,
            mock_backend(),
            FRAME_W,
            FRAME_H,
        );

        for _ in 0..8 {
            auto.acquire_frame().expect("black frame");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "switched to histogram");

        // Three Unavailable errors propagate unchanged; the session stays up.
        for i in 1..=3 {
            match auto.acquire_frame() {
                Err(CaptureError::Unavailable(native)) => assert_eq!(native.code, 2),
                other => panic!("expected Unavailable #{i}, got {:?}", other.err()),
            }
        }
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "histogram box still alive after 3 errors"
        );

        // Fourth Unavailable: propagates as-is for THIS call, drops the
        // histogram box, and switches back to frame.
        match auto.acquire_frame() {
            Err(CaptureError::Unavailable(native)) => {
                assert_eq!(native.code, 2, "the original error is returned unchanged");
            }
            other => panic!("expected 4th Unavailable, got {:?}", other.err()),
        }
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "histogram box dropped once"
        );
        assert_eq!(
            auto.resolution(),
            (FRAME_W, FRAME_H),
            "frame is the active backend again"
        );

        // The next call hits the frame fake (past its black script).
        let frame = auto.acquire_frame().expect("frame after switch-back");
        assert_eq!(frame.width, FRAME_W);
        assert!(!is_black_frame(&frame));
    }

    // 3. Frame errors propagate unchanged and never touch the black counter.
    #[test]
    fn frame_errors_propagate_unchanged_and_never_touch_the_black_counter() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut frame_steps = vec![Step::Unavailable];
        frame_steps.extend(black(8));
        let mut auto = AutoCapture::with_backends(
            Some(frame_backend(frame_steps, true)),
            none_factory(calls.clone()),
            mock_backend(),
            FRAME_W,
            FRAME_H,
        );

        match auto.acquire_frame() {
            Err(CaptureError::Unavailable(native)) => assert_eq!(native.code, 2),
            other => panic!("expected Unavailable, got {:?}", other.err()),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // 7 further black frames stay under the threshold: the error did not
        // advance (or reset) the black counter.
        for _ in 0..7 {
            let frame = auto.acquire_frame().expect("black frame");
            assert_eq!(frame.width, FRAME_W);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no factory call before the 8th black frame"
        );

        // The 8th black frame attempts the switch; the factory returns None,
        // the capture stays on frame, and the black frame is returned as-is.
        let frame = auto.acquire_frame().expect("black frame on failed switch");
        assert_eq!(frame.width, FRAME_W);
        assert!(is_black_frame(&frame));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            auto.resolution(),
            (FRAME_W, FRAME_H),
            "stayed on the frame backend"
        );
    }

    // 4. Hysteresis reset: a colored frame clears the black streak.
    #[test]
    fn a_colored_frame_resets_the_black_streak_before_the_switch_threshold() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut frame_steps = black(7);
        frame_steps.push(Step::Colored);
        frame_steps.extend(black(8));
        let mut auto = AutoCapture::with_backends(
            Some(frame_backend(frame_steps, true)),
            none_factory(calls.clone()),
            mock_backend(),
            FRAME_W,
            FRAME_H,
        );

        for i in 0..15 {
            let frame = auto.acquire_frame().expect("frame");
            assert_eq!(
                frame.width, FRAME_W,
                "call {i} never leaves the frame backend"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "7 black + 1 colored + 7 black stays under the threshold"
        );

        // The 8th black frame after the reset attempts the switch.
        auto.acquire_frame().expect("16th frame");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // 5. `is_real_hardware` reflects the active backend.
    #[test]
    fn is_real_hardware_reflects_the_active_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let factory = histogram_factory(vec![Step::Colored], true, calls.clone(), dropped.clone());
        let mut auto = AutoCapture::with_backends(
            Some(frame_backend(black(8), true)),
            factory,
            mock_backend(),
            FRAME_W,
            FRAME_H,
        );
        assert!(auto.is_real_hardware(), "frame fake is real hardware");
        for _ in 0..8 {
            auto.acquire_frame().expect("black frame");
        }
        assert!(
            auto.is_real_hardware(),
            "histogram fake is real hardware after the switch"
        );

        // Test seam: frame None → Mock, never real hardware.
        let mock_auto = AutoCapture::with_backends(
            None,
            none_factory(calls.clone()),
            mock_backend(),
            FRAME_W,
            FRAME_H,
        );
        assert!(!mock_auto.is_real_hardware());
    }

    // 6. `is_black_frame` checks only color bytes.
    #[test]
    fn is_black_frame_checks_only_color_bytes() {
        let black_pixel = [0u8, 0, 0, 255];
        let mut flat_black = Vec::new();
        for _ in 0..4 {
            flat_black.extend_from_slice(&black_pixel);
        }
        assert!(
            is_black(&flat_black, true),
            "BGRA zero colors, opaque alpha"
        );
        assert!(
            is_black(&flat_black, false),
            "RGBA zero colors, opaque alpha"
        );

        // Green is offset 1 in both layouts (BGRA: B,G,R,A; RGBA: R,G,B,A).
        let mut green_bgra = flat_black.clone();
        green_bgra[1] = 1;
        assert!(!is_black(&green_bgra, true));
        let mut green_rgba = flat_black.clone();
        green_rgba[1] = 1;
        assert!(!is_black(&green_rgba, false));
        // A non-zero red or blue color byte disqualifies too.
        let mut red_bgra = flat_black.clone();
        red_bgra[2] = 4;
        assert!(!is_black(&red_bgra, true));

        assert!(!is_black(&[], true), "empty data is not a black frame");
        assert!(
            !is_black(&[0, 0, 0, 255, 0], true),
            "non-multiple-of-4 length is not a black frame"
        );
    }

    // Wiring: the Auto arm of create_capture degrades to the synthetic mock
    // on hosts without TV libraries (frame chain → mock, histogram → None).
    #[test]
    fn auto_mode_without_tv_libraries_degrades_to_the_synthetic_mock() {
        let mut capture = crate::capture::create_capture(160, 90, CaptureBackendMode::Auto);
        let frame = capture.acquire_frame().expect("Acquire mock frame failed");
        assert_eq!(frame.width, 160);
        assert_eq!(frame.height, 90);
        assert_eq!(frame.data.len(), 160 * 90 * 4);
        assert!(!capture.is_real_hardware());
    }
}
