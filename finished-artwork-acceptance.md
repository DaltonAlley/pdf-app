# Finished-size and mixed-artwork acceptance

## Intended workflow

Choose explicit finished width and height before arranging impressions. Both fields stay visible and editable. Reusable presets, including the starter 5x7 on 12x18 setup, are explicit user choices and never infer sizing from artwork. Applying one preserves the current source and page quantities while replacing reusable setup fields. Presets support create, apply, rename, update, and delete. PDFs and images both require a user-defined size, not automatic original-page sizing. Explicit dimensions survive replacement, retries, and bleed changes. Mixed artwork shares this size unless the user manually enters a per-artwork override. Regular slots are sized for the largest piece; this is not irregular nesting.

Fit keeps the complete artwork visible and may leave borders. Fill preserves proportions and deliberately crops, with shared or per-artwork positioning. Stretch independently scales both axes to fill the finished size and explicitly warns of distortion. Finished orientation controls the product, while impression orientation controls placement on the parent sheet.

Impose uses three bounded contexts in one rail: Setup, Artwork, and Preview. Setup steps are Size, Quantity & sheet, Arrangement, and Bleed. The in-flow toolbar keeps Presets, one global Fit/Fill/Stretch control, impression orientation, the conditional Position entry point, and an override badge. Artwork owns the full crop and mixed-artwork/source inspector with drag, nine anchors, fine-tune sliders, reset, and sticky Done. Preview owns Guides and Fit-sheet/100% zoom. No sheet-covering artwork overlay is used, and the footer remains bounded and visible.

In Edit copy quantities, valid Apply to all commits every page/pair quantity and closes the dialog with focus returned to its opener. Invalid bulk input stays visible and does not apply.

Bleed is a separate enlargement after fitting. Scale to add bleed preserves normalized positioning within the expanded bleed frame, not an exactly fixed landmark relative to the cut. It can remove additional edge artwork. Contain can retain borders. Supplied and manually selected source bleed must use the same resolved artwork extent in raster previews and PDF export.

## Workspace-level Artwork toolbar context and transition polish

Browser measurements against rebuilt assets show exactly one global Artwork context mounted in the workspace toolbar, outside the Setup rail and Preview panel. It presents fitting and impression orientation as the two quick choices, keeps the current state visible, and moves crop editing, per-artwork overrides, plus source and bleed information into a bounded disclosure panel. The desktop footer remains visible at y812–867. Setup owns one scroller, while the right workspace contains the toolbar and sheet inspection tools beside the sheet.

Frame traces identify and correct an immediate 147.8125 px structural jump on Impose entry. The rebuilt motion keeps the first frame at the prior y231.8125/height502.375 geometry; measured card-center error falls below 0.008 px during cold and warm entry. Entry/exit and accepted rapid reversal pass at desktop and laptop sizes. At 1280 × 633 with reduced motion, the first post-click frame equals the settled rectangle in both directions, with no intermediate rectangle.

The 800 × 900 review exposed an offscreen footer caused by the narrow workspace using `align-self: start`, allowing its content to exceed the allocated grid row. Changing that alignment to stretch keeps the scroller bounded and the footer at y812–867. A fresh rebuilt-browser screenshot confirms the footer is visible with the Artwork toolbar and its internally scrolling open panel, without injected CSS.

The complete project check passed at 03:42 UTC on September 14: 303 backend and 95 frontend tests, formatting/lint/docs, split release build, and all integrated browser suites. Observed checks include cold/warm/rapid transitions, delayed completion and failed-module Retry, first-frame reduced motion, all five downloads, Fit/Fill/Stretch exported pixels, real crop dragging, retained settings across narrow tabs and all four Setup steps, fresh 800 px entry with bounded document/footer, and manual-bleed recovery. The production image was freshly rebuilt, and its complete disposable-container suite passed at 03:45 UTC: bounded desktop/narrow setup, all five exports, Fit/Fill/Stretch and bleed pixels, manual-bleed recovery, health/versioned gzip assets, and an actual missing deployed-module Retry/reload workflow. This was a build-and-test run, not a reuse-only container check.

