# Notes for maintainers and coding agents

memetag is a Rust workspace: `memetag` (CLI, workers), `memetag-gui` (egui browser),
`memetag-infer` (CLIP vectors). Build and test in a local checkout:

```sh
cargo build --locked --release -p memetag -p memetag-gui
cargo test --locked -p memetag --features gui
cargo fmt --all --check
```

The default desktop build excludes image inference. Build `--workspace` only when
the inference helper is needed; its ONNX Runtime setup may download binaries.
Keep Cargo.lock pinned. Run `cargo fmt --all` after editing Rust, and use
`cargo clippy --locked --workspace --all-targets` when checking the full workspace.

Every executable answers `--version` without configuration, a display or models.
Run checks against a disposable collection under temporary XDG directories; never
against a live collection or through a network mount. `tools/check-gui.sh` opens a
private Xvfb display for GUI checks and must not send input to a real desktop.

## Code map and data invariants

- `src/lib.rs`: configuration and CLI/GUI entry points; `gui/` enables the desktop
  feature, while `infer/` owns ONNX model loading and embedding generation.
- `src/xmp.rs`, `containers.rs`, `matroska.rs`: metadata merging and container
  handling. Preserve foreign properties and media payloads, including animation.
- `src/writer.rs`, `locking.rs`: verified in-place writes, interrupted-write
  journals and advisory locks. Preserve inode and timestamps; never remove journals
  during an upgrade or roll back a newer completed edit with an old journal.
- `src/sources.rs`, `library.rs`, `index.rs`: source identity, selected folders and
  SQLite caches. Index paths use `SOURCE_ID/relative/path`; keep IDs stable when
  renaming or relocating sources. Disabled/offline sources retain cached data.
- `src/batch.rs`, `pull.rs`, `vocabsync.rs`: server workers and synchronization.
  Scan/write network collections on their configured file server; do not bypass
  mount checks. Reject stale edits and stale vocabulary saves. Content IDs exclude
  XMP: use SHA-256 of the complete file for edit revisions, never a content ID.
- `src/grid.rs`, `editor.rs`, `library_ui.rs`: desktop views; `serve.rs`: the phone
  browser grid. Keep blocking filesystem/network work off desktop UI threads.

Tags and embedded OCR belong to the media; the index and thumbnails are caches.
Vectors and proposal decisions are index-only. Use SQLite's backup API or `.backup`
for a live index rather than copying a database while WAL writes are active.

## Phone webapp handling

- `src/serve.rs` renders the page; `src/web_actions.js` handles Copy, Share and Open.
  Modern clipboard writes start during the tap with a promised PNG. Share needs
  a fresh user activation after a slow download; retain the prepared file and offer
  a finish button. Older ClipboardItem implementations use the same fallback.
- Retry only read-only downloads, with bounded timeouts and complete-file checks.
  Never automatically retry clipboard writes, share sheets or `/share` commands.
  Cancelling a pending download must prevent a later copy. Do not report a native
  browser failure just because its permission dialog takes more than four seconds.
- Originals from configured network sources use `batch::read_for_view` over SSH,
  including legacy pull-only settings; avoid probing a stalled mount first.
  Editing keeps its matching-writer checks. Thumbnails remain local caches.
- `node tools/check-web.mjs` runs isolated headless Chromium checks with latency,
  dropped/stalled transfers and real user-activation expiry. It needs Node with
  built-in WebSocket and Chromium (`MEMETAG_TEST_BROWSER` selects its executable).
  Android clipboard/share handoffs are stubbed there; also test on the phone.
  `tests/serve_cli.rs` exercises real HTTP and fake SSH against disposable media.

## OCR handling

`src/ocr.rs` runs inference and tracks completion; `embed.rs` writes saved results;
`editor.rs` and `index.rs` handle reviewed text and search precedence.

- `memetag ocr` normally stores machine text in SQLite, not in the files.
  `ocr --embed` embeds newly inferred results. `embed-text` embeds existing nonempty
  results without inference; use `--dry-run` first. For bulk network embedding,
  transfer a coherent index backup and run on the file server (see the command reference).
