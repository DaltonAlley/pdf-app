# PDF Tools

PDF Tools is a self-hosted web app with a Rust backend and Leptos CSR frontend. It
turns PDF pages into PNG or JPEG images, extracts selected pages as individual
PDFs in a ZIP, merges PDFs, converts ordered PNG or JPEG images into a PDF, and
imposes artwork onto print sheets.

The print-sheet workflow supports repeated or unique pages, simplex and duplex
layouts, automatic placement, previews, reusable presets, and print-ready
PDF exports.

Images to PDF keeps this workflow deliberately simple: each ordered PNG or JPEG
becomes one PDF page at its natural 300 DPI size. Page resizing, orientation,
fitting, and margins belong to other workflows.

## Make customer artwork into a print-ready flyer

Choose **Impose artwork**, then explicitly set the **Finished width** and
**Finished height** the customer wants. These fields are always editable. PDFs
and images both require an explicit choice; source dimensions never establish
the intended product size.

- **Fit** preserves proportions and content, with white borders
  where the source and finished shapes differ.
- **Fill** preserves proportions and crops to the chosen frame.
  Drag the artwork or use the crop-position sliders to choose what remains.
  **Reset crop position** centers it again.
- **Stretch** fills the finished size by scaling width and height independently.
  It keeps the artwork but intentionally distorts its proportions. Crop controls
  apply only to Fill.
- **Scale to add bleed** enlarges the fitted artwork uniformly. The normalized
  crop position is retained relative to the bleed frame, but additional content
  can fall outside the finished cut. Review the cut boundary before downloading.
  Fill and Stretch cover the bleed frame; Fit can retain white borders. No mode
  invents missing edge content.
- **Finished orientation** changes the product between Portrait and Landscape.
  **Impression orientation** in the left setup column rotates its placement on the
  sheet: Auto, Upright, or Quarter-turn. These are separate decisions.

Use **Presets** to save an impose setup you expect to use again. The
starter **5x7 on 12x18** preset is available without being applied automatically.
Applying a preset fills the finished size, sheet, fitting, bleed, arrangement,
and printing choices without replacing the uploaded artwork or its page
quantities. Presets can be created, renamed, updated, applied, or deleted.

Artwork is a workspace-level global context exposed through the compact
**Artwork** toolbar beside the sheet preview, not a separate Setup step. **Fit**,
**Fill**, **Stretch**, and impression orientation stay available as you move
through the four-step Size, Quantity & sheet, Arrangement, and Bleed walkthrough. Open the
Artwork disclosure when you need fitting controls, crop positioning, per-artwork
overrides, or source and bleed information. On narrow screens, the toolbar stays
outside the guided Setup rail while **Preview** remains focused on inspecting the
sheet.

Mixed-size PDF pages and images can share one manually defined finished size.
Mixed pieces use regular slots large enough for the largest piece, with
individual cut guides, not an optimized nesting algorithm. Select artwork to
adjust its fitting or manually override its finished size independently. Duplex
pairs must have matching finished cuts, and odd source counts cannot be paired
automatically. In **Edit copy quantities**, **Apply to all** commits the entered
quantity to every page or pair and closes the dialog.

## Quick start with Docker Compose