## UI regression correction acceptance (revision `979c53ec`)

These results cover the explicit-sizing and Stretch follow-up before the placement/motion changes.

The final `nix develop .#pdf-app --command nu --no-config-file scripts/check.nu pdf-app` passed at 02:36 UTC on September 14: 303 backend tests, 95 frontend tests, formatting/lint/documentation checks, split release compilation, and all integrated startup, inspection, transition, workflow, mixed-artwork, and manual-bleed browser suites. No tests were skipped.

The updated production image was rebuilt from the corrected application. Its complete disposable-container suite passed on September 14. The final container rerun reused that new image (`--skip-build`) after correcting the test's Pages-button locator; it did not rebuild it again.

| Requirement | Observed check |
| --- | --- |
| Always manually define finished size | Fresh image and PDF uploads show blank, visible width/height fields. Width alone cannot advance. Preset application explicitly sets both dimensions without inferring them from source geometry. The impose UI does not expose original source sizing controls. |
| Recover invalid dimensions predictably | Blank/invalid width blocks progression; applying a reusable preset restores its displayed dimensions and clears stale validation even when the committed size is unchanged. |
| Retain deliberate dimensions | Replacing the image with a mixed Letter/A4/landscape PDF retains 8.5 × 11 and distinct source geometry. Manual-bleed change/clear/failure/Retry retains the chosen cut. |
| Apply to all commits and closes | Keyboard Apply with zero and pointer Apply with 37 close the dialog, restore opener focus, and persist all values on reopening. Zero blocks empty-job progression. Negative bulk input stays invalid; unchanged bulk Apply replaces an invalid individual draft. Duplex quantities apply per page pair. |
| Fit, Fill, and Stretch agree with export | Downloaded PDFs are rasterized through the public endpoint. Fit borders, Fill crop anchors and bleed pixels pass. Stretch independently scales both axes, exposes no crop editor, and retains red/green/blue stripes at top, middle, and bottom of the Letter cut, with white outside it. Backend tests cover rotated exported geometry, supplied/manual bleed, and saved shared/per-piece Stretch settings. |
| Keep Impose simple and usable | Direct agent-browser review at 1366 × 900 and 800 × 900 shows editable dimensions, one workspace-level Artwork toolbar context with three short fitting choices, secondary artwork details in a bounded disclosure, and a visible sheet. Real pointer crop dragging and narrow-screen bounds also pass in the integrated container suite. |
| Preserve surrounding workflows | All five downloads, range recovery, numeric drafts, modal focus, keyboard reorder, failed-upload retry, cancellation, versioned gzip assets, and a physically missing deployed-module Retry/reload workflow pass in the container. |

Browser testing exposed two application causes, not just test failures: same-value operation updates remounted Impose and discarded settings, and same-value reusable preset applications retained stale numeric drafts. A memoized workflow boundary and preset-application-keyed dimension inputs fix these respectively. The test now navigates back to Size explicitly because replacement correctly retains the mounted workflow and its current step.

These checks establish functional usability and output correctness, not measured customer satisfaction.

## PR 7 impose workflow acceptance

The pre-fix browser reproduction used a real PDF upload and a cold entry into
Impose. No control had focus (`document.activeElement` was `BODY`), the Setup
rail displayed numeric status markers (`1`, `2`, `3`, `4`), the Artwork menu
listed `Fit`, `Fill`, `Stretch`, and Presets remained below Finished size. To
reproduce the decimal failure, focus Finished width, press Control+A, press
Backspace, then type `4.25` one key at a time. The controlled input ended at
`254`; a bulk fill of `4.25` appeared to work and therefore did not expose the
bug. The same sequence reproduced the failure for the height field.

The cause was the reactive `value` binding on the shared numeric input. After
the `4` keystroke, the temporary draft `4.` parsed as the committed number
`4`, so the next render replaced the browser value `4.` with `4`. The next
keystrokes then produced `42` and `425`, rather than the intended decimal.
The fix leaves the native input draft uncontrolled while typing, synchronizes
it imperatively only when the committed value actually differs, and keeps the
existing range, finite-number, and whole-number validation. Both finished-size
dimensions now use the same path, including decimal values in request
signatures and rendered cut geometry.

