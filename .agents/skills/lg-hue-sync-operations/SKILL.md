---
name: lg-hue-sync-operations
description: Build, transfer, verify, roll back, uninstall, or release lg-hue-sync for rooted LG webOS TVs while preserving credentials and separating local validation from live proof.
---

# lg-hue-sync operations

Read `AGENTS.md`, then the relevant guide:

- Build/transfer/rollback/uninstall/diagnostics: `docs/operations.md`
- Version/tag/release: `docs/releasing.md`
- Hue/Nanoleaf mapping and ownership: `docs/architecture.md`

## Invariants

- Deploy only `armv7-unknown-linux-gnueabi`; never transfer a host binary.
- Default `make build`/`make build-native` uses the macOS webOS SDK compiler with the extracted Buster sysroot after `make native-setup`, not its older libc/polyfills. Native v0.6.1 passed TV service/capture checks; require ELF/ABI/glibc checks for each artifact. `make build-docker` remains the explicit cached fallback and Linux CI path.
- [`openlgtv/buildroot-nc4`](https://github.com/openlgtv/buildroot-nc4) underpins that SDK; consult [`webosbrew/webos-userland`](https://github.com/webosbrew/webos-userland/tree/main) and its [generated API reference](https://www.webosbrew.org/webos-userland/index.html) for capture headers, then verify symbols and behavior on the target firmware. See `docs/operations.md` for gates.
- Prefer installed `ares-rs-*` aliases for launcher packaging/install and ordinary transfer; retain SSH for root service lifecycle.
- Preserve the TV's paired `config.json` during updates.
- Test source transitions: HDMI → built-in app/Home releases capture and reports `unsupported_source`; returning to HDMI resumes only source-paused sync. Manual Stop must survive input changes. Unknown input observations must not invent compatibility or DRM status.
- Use the shared bounded Luna transport with stdin retained until reply. Closing stdin early makes this firmware's `luna-send` silently exit; validate a real read-only query on the TV rather than relying only on host fixtures.
- Treat credentials, tokens, pins, IPs, and MACs as private.
- Require explicit direction before TV writes, service restarts, Luna changes, autoroot, or light output.
- Verify local/remote SHA-256, service state, and dashboard response after transfer.
- Verify physical output before claiming capture-to-light success.
- Preserve Hue v2 channel IDs and grouped gradient member identity.
- Use CA-verified HTTPS, saved Bridge ID and `/auth/v1` application identity; see `docs/architecture.md` for legacy-pin migration and ownership rules. Do not resume cached Hue output after known deletion, revoked access or a failed layout refresh. Verify recovery on the TV before claiming it works live.
- Work directly on `main`; no branch or PR unless explicitly requested.

Use repository scripts and Make targets. If a script would overwrite configuration or broaden device mutation, fix the script instead of issuing an ad-hoc destructive command.