Install Docker with the Compose plugin. Published images for Linux amd64 and
arm64 are available at [GHCR](https://github.com/DaltonAlley/pdf-app/pkgs/container/pdf-app).
Download `compose.release.yml` and `SHA256SUMS` from a
[GitHub release](https://github.com/DaltonAlley/pdf-app/releases) into an empty
deployment directory, then run:

```sh
sha256sum --check SHA256SUMS
docker compose -f compose.release.yml up -d --wait
```

No source checkout or registry login is required for the public package. The
release attachment pins that release's immutable image digest. If using the Compose file
from the repository instead, `PDF_APP_VERSION` defaults to `0.2.11` and can be set
in `.env` to select another published version.

To build locally instead, clone <https://github.com/DaltonAlley/pdf-app>, enter
its repository root, and run:

```sh
docker compose up -d --build --wait
```

Open <http://localhost:3000>. Compose waits for the app's built-in health check
before returning. The supplied deployment is available only from the Docker
host because the app has no built-in authentication.

Compose stores persistent application data in `./data` beside the Compose file
and mounts it at `/app/data` in the container. This preserves impose presets,
recent jobs, and export history when the container is replaced.
Impose uploads are staged in that mounted data directory so large artwork uses
the host volume's capacity instead of the container's smaller temporary layer.
Temporary background-job downloads do not survive a restart. Back up `./data`
with the service stopped if that information matters to you.

## Optional configuration

The defaults require no configuration. To override them, create `.env` beside
`compose.release.yml` (or `docker-compose.yml` for source builds). The
repository's `.env.example` lists the available settings:

| Variable | Default | Purpose |
| --- | --- | --- |
| `HOST_PORT` | `3000` | Port published on the Docker host |
| `PDF_TOOLS_DATA_PATH` | `./data` | Persistent host directory mounted at `/app/data` |
| `PUID` / `PGID` | `1000` | Host IDs used to own persistent files |
| `MAX_UPLOAD_MB` | unset | Optional limit for generic multipart operations; canonical impose intake has separate safeguards |
| `PDF_TOOLS_MAX_IMPOSE_UPLOAD_MB` | `512` | Total compressed artwork bytes accepted by one canonical impose upload |
| `PDF_TOOLS_IMPOSE_UPLOAD_TIMEOUT_SECONDS` | `120` | Maximum elapsed time spent reading one canonical impose upload |
| `MAX_RENDER_PAGES` | unset | Optional page limit for render and split requests |
| `MAX_DOWNLOAD_MB` | unset | Optional generated-download size limit |
| `PDF_TOOLS_CPU_PERMITS` | up to 4 host CPUs | Shared workers for PDF and image processing |
| `PDF_TOOLS_MAX_ACTIVE_OPERATIONS` | CPU permit count | Concurrent upload and PDF operations |
| `RUST_LOG` | `info` | Server log filter |

Set `PUID` and `PGID` to the output of `id -u` and `id -g` if the deployment
account does not use ID `1000`.

When `MAX_UPLOAD_MB` is unset, generic uploads have no application-wide byte
limit. Canonical impose sources remain disk-streamed and are independently
bounded by `PDF_TOOLS_MAX_IMPOSE_UPLOAD_MB` and
`PDF_TOOLS_IMPOSE_UPLOAD_TIMEOUT_SECONDS`. Setting `MAX_UPLOAD_MB` limits
conversion, merge, split, and legacy job multipart endpoints; it does not
replace the canonical `/gang-up/sources` safeguards.

PDFium calls run on bounded blocking workers and are serialized because the
native library is process-global. Cancellation and deadlines are checked at
safe upload and page boundaries; a native call already in progress cannot be
forcibly interrupted. Preview requests are intentionally small (at most four
pages per batch), and request traces provide the source route and response
latency needed to identify repeated or slow renders without retaining another
in-memory copy of source PDFs.

## Upgrade or roll back

For released images, download the new release's Compose file and checksums,
verify them, then pull and replace the app:

```sh
sha256sum --check SHA256SUMS
docker compose -f compose.release.yml pull
docker compose -f compose.release.yml up -d --wait
```

To roll back, use the previous release's Compose file and repeat the commands.
For local source builds, update the checkout and run
`docker compose up -d --build --wait` instead.

The replacement briefly interrupts requests; the `./data:/app/data` mount
preserves persistent application data. Reload open browser tabs after the
health check succeeds. Back up data before upgrades if rollback is important.

## Publishing a release

The standalone repository runs `.github/workflows/release.yml` on `v*` tags or
manual dispatch. The tag must equal `v` plus the version in `Cargo.toml`; manual
dispatch takes the exact version without `v` and releases the selected ref.
Both paths build the existing Dockerfile for Linux amd64 and arm64, publish
`ghcr.io/daltonalley/pdf-app:<version>` using `GITHUB_TOKEN`, and smoke-test both
architectures by immutable image digest. Only after health and versioned frontend
asset checks pass does the workflow create a GitHub release with a digest-pinned
Compose file and `SHA256SUMS`. Images can exist even if a later smoke check fails.
An existing release is rejected before building or publishing an image.

Repository administrators must allow Actions to write contents and packages.
After the first publish, set the linked GHCR package's visibility to **Public**
in its package settings. New GHCR packages are private by default, even for a
public repository. The OCI source label links the package to this repository.

## Build from source

The source-build command above runs from the standalone repository root and
requires only Docker. For the pinned development shell and full checks, run from
that same repository root:

```sh
nix develop .#pdf-app
nu scripts/check.nu pdf-app
```

Both deployments use the same port, optional `.env`, and persistent `./data`.
Stop the source-built service with `docker compose down` before switching to the
released-image deployment on the same port.

## Local development

PDF Tools pins Rust 1.97.1, Leptos 0.8.20, the
`wasm32-unknown-unknown` target, cargo-leptos 0.3.7, cargo-nextest 0.9.100,
and just 1.58.0. Install the Rust toolchain and project tools once:

```sh
rustup toolchain install 1.97.1 --component clippy,rustfmt --target wasm32-unknown-unknown
cargo install cargo-leptos --version 0.3.7 --locked
cargo install cargo-nextest --version 0.9.100 --locked
cargo install just --version 1.58.0 --locked
```

Ensure Cargo's binary directory is on `PATH` before running the project checks.

Start the complete local application from the repository root:

```sh
just dev
```

If PDFium is not on the system library path, set
`PDF_TOOLS_PDFIUM_PATH=/path/to/libpdfium.so`. The Docker image already bundles
PDFium. The supervisor builds and starts the backend and cargo-leptos frontend
watcher, verifies the application route, and then prints the ready URL. It owns and
stops only the two processes it starts. Local services bind to loopback because
PDF Tools has no built-in authentication. A directly run server also defaults
to `127.0.0.1`; set `PDF_TOOLS_BIND_ADDRESS` to another IP address only when an
intentional trusted-network bind is required. The container explicitly uses
`0.0.0.0` internally while Compose keeps its host-side port on loopback.

Build the optimized frontend into the exact directory served by Axum with:

```sh
just release
```

The frontend release script builds with Wasm splitting and versioned asset
names, resolves those names in the static HTML entrypoint, and generates gzip
sidecars. The upload screen loads independently; generic PDF tools and imposition
load when opened. A failed workflow download keeps the selected files available
and offers Retry. Deploy the complete `frontend/dist` directory together so the
bootstrap, shared code, and workflow modules remain compatible.
Retain previous versioned asset directories if old tabs must continue opening
unloaded workflows across updates. The standard replacement container does not
retain prior bundles. If Retry cannot recover an old tab, its loading error offers
**Reload and clear workspace** and explicitly warns that files and settings must
be selected again. Reload never deletes the original files on disk.

Run the full project checks from the repository root:

```sh
just check
```

Tests target three boundaries: model tests cover layout, page selection, queue
ordering and lifecycle rules; API tests process real PDFs and validate failures
at the HTTP boundary; browser smoke checks cover actual input, focus, recovery,
and downloads. Keep copy-only assertions and helpers used only by tests out of
the suite; a queue-ID string assertion cannot prove that keyboard focus survives
reordering.

With the app running at `http://127.0.0.1:3200`, run from the PDF project shell:

```nu
nu scripts/workflow-browser-smoke.nu
```

The full project check runs this smoke and the startup, inspection, transition,
and mixed-artwork suites against its isolated release server.
This self-contained smoke creates PDF fixtures in the browser and exercises
mixed-size extraction, shortened-source range recovery, incomplete imposition
inputs, repeat export, modal focus, repeated keyboard reordering, merge, and
half-screen layout, upload failure/retry, cancellation, raster output, and fixed-page
image conversion. It observes real downloads, reads back the extracted PDF,
and verifies the image PDF's Letter landscape page dimensions.
The mixed-artwork smoke takes an odd-ratio image through Letter fitting, crop
positioning, bleed and PDF download, then rasterizes the actual downloaded PDF
through the public API and checks colored landmarks and border pixels. It also
checks mixed PDF sizes and selected-piece overrides. Deterministic labeled PDFs
are generated by `scripts/smoke-fixtures.nu`; no customer files are required.

Container packaging is checked separately because it requires a Docker engine:

```nu
nu scripts/container-smoke.nu
```

This builds a local validation image and starts only a disposable, loopback-bound
container. It checks health, compressed assets, real workflow exports, and an
old tab's recovery after a deployed module is physically removed and restored.
It does not replace Compose services or mount persistent shop data. Use
`--skip-build --image <image>` to check an already-built image.
