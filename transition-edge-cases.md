# Loading transition fixes

Agent-browser frame measurements found three related transitions:

- First upload mounted a 118 px loading card before a 506 px workflow, moving
  its top upward by about 194 px.
- Retrying a failed generic module shrank the error card from 162 to 118 px,
  then expanded it to 506 px. This was reproduced twice.
- Replacing imposition artwork inserted a 116 px inspection row above the
  retained workspace for 33–67 ms, moving the controls down in one frame.

The app now retains its upload view until the generic workflow factory is ready.
Initial module errors and Retry use that same upload view. Factories are cached
at the app owner; views and their signals still belong to the current workflow.
The separate form entrance animation was removed while card resizing remains.
Imposition inspection feedback overlays the retained workspace and exposes Cancel
without adding a layout row. Visual feedback waits 200 ms, avoiding an overlay
flash for quick inspections, and disappears immediately when inspection finishes.

Verification also found that collapsing Chunks at 800 px made the action jump
ahead of its shrinking card. The stacked layout now lets the tool form fill the
remaining animated grid row instead of immediately collapsing to its contents.

## Browser evidence

`scripts/transition-browser-smoke.nu` passed against the release build: no short
intermediate card, stable first-reveal height, no shrinking upload view on Retry,
unchanged imposition control bounds under delayed inspection, and working Cancel.
The suite also checks repeated narrow Chunks collapse: the action maintains its
distance from the card bottom throughout the transition.
The comparison recording showed a single 502 px workflow height and stable
position throughout the first reveal; no loading card was rendered.

An independent subagent verified unchanged setup/preview geometry at 1280×633
and 800×900. Cancellation retained the original source; successful replacement
updated dimensions and page count and enabled continuation. The existing startup
and inspection browser smoke scripts also passed, covering failed asset recovery,
file identity and settings, cancelled/late inspection results, and visible corrupt
replacement errors.

A delayed imposition module was allowed to finish after switching to a generic
tool and removing every file. The app remained on the upload view. Reuploading
and returning to imposition reused the module without a second fetch.

Independent 800 px measurements confirmed a constant 32 px action-to-card gap
on two Chunks collapses. Reduced motion reached the final layout in one frame.
`scripts/check.nu pdf-app` passed: 276 backend and 86 frontend tests, formatting,
linting, documentation, native/Wasm checks, release build, and runtime smoke.

These are local browser observations, not cross-device performance benchmarks.
