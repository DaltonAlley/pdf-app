# Startup edge-case audit

Agent-browser checks on 2026-09-04 found a stylesheet-loading defect in PR 7.
A three-second CSS response delayed the startup indicator until 3,047 ms in
two runs, immediately before the interface appeared. The blocking stylesheet
prevented the module script from starting the 700 ms timer.

The stylesheet now preloads without blocking rendering. Startup installs its
load/error handlers and activates the stylesheet, then waits for it before
initializing the interactive app. CSS failures use the existing immediate
error and Retry path. This also avoids mounting an unstyled interface.

## Browser verification

The release assets were served over loopback HTTP with controlled asset delays
and failures. A document mutation observer recorded startup visibility; these
are local timing observations, not general network performance estimates.

| Case | Observed result |
| --- | --- |
| Fast startup | Ready at 67 ms; indicator never shown |
| CSS delayed two seconds | Indicator at 701 ms; ready at 2,110 ms |
| JavaScript delayed two seconds | Indicator at 700 ms; ready at 2,099 ms |
| Wasm delayed two seconds | Indicator at 700 ms; ready at 2,094 ms |
| CSS / JavaScript / Wasm failure | Error and Retry at 33 / 36 / 57 ms |
| Retry after each restored asset | Interactive upload screen recovered |
| Saved dark theme and system fallback | Applied before initialization |

The expanded `scripts/startup-browser-smoke.nu` also passed against the app on
port 3200: no deferred workflow requests before selection, failed generic and
imposition requests with Retry, retained file identity and settings, no repeated
preflight on generic retry, and CSS/JavaScript/Wasm startup recovery.

Additional browser checks confirmed that a corrupt replacement preserves the
valid source and that an out-of-range page disables Apply with an inline error.

`scripts/check.nu pdf-app` passed: 276 backend and 86 frontend tests, formatting,
linting, documentation, native/Wasm checks, release build, and runtime smoke.