- Human-reviewed `meme:text` with `meme:textSource=manual` wins over machine text,
  **including deliberately empty text**. Keep `manual_text` separate from machine
  results so a late OCR worker cannot overwrite a review. Respect this marker in
  inference, embedding and reindexing; empty reviewed text is not missing OCR.
- A tag-only save must not replace newer indexed machine text with an older caption
  embedded in the file. Preserve completion metadata and the editor's precedence.
- Completion is keyed by `tesseract` or `ollama:<model>` in `text_meta`. Changing the
  model triggers new work; changing only the prompt does not. Use `ocr --all` for
  an intentional rerun; human reviews remain protected. Keep engine/model/prompt
  and the Ollama URL configurable rather than assuming a particular machine.
- Each image commits separately; stopping finishes the image in flight and a rerun
  resumes. Preserve pull/OCR coordination. Test changes on disposable media with
  mocked engines, including empty reviews, late results and failed embedding retries;
  see editor tests and `tests/embed_cli.rs`.

## XMP namespace

memetag's own properties (`meme:id`, `meme:origMtime`, `meme:text`) are written under
`https://github.com/nyanmeister/memetag/ns/meme/1.0/` (`xmp::DEFAULT_MEME`). It is a
name, not an address; nothing fetches it. Do not change the default: every file
already tagged carries it, and a reader with a different string treats those
properties as foreign.

Users whose collections were written under another namespace configure it instead of
patching the source. In `config.toml`:

```toml
# what new writes use (default: the namespace above)
xmp_namespace = "https://github.com/nyanmeister/memetag/ns/meme/1.0/"
# namespaces earlier builds wrote: read as memetag's, moved to xmp_namespace on the next write
legacy_xmp_namespaces = ["https://old.example/ns/meme/1.0/"]
```

`MEMETAG_XMP_NAMESPACE` and `MEMETAG_LEGACY_XMP_NAMESPACES` (comma separated) override
these for one process. All programs that write a collection, including file-server
workers, need the same settings; `memetag doctor` prints the ones in effect.

Coordinate namespace settings across all writers before changing them. Clients
without a matching write or legacy namespace treat those properties as foreign.
`tests/xmp_namespace_cli.rs` exercises configuration precedence and migration;
run it when changing initialization or namespace handling.

## Checks and publication

Integration tests in `tests/` cover CLI writes, source migration, remote mappings,
namespace migration and bounded mutation fuzzing. Longer fuzz campaigns belong on
scratch copies with an external timeout; see docs/legacy-workflow.md. Clear inherited
MEMETAG_* overrides when setting up isolated tests so they cannot select real data.

Keep runtime configuration, credentials, media, models, build output and private
maintenance notes outside version control. Use fictional paths/hosts in examples.
Check every ref and historical object being published, not just the current tree;
never import private history into a cleaned public repository.

Keep private and public tracked source trees in step, including tests, dependencies,
examples and packaging. Fetch public updates before publishing, preserve direct
GitHub edits, and compare complete tree IDs after separately committing reviewed
content. Record the paired heads and any intentional differences privately; see
[docs/releasing.md](docs/releasing.md#keeping-private-and-public-source-in-step).
Source parity does not imply matching installed binaries or runtime configuration.

`tools/package.sh` stages native archives and checks executable versions, runtime
libraries and dependency notices. The CI workflow uploads build artifacts, not a
GitHub release. Neither packaging nor publication authorizes installation, service
activation or collection migration. Preserve upstream license notices.

## Documents

- [README.md](README.md): feature overview and quick start.
- [docs/installation.md](docs/installation.md): dependencies, components, sources,
  optional programs and phone setup.
- [docs/legacy-workflow.md](docs/legacy-workflow.md): detailed command/search/OCR
  reference despite its historical filename.
- [docs/concurrency.md](docs/concurrency.md): write/sync guarantees and upgrade order.
- [docs/network-sources.md](docs/network-sources.md): supported SSH sources and backend
  extension boundaries.
- [docs/releasing.md](docs/releasing.md) and [PORTABILITY.md](PORTABILITY.md): privacy,
  source history, packaging, licenses and platform constraints.
