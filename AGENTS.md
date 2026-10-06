# Notes for maintainers and coding agents

memetag is a Rust workspace: `memetag` (CLI, workers), `memetag-gui` (egui browser),
`memetag-infer` (CLIP vectors). Build and test in a local checkout:

```sh
cargo build --locked --release -p memetag -p memetag-gui
cargo test --locked -p memetag --features gui
cargo fmt
```

Every executable answers `--version` without configuration, a display or models.
Run checks against a disposable collection under temporary XDG directories; never
against a live collection or through a network mount. `tools/check-gui.sh` opens a
private Xvfb display for GUI checks and must not send input to a real desktop.

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

## Documents

README.md for use, docs/installation.md for setup, docs/concurrency.md for the write
and sync rules, docs/releasing.md and PORTABILITY.md before a release.
