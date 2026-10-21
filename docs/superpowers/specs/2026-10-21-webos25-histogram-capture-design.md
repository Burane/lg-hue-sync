# webOS 25 histogram capture for lg-hue-sync — design

**Date:** 2026-10-21
**Status:** draft for review
**Evidence base:** `analysis/findings.md` (research record) and the decompiled
sources under `analysis/tools/decompiled/` (gitignored, local only).

## Intent

Run lg-hue-sync on a rooted LG C5 (webOS 25) and sync lights — including during
DRM-protected native apps such as Netflix — by capturing through the undocumented
histogram API of `libvtcapture.so.1`, the same mechanism the official Philips Hue
Sync TV app uses. The user watches DRM content on the C5's native apps; the
official app proves the picture is achievable there; lg-hue-sync should achieve it
with our own outputs (Hue Entertainment + Nanoleaf 4D) and without the app's
subscription.

## Why

- DRM playback blacks out the frame-dump path on all known webOS versions; the
  official app's use of the histogram statistics path demonstrates the pipe exists
  and serves live colors during DRM **for that app**. That it works identically for
  a root daemon is a hypothesis the probe must falsify or confirm (findings §5).
- The user's TV is a rooted C5. The C1 (webOS 6) remains deployed as-is; nothing
  about it should change.
- Root outranks the official app's privileged jail; no permission gate has been
  observed, though none has been disproven either.

## Goals

1. New capture backend (`src/capture/histogram.rs` or equivalent) that drives the
   four histogram functions with the app's exact calling discipline (pre-filled
   zone indices, reinit on policy callback).
2. DRM-session resilience: when the library fires the policy callback, the daemon
   re-runs finalize → init and continues — lights stay in sync across DRM session
   start/stop, matching the native app.
3. 16-zone spatial colors feeding the existing Hue Entertainment and Nanoleaf
   outputs and the existing dashboard.
4. Deployment to the C5 with the repo's current safety rules: binary updates
   preserve config, prior binary recoverable, verification by digests + dashboard
   + physical light check.
5. Physical validation: DRM content playing on a native app, lights following,
   verified in person (a healthy process alone is insufficient — repo rule).

## Non-goals

- No change to C1/webOS 6 behavior; its build path and capture backends stay
  untouched and gate-clean.
- No attempt to recover actual pixels or defeat the frame-path DRM blackout; we
  use statistics only.
- No replication of the official app's UI, licensing, cloud, or account features.
- No Homebrew Channel publication of a C5 package until the package owns its full
   lifecycle (repo rule).

## Anti-goals (failure even if it "works")

- Committing any proprietary Signify material, device credentials, TV addresses,
  or populated configs (analysis/ stays gitignored).
- Breaking the existing host gate (`cargo fmt --check`, `nextest --locked`,
  `clippy -D warnings`) or the ARMv7 target build.
- A histogram path that fabricates data: the error state triggers only when the
  **whole** buffer is absent/unfilled (all 16 zones); individual zero zones are
  legitimate black content, and zone indices the library leaves at defaults
  (the official app defaults 14/15 empty) are not fabricated into colors.

## Constraints

- Target triple stays `armv7-unknown-linux-gnueabi` (Debian Buster glibc 2.28
  floor). The official service being a 32-bit ARM ELF says webOS 25 runs ARM32
  native services; glibc on the C5 must be verified ≥ 2.28 before relying on it.
- All library access stays defensive dynamic loading (dlopen/dlsym, runtime
  symbol checks, graceful failure) per `docs/architecture.md` — firmware variance
  is expected.
- Upload/install/restart on any TV requires the user's explicit direction
  (repo rule); the plan must stage on-device steps as user-approved checkpoints.
- The four runtime unknowns (ctx layout, bin semantics, zone mapping, preset
  values) are unresolved until a probe runs on the C5; the design must not guess
  them silently.

## The how, as decided

0. **Library decompile before probe (precondition).** The user runs
   `pull-vtcapture.ps1` so `libvtcapture.so.1` from the C5 can be decompiled
   locally. The ctx struct layout is a library-side fact the app binary cannot
   reveal; the probe is designed against the library, not guessed.
1. **Probe before integration.** First deliverable is a standalone probe mode
   (in the existing `test-capture` tool or a sibling) that on the C5: dlopens
   `libvtcapture.so.1`, resolves the four symbols, builds a minimal ctx (callback
   that logs + sets a reinit flag), inits, optionally sets preset, then loops
   getHistogram printing per-zone summaries (channel sums / peaks). The probe
   resolves: ctx layout feasibility, bin semantics (u8 vs u16 — inspect data),
   zone activity vs known on-screen colors, preset return codes, and whether DRM
   playback truly leaves histograms populated for a root process. The probe never
   saves full frames.
