# webOS 25 Histogram Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a DRM-safe histogram capture backend (libvtcapture histogram API, `/dev/video60`) to lg-hue-sync for the LG C5 (webOS 25), with a three-way `capture_backend` toggle (`auto`/`frame`/`histogram`) where `auto` falls back to histogram when frames go black.

**Architecture:** New `HistogramCapture` backend beside `vtcapture.rs`/`dile_vt.rs` implementing the existing `ScreenCapture` trait; zone histograms are converted to RGB and block-upsampled onto the configured capture canvas as a BGRA frame so all downstream (letterbox, smoothing, Hue/Nanoleaf mapping) is unchanged. An `AutoCapture` wrapper owns the frame→histogram switching. A probe subcommand (`test-capture --histogram`) resolves the last runtime unknowns on the C5.

**Tech Stack:** Rust 2021, `libloading` (existing pattern), `libc` ioctls not required (library path first), tokio/tracing as in repo.

**Spec:** `docs/superpowers/specs/2026-10-21-webos25-histogram-capture-design.md` — the design; `analysis/findings.md` §3.2 — the recovered API evidence. Both travel with this plan.

## Global Constraints

- Host gate must pass after every task: `cargo fmt --all -- --check && cargo nextest run --locked && cargo clippy --bin lg-hue-sync -- -D warnings`.
- Target build must pass for runtime changes: `make build` (`armv7-unknown-linux-gnueabi`); the C5 is aarch64-kernel/ARM32-userland, same triple.
- Never fabricate zone data: whole-buffer-absent is an error; individual zero zones are legitimate black.
- C1 behavior unchanged: with no config change, `auto` must resolve to today's chain when histogram symbols are absent.
- No commits of anything under `analysis/` (gitignored; contains proprietary material).
- On-device steps (probe runs, deploys) are user-directed checkpoints — the executor prepares commands, the user runs them.
- Exact recovered constants (from findings §3.2): library path `/usr/lib/libvtcapture.so.1`; symbols `vtCapture_initHistogram(bool(*)(void)) -> *mut c_void`, `vtCapture_setPresetHistogram(*mut c_void, i32) -> i32`, `vtCapture_getHistogram(*mut c_void, *mut VtHistogramEntry) -> i32`, `vtCapture_finalizeHistogram(*mut c_void) -> i32`; entry = `{ i32 index; [u8; 3*512] channels }` = 0x604 bytes; zone counts per preset `[16, 16, 16, 14]`; success rc 0; rc 2 = null-handle/not-active, rc 3 = null-buffer/bad-preset.

## Review Focus

1. **DRM session start mid-stream** — policy callback fires → next `acquire_frame` re-inits and returns fresh data; loop must not stall or error out. Test in Task 3.
2. **All-zero histogram vs all-black frame ambiguity** — histogram backend must report whole-buffer-absent as `Unavailable`, never as a valid black frame, or `AutoCapture`'s black-detector will fight it. Tests in Tasks 3 and 5.
3. **Frame backend busy (code 11) while histogram session active** — `auto` must degrade to histogram-only, not die; when frame returns, resume. Test in Task 5.
4. **Old config without `capture_backend`** — parses as `auto`, byte-identical behavior on the C1. Test in Task 4.
5. **Fallback flapping** — switching in either direction requires N consecutive trigger frames (hysteresis), or DRM intro flashes will ping-pong backends. Test in Task 5.

---

### Task 1: Histogram probe — FFI types and buffer helpers (host-testable core)

**Files:**
- Create: `src/capture/histogram_types.rs`
- Modify: `src/capture/mod.rs` (add `pub mod histogram_types;`)

