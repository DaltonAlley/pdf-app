# Deferred workflow startup

Measured on 2026-09-05 UTC against the client-rendered PR 7 baseline (`56e675c3`).
Both variants used release builds, gzip, and the same browser and machine.
Five fresh browser sessions per variant alternated between baseline and split.
The comparison server added 50 ms latency per response and limited each response
to 1.25 MB/s. Assets were compressed before the measurements. These are controlled
local results, not a shared-link network simulation or a mobile-device benchmark.

| Measurement | Baseline | Split |
| --- | ---: | ---: |
| Initial JS, CSS, and Wasm response bodies, compressed | 627,015 bytes | 229,745 bytes |
| Navigation to upload interface, median | 772 ms | 492 ms |
| PDF selection to generic form rendered, median | 82 ms | 250 ms |
| First imposition click to workspace rendered, median | 26 ms | 398 ms |

Initial transfer dropped 63.4%, and the upload interface appeared 36.3% sooner.
The initial split Wasm is approximately 603 KB uncompressed, compared with
1.93 MB for the original bundle. Resource inspection confirmed that startup
requested no `split_*` or shared `chunk_*` Wasm files.

The tradeoff is approximately 169 ms additional latency on first entry to the
generic tools and 372 ms on first entry to imposition in this configuration.
Generic entry includes PDF preflight; imposition timing ends when the workspace
renders, not when every preview is ready. Successful modules are reused within
the browser session. Splitting optimizes initial entry, not the total path for a
user who immediately opens every workflow.

The initial app owns file objects, settings, and cancellation state. Deferred
modules return view factories so reactive state and cleanup belong to the
currently mounted workflow. Failed module requests can retry without replacing
the selected files or repeating generic PDF preflight. Late completions cannot
restore a workflow that the user has left.

Release builds use cargo-leptos's asset hashes and a directory fingerprint of the
complete linked bundle, including rewritten JavaScript references. An old tab therefore cannot accidentally instantiate
a new build's incompatible chunk. If an update removes the old assets, that tab
can encounter a missing-module error; keeping prior versioned assets available
during deployments avoids this. No cross-version state migration is introduced.

Run the project checks from the repository root with
`nix develop .#pdf-app --command nu scripts/check.nu pdf-app`.
Against a running app, exercise startup and deferred-module failures with:

```nu
nu pdf-app/scripts/startup-browser-smoke.nu /path/to/a-small-valid.pdf
```

Use `--url http://127.0.0.1:<port>` to test another local instance.

Verification passed: `scripts/check.nu pdf-app` (276 backend tests, 86 frontend
tests, formatting, linting, documentation, release build, and runtime asset smoke),
browser startup and module-failure recovery, navigation away during a three-second
imposition module delay, and a real PDF-to-PNG export with valid PNG chunks and
compressed pixel data. A full Docker image build was not run.

Startup uses the saved or system theme immediately. The subtle opening indicator
appears only after 700 ms, and initialization errors reveal Retry immediately.
There is no minimum display time. Browser checks of the final startup markup
confirmed no indicator during a 60 ms startup, a reveal at approximately 700 ms
with either JavaScript or Wasm delayed two seconds, immediate errors for failed
requests, successful Retry, and theme selection before initialization. These
UI checks are separate from the controlled bundle measurements above.