2. **Backend, not rewrite.** Histogram capture lands as a new capture backend
   beside `vtcapture.rs`/`dile_vt.rs`. Backend selection is a new config field
   `capture_backend` with three modes (decided by the user): **`auto`** (default —
   frame capture preferred while healthy, automatic fallback to histogram when
   frames come back black, e.g. during DRM; the black-frame signal reuses the
   existing OLED-gating logic), **`frame`** (force the classic path),
   **`histogram`** (force the Hue-app path), threaded through the
   `create_capture` call sites in `main.rs`. The C1's default stays `auto`
   resolving to today's chain (histogram backend simply absent/unloadable there,
   so behavior is unchanged); on the C5 `auto` gives full fidelity on normal
   content and DRM survival. The probe determines whether both capture sessions
   can coexist in one process; if not, `auto` performs a clean
   finalize→switch→init transition instead of parallel sessions.
3. **Calling discipline copied from the official app:** pre-fill indices 0..15,
   memset the 0x6040 buffer each tick, getHistogram → 0 is success, 2 latches
   unavailable, any policy callback triggers finalize+init on the next tick.
4. **Zone colors into the existing pipeline.** Preferred: convert the 16 zone
   histograms into a 4×4 RGB image per tick, then **block-upsample onto the
   configured capture canvas** so the existing downstream (letterbox detection,
   smoothing, Hue channel mapping, Nanoleaf panels) operates unchanged — a bare
   4×4 canvas would make letterbox detection structurally inert (bars need ≥4
   rows; a 4-row canvas cannot express them), so upsampling is required, not
   optional. Fallback (builder's call): a direct zone-color source if the frame
   shim distorts behavior.
5. **Error taxonomy:** histogram rc==2 maps to `CaptureError::Unavailable`;
   a failed reinit maps to `RestartRequired`; the internal finalize→init cycle
   does not consume the existing recovery-restart budget.
6. **Tick rate:** getHistogram is driven from the existing `frame_interval`
   loop; no new scheduler.
7. **Preset default:** `video_preset` unless config says otherwise; runtime
   switching is future work.
8. **Deployment:** the Makefile/deploy tooling is already parametric on
   `TV_IP`; the C5 is a documented second device using the same mechanism (no
   new tooling), same service unit pattern, same config-preservation and
   rollback rules, dashboard reachable as today.
9. **Docs:** update `docs/architecture.md` (histogram path, DRM behavior) and
   `docs/operations.md` (C5 deploy + probe procedure) as part of the work, not
   after.

## What's left to the builder

- On-device probe resolves the last unknowns: bin semantics (primary-color test),
  zone→region mapping (position patches), whether direct `/dev/video60` ioctls
  work without the resource-manager acquire. Everything else is now specified
  (see findings §3.2: callback pointer, preset table {16,16,16,14}, control ids
  0x982a14/0x982a15/0x982a16, errno map).
- Rust `repr(C)` definitions and the reinit state machine (finalize → init on
  policy callback; rc 2/3 semantics).
- Zone→RGB math (bin weighting, channel order confirmation from probe).
- How the 16 zones map onto the user's Hue Entertainment area and Nanoleaf
  layout (reuse existing calibration/mapping; do not fabricate).
- Config schema for backend selection + preset (video/games), with safe default
  that never disturbs existing configs.
- Test strategy: host-side unit tests for parsing/validation of the 0x6040
  buffer and the reinit state machine; on-device checks per `docs/operations.md`.
- Read GLIBC_x.xxx symbol versions from `libvtcapture.so.1.0.0` (in
  analysis/huesync or %TEMP%) to confirm the C5 glibc floor; verify service
  manager behavior before first deploy.

## Open questions for the user

1. ~~Histogram exclusive on the C5, or frame-fallback?~~ **DECIDED (user, 2026-10-21):
   three-way `capture_backend` toggle — `auto` (default: frame capture preferred,
   histogram fallback when frames go black, e.g. DRM), `frame` (force classic),
   `histogram` (force Hue-app path).** The probe determines whether frame capture
   works on the C5 and whether both sessions coexist in one process; if not,
   `auto` performs a clean finalize→switch→init transition.
2. ~~Shared Hue Bridge / Entertainment area between C1 and C5?~~ **DECIDED (user,
   2026-10-21): one TV streams at a time.** Documented as an operating rule in
   `docs/operations.md` when the C5 deployment lands; no code coordination.

All design questions resolved. Awaiting user approval of this document before
the implementation plan is written.

## Builder-review record (2026-10-21)

Independent review verdict: buildable. Settled by reviewer-defaults, now part of
this design: `capture_backend` config field with `auto` default; block-upsampled
zone canvas so letterbox detection keeps working; whole-buffer-absent (not
per-zone-zero) as the error condition; rc==2 → Unavailable, failed reinit →
RestartRequired, internal reinit exempt from restart budget; frame_interval-driven
tick rate; `video_preset` default; C5 as documented second `TV_IP` device with no
new tooling; decompile-before-probe ordering. Hypothesis hedges in the Why section
were tightened per review.