The rebuilt browser workflow verifies real focus on Finished width, toolbar
placement and preset disclosure behavior, the four unchanged step labels with
no numeric markers, keyboard selection of Artwork options in the order
Stretch, Fit, Fill, valid `4.25` × `6.25` input through rendered `.piece-cut`
geometry, and invalid/partial dimension blocking. The project check and the
workflow browser smoke are the acceptance commands for this revision.

## Earlier acceptance evidence (before the UI regression correction)

The following results describe revision `adcc9b34`. Its automatic original-size UI and image-only confirmation behavior are superseded by the requirements above.

- Final `nix develop .#pdf-app --command nu --no-config-file scripts/check.nu pdf-app` passed on September 14: 296 backend tests and 93 frontend tests, formatting, linting, documentation, split production compilation and every integrated browser suite. Earlier iterations corrected canonical frontend formatting and an automation click outside a nested scrollport. The control was clipped correctly by the application. Browser checks now scroll and hit-test the target before clicking.
- A production Docker image built and its disposable loopback container passed health, versioned gzip asset checks, all five workflow downloads, mixed-artwork acceptance and a real missing deployed-module recovery test. The module and gzip sidecar were temporarily renamed only inside that disposable container. Retry retained the source, explicit reload disclosed and cleared workspace state, and restored assets allowed re-entry.
- Real browser upload of a 900×500 labeled red/green/blue image produced a Letter flyer on a 12×18 sheet. Downloaded PDF pages were rendered through the public conversion endpoint. Raster size and independent white/red/green/blue pixel expectations passed for Fit, Fill, left/right crop positions and bleed coverage.
- Desktop and 800×900 preview sheet bounding boxes passed with the cover editor open. Real pointer drags matched backend `positionTravel` and rendered crop-image displacement. Keyboard crop endpoints and Reset passed.
- Mixed Letter/A4/landscape PDF pages retained individual original dimensions. Common sizing and a 6×9 per-artwork override left other pieces unchanged. Odd-page duplex was unavailable. Exported sheet count matched the authoritative layout, rather than incorrectly assuming one source page per output sheet.
- Backend API tests use labeled/asymmetric content to verify mixed exports, authoritative source geometry, duplex flips/back rotation, supplied bleed outside CropBox, preset round trips and resized manual-bleed history without expired upload IDs.

## Final follow-through

Final review identified and fixed the manual-MediaBox exposure case with override-aware preview requests and cache identities bound to the resolved layout. Both the complete project check and the final production-container suite passed real-browser change → clear → failed automatic retries → explicit Retry checks, observing request amounts, cache URLs and displayed blue/red raster pixels. API coverage additionally checks GET/batch equality, export clipping, default-raster recovery and invalid query encodings/amounts. The final container was rebuilt after the parity fix, then its full suite was rerun with the added manual-bleed browser check enabled.

## Bare CI browser environment

The first published PDF check passed native tests and release compilation but failed initial browser navigation. Reproducing the pinned bare Nix CI image showed Chromium also crashing on static HTML, with a fatal Skia font-manager error. The runner had no configured fonts. The PDF development shell now declares `FONTCONFIG_FILE` using `pkgs.makeFontsConf` and DejaVu fonts, without changing browser timeouts or weakening assertions.

With only that font declaration added, the original Chromium/agent-browser pair passed persistent navigation, the complete startup recovery suite and mixed-artwork/manual-bleed acceptance inside the same pinned root CI image with its default shared-memory limit. The full PDF project check and `nix flake check .#` also passed. Hosted status for the follow-up revision is reported separately by Forgejo.

Historical startup performance numbers in the existing reports describe earlier PR snapshots, not a fresh performance measurement of this implementation. Browser geometry and exported pixels establish functional behavior, not measured customer usability.