**Interfaces:**
- Produces: `pub const LIBVTCAPTURE_PATH: &str = "/usr/lib/libvtcapture.so.1";`, `pub const ZONES_PER_PRESET: [usize; 4] = [16, 16, 16, 14];`, `pub const ZONE_GRID: (u32, u32) = (4, 4);` (row-major hypothesis, validated in Task 7), `#[repr(C)] pub struct VtHistogramEntry { pub index: i32, pub channels: [u8; 1536] }` with `pub const ENTRY_SIZE: usize = 0x604;`, `pub struct HistogramBuffer { entries: Vec<VtHistogramEntry> }` with `HistogramBuffer::new(zones: usize)`, `fn prefill_indices(&mut self)`, `fn as_mut_ptr(&mut self) -> *mut VtHistogramEntry`, `fn zones(&self) -> usize`, `fn entry(&self, i: usize) -> &VtHistogramEntry`, `fn is_wholly_absent(&self) -> bool` (true when every entry's channels are all zero), `pub fn validate_indices(&self) -> Result<(), anyhow::Error>` (each `index == position`, else InvalidFrame-style error naming the mismatch).

- [ ] **Step 1: Write failing tests in `src/capture/histogram_types.rs` (`#[cfg(test)] mod tests`)**

Assert: `size_of::<VtHistogramEntry>() == 0x604`; `offset_of!(VtHistogramEntry, channels) == 4`; `ZONES_PER_PRESET[0] == 16 && ZONES_PER_PRESET[3] == 14`; a 16-zone buffer after `prefill_indices` has `entry(i).index == i as i32` for all i and `as_mut_ptr()` alignment 4; `validate_indices` fails on a buffer with one index overwritten to 99 and succeeds on a fresh one; `is_wholly_absent` is true for a fresh zeroed buffer and false after setting one channel byte non-zero.

- [ ] **Step 2: Run tests, verify failure**

Run: `cargo nextest run -p lg-hue-sync histogram_types` — module not found / compile error.

- [ ] **Step 3: Implement `histogram_types.rs`**

Plain data + helpers, no FFI calls. Use `memoffset::offset_of!` only if already a dependency; otherwise assert offset via a constructed pointer cast in tests (avoid new deps).

- [ ] **Step 4: Run tests, verify pass** — `cargo nextest run histogram_types`

- [ ] **Step 5: Commit** — `git add src/capture/histogram_types.rs src/capture/mod.rs && git commit -m "feat(capture): histogram FFI types and buffer helpers"`

### Task 2: Probe subcommand `test-capture --histogram`

**Files:**
- Create: `src/capture/histogram_probe.rs` (library-agnostic probe runner)
- Modify: `src/main.rs` (subcommand flag + dispatch at the `test-capture` match arm, ~line 111 and `run_test_capture`, ~line 1589)

**Interfaces:**
- Consumes: Task 1 types.
- Produces: `pub struct HistogramProbe { handle: *mut c_void, lib: libloading::Library, fns: ProbeFns }` with `HistogramProbe::run(seconds: u64, preset: i32, period: Duration)` logging per-zone channel sums — reusing the dynamic-load pattern of `src/capture/vtcapture.rs:157-290` (`Library::new`, `lib.get(b"vtCapture_initHistogram\0")`, error 11/EBUSY messaging adapted to `/dev/video60`). The policy callback is an `extern "C" fn(*mut c_void) -> bool` trampoline setting an `AtomicBool` stored via `Box::into_raw` ctx — note: recovered API passes a bare `bool (*)(void)` with no user-data argument, so the trampoline must set a process-global `static REINIT: AtomicBool`.

- [ ] **Step 1: Write failing tests** — probe summary math is pure: `pub fn zone_summary(entry: &VtHistogramEntry) -> (u64, u64, u64)` returning per-channel sums (u8 interpretation: sum every byte of each 512-byte block; also `zone_summary_u16` summing `u16` LE pairs). Tests: synthetic entry with channel 0 all 1s, others 0 → (512, 0, 0); u16 variant with two bytes `01 00` per bin → (256, 0, 0).

- [ ] **Step 2: Run, verify fail** — `cargo nextest run histogram_probe`

- [ ] **Step 3: Implement probe + CLI flag** — `--histogram` flag on the `test-capture` clap subcommand; when set, `run_test_capture` ignores `--raw-nv12-stats`, constructs `HistogramProbe`, runs 20 iterations at 100 ms printing `zone_summary` per zone plus raw first-8-bytes hex of zones 0 and 15 (this is what disambiguates u8 vs u16 on-device). All dlopen/dlsym failures return `anyhow!` with the symbol/library name — no panics on host (library absent → clean error).

- [ ] **Step 4: Host gate** — full gate command from Global Constraints (expect pass; clippy must not flag the raw-pointer trampoline — use `#![allow(clippy::missing_safety_doc)]` at fn level only).

- [ ] **Step 5: Commit** — `git commit -m "feat(capture): test-capture --histogram probe for webOS 24/25"`

### Task 3: `HistogramCapture` backend with reinit state machine

**Files:**
- Create: `src/capture/histogram.rs`
- Modify: `src/capture/mod.rs` (`pub mod histogram;`, `pub use histogram::HistogramCapture;`)

**Interfaces:**
- Consumes: Task 1 types; `ScreenCapture`, `CapturedFrame`, `CaptureError`, `NativeCaptureError` from `src/capture/mod.rs`.
- Produces: `pub struct HistogramCapture`; `HistogramCapture::try_new(width: u32, height: u32, preset: i32) -> anyhow::Result<Self>` (dlopen + init + setPreset; error code → `NativeCaptureError { backend: "histogram", .. }` with rc 2 mapped to message "not activated" and rc 3 "bad preset"); `impl ScreenCapture for HistogramCapture` returning a BGRA canvas (`is_bgra: true`) of exactly `width × height × 4` bytes; `fn reinit_requested(&self) -> bool` (reads the global `REINIT` AtomicBool); internal `fn reinit(&mut self) -> Result<(), CaptureError>` = finalize + init + setPreset. Error mapping per findings: rc 2 → `CaptureError::Unavailable` (internal reinit does NOT go through `CaptureRecovery` — `acquire_frame` retries reinit up to 3 times inline before returning `RestartRequired { backend: "histogram", reason: "reinit failed" }`); `is_wholly_absent` buffer → `CaptureError::Unavailable` with operation "empty_histogram" (the backend owns a Task 1 `HistogramBuffer` sized by `ZONES_PER_PRESET[preset]`).

- [ ] **Step 1: Write failing tests (state machine with injected fakes)**

Structure the backend so all `unsafe` FFI lives behind a trait `HistogramLib` with `fn init(&mut self, cb) -> *mut c_void`, `fn set_preset(&mut self, h, i32) -> i32`, `fn get(&mut self, h, *mut VtHistogramEntry) -> i32`, `fn finalize(&mut self, h) -> i32`, plus a real impl over dlopen and a `FakeHistogramLib` in tests. Tests: (a) healthy path fills canvas; (b) `REINIT` set → next `acquire_frame` calls finalize+init+set_preset (fake records call order) then succeeds; (c) three consecutive failed reinits → `RestartRequired`; (d) rc 2 → `Unavailable`; (e) wholly-absent buffer → `Unavailable` (never a black frame); (f) mismatched indices → `InvalidFrame`.

- [ ] **Step 2: Run, verify fail** — `cargo nextest run histogram::`

- [ ] **Step 3: Implement** — state: `enum State { Active, ReinitPending }`; canvas render: for each zone compute RGB via Task 2 `zone_summary`/`zone_to_rgb` (choose interpretation from `pub enum BinLayout { U8x512, U16x256 }` const `BIN_LAYOUT` defaulting `U8x512`, and `CHANNEL_ORDER` default `(0,1,2)`; both finalized in Task 7), then block-fill the canvas row-major per `ZONE_GRID` (zone z → rect of `width/grid.0 × height/grid.1`; every pixel in rect gets the zone RGB, BGRA byte order).

- [ ] **Step 4: Canvas geometry test** — with `ZONE_GRID=(4,4)`, `width=320,height=180`: 16 zones, zone 5 (row 1, col 1) set pure red via its histogram bytes → exactly pixels in x∈[80,160), y∈[45,90) are (0,0,255,255) BGRA, all other zones black.

- [ ] **Step 5: Host gate + target build** — gate command; then `make build` (libloading only, no new deps).

- [ ] **Step 6: Commit** — `git commit -m "feat(capture): histogram backend with DRM reinit state machine"`

### Task 4: `capture_backend` config and selection plumbing

**Files:**
- Modify: `src/config.rs` (Config struct near `capture_width`/`capture_height`, ~line 227), `src/capture/mod.rs` (`create_capture` signature), `src/main.rs` (4 call sites: lines 499, 1156, 1472, 1621)

**Interfaces:**
- Consumes: Task 3 `HistogramCapture`.
- Produces: `#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)] #[serde(rename_all = "lowercase")] pub enum CaptureBackendMode { Auto, Frame, Histogram }` (impl Default → Auto) in `config.rs` with `#[serde(default)] pub capture_backend: CaptureBackendMode`; new signature `pub fn create_capture(width: u32, height: u32, mode: CaptureBackendMode) -> Box<dyn ScreenCapture>` in `capture/mod.rs` with selection: `Frame` → today's VtCapture→DileVt→Mock chain unchanged; `Histogram` → HistogramCapture→(fallback chain on failure)→Mock; `Auto` → `AutoCapture` from Task 5 (until then, temporarily resolves to the Frame chain; Task 5 replaces).

- [ ] **Step 1: Failing config tests** — parse `{}` → `Auto`; `{"capture_backend":"histogram"}` → `Histogram`; invalid string → error; round-trip serialize preserves value.

- [ ] **Step 2: Run, verify fail; implement enum + field; verify pass** — `cargo nextest run config`

- [ ] **Step 3: Change `create_capture` signature and update the 4 `main.rs` call sites to pass `config.capture_backend`** — compile error drives the change; host gate passes with `Auto` still resolving to the Frame chain (documented in the commit message as temporary until Task 5).

- [ ] **Step 4: Host gate + commit** — `git commit -m "feat(config): capture_backend toggle (auto/frame/histogram)"`

### Task 5: `AutoCapture` wrapper — frame↔histogram switching with hysteresis

**Files:**
- Create: `src/capture/auto.rs`
- Modify: `src/capture/mod.rs` (`pub mod auto;`, wire `Auto` arm of `create_capture`)

**Interfaces:**
- Consumes: `ScreenCapture`, `CaptureError`, `HistogramCapture`, `CaptureBackendMode`.
- Produces: `pub struct AutoCapture` with `AutoCapture::try_new(width: u32, height: u32) -> Box<dyn ScreenCapture>` (tries VtCapture, then DileVt, then Histogram, then Mock; if frame backend available it is primary) and `#[cfg(test)] AutoCapture::with_backends(frame: Box<dyn ScreenCapture>, histogram_factory: HistogramFactory, mock: Box<dyn ScreenCapture>)` where `type HistogramFactory = Box<dyn FnMut() -> Option<Box<dyn ScreenCapture>>>` lazily constructs the histogram backend on first switch (coexistence risk from the design: histogram session is only opened when needed, closed when switching back — call `Drop` on the inner box).

**Switch rules (constants, tested):** `BLACK_FRAMES_BEFORE_SWITCH: u32 = 8`, `HISTOGRAM_ERRORS_BEFORE_RETURN: u32 = 4`. A frame is "black" when every byte of its data is 0 (same detector class as the OLED gate; reuse if `zones.rs` exposes one, else implement locally). Active backend errors propagate through the existing `CaptureRecovery` unchanged; switching itself never counts as a restart.

- [ ] **Step 1: Failing tests with fakes** — (a) 7 black frames → still frame backend; 8th black → histogram backend active (fake histogram returns colored frame); (b) histogram returns 4 consecutive `Unavailable` → switch back to frame; (c) frame backend returns `Unavailable` immediately (busy 11) and histogram absent → error propagates, not a panic; (d) hysteresis counter resets on one healthy frame; (e) `is_real_hardware()` reflects the active backend.

- [ ] **Step 2: Run, verify fail; implement; verify pass** — `cargo nextest run auto::`

- [ ] **Step 3: Wire `Auto` arm in `create_capture`; full host gate + `make build`**

- [ ] **Step 4: Commit** — `git commit -m "feat(capture): auto backend with DRM-aware frame/histogram fallback"`

### Task 6: C5 deployment readiness

**Files:**
- Modify: `Makefile` (nothing structural — confirm `TV_IP` parametric flow covers the C5; add `test-histogram` target mirroring `test-capture` running `test-capture --histogram`), `scripts/deploy.sh` only if a hardcoded assumption blocks a second TV.

**Interfaces:**
- Produces: `make test-histogram TV_IP=<ip>` (SSH run of the probe on any TV), deployment flow unchanged.

- [ ] **Step 1: Add `test-histogram` target** mirroring the existing `test-capture` target (Makefile ~lines shown in plan research); verify `make -n test-histogram TV_IP=1.2.3.4` prints the SSH command.

- [ ] **Step 2: Host gate + `make build`; commit** — `git commit -m "build: test-histogram target for webOS 24/25 probe"`

### Task 7: On-device probe run, constant finalization, docs (USER CHECKPOINT)

**Files:**
- Modify: `src/capture/histogram.rs` (`BIN_LAYOUT`, `CHANNEL_ORDER`, `ZONE_GRID` constants), `docs/operations.md`, `docs/architecture.md`, `README.md` (Limits section)

**Interfaces:**
- Consumes: built ARMv7 binary from Task 6, user-run probe on the C5.
- Produces: finalized constants + documented procedure.

- [ ] **Step 1: USER runs the probe on the C5** — `make build && make deploy-bin TV_IP=<C5>` then `make test-histogram TV_IP=<C5>` with (a) full-screen red HDMI/test pattern, (b) a corner-limited color patch, (c) Netflix DRM playing. Executor logs the outputs into `analysis/probe-results.md` (gitignored).

- [ ] **Step 2: Set constants from evidence** — u8 vs u16 (hex dump of first 8 bytes decides), channel order (which channel moves for red), zone mapping (which zone index lights for a corner patch), and whether histograms stay populated during (c). If (c) shows wholly-absent data during DRM, STOP: the design's central hypothesis is falsified for a root daemon — report back before proceeding.

- [ ] **Step 3: Update zone-summary tests to the confirmed interpretation; host gate; commit.**

- [ ] **Step 4: Docs** — operations.md: C5 as second `TV_IP` device, probe procedure, one-TV-at-a-time Hue rule (design decision), `capture_backend` modes. architecture.md: histogram path summary with pointer to `analysis/findings.md` facts (constants only, no proprietary content). README Limits: replace "External HDMI playback is the reliable path for protected content" with histogram-mode statement scoped to webOS 24/25. Host gate; commit.

### Task 8: Full verification and first C5 run (USER CHECKPOINT)

- [ ] **Step 1: Full host gate + `make build` + SHA-256 of the ARMv7 ELF recorded.**
- [ ] **Step 2: USER deploys to C5** (`make deploy-bin TV_IP=<C5>`; config.json on the C5 created via dashboard setup or copied credentials path per operations.md), confirms `make status TV_IP=<C5>` + `curl http://<C5>:8088/api/status`.
- [ ] **Step 3: USER physical validation** — non-DRM HDMI content in `auto` (frame path active), then Netflix native app (auto switches, lights keep following). The design's success criterion; report result before closing.
