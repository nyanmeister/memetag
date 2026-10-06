# Release preparation

Memetag is licensed under GPL-3.0-or-later. Dependencies and optional models retain
their own licenses. All Cargo packages have `publish = false`; GitHub source hosting
does not publish them to crates.io.

The CI workflow builds/tests and uploads an Actions artifact. It does not publish
a GitHub release or install packages. `tools/package.sh` creates core, desktop or
full Linux archives without publishing or installing them.

## Source publication

Publish reviewed source with generic paths and configuration examples. Keep private
maintenance records, collection data, credentials, runtime configuration, caches,
models and build directories outside the public checkout. Review all tracked files,
including documentation, test fixtures, service units, packaging and workflow files.
A clean current tree does not remove private information from earlier commits.
When importing a private project, publish a fresh root commit from the reviewed
tracked tree and retain the original history separately. Do not push private tags,
branches or a mirror of the original repository. Publish subsequent changes from
that public history.

The default XMP namespace documented in PORTABILITY.md is a format identifier, not a
server configuration. Never change the default: existing metadata must remain readable.
A collection written under another namespace is handled by `legacy_xmp_namespaces`, not
by editing the constant.

## Native archives

Build/test the exact clean revision. Check each executable's `--version`, test a
native package in a disposable desktop, record library requirements and archive
checksums, and supply a verified source checksum in any published PKGBUILD.
The included PKGBUILD consumes a locally produced archive with `SKIP`; it is not
an AUR submission or a verified remote-download recipe. Rebuild on each supported
distribution baseline.

Local archives include dependency/font notices and the pinned ONNX Runtime 1.28.0
notices. Crates omitting license text have pinned upstream copies under
`packaging/licenses`, with source revisions recorded. Archive creation refuses
missing dependency notices. The full ONNX notices include optional providers;
listing a notice does not establish that its component is in the downloaded CPU
static library. Audit runtime build provenance before a public inference-binary
release. Models are downloaded separately and are never bundled.

The inference build script treats Cargo offline mode as a request to skip runtime
setup, including a cached runtime download. Leave CARGO_NET_OFFLINE unset for normal
inference builds. A fully offline build requires a prepared ONNX Runtime library
and ORT_LIB_PATH. Core and desktop builds do not need this runtime.

The desktop build omits inference. Existing image vectors support proposals;
generating missing vectors requires memetag-infer and the configured CLIP model.
An inference error leaves committed batches available for retry. Compare combined
shipping size; splitting executables can duplicate code.
