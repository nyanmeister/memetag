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

## Keeping private and public source in step

Keep the same features, dependencies, tests and generic configuration examples in
both trees. Machine-specific configuration and maintenance notes belong outside
the source tree. Private history stays private; different commit IDs are expected.
Do not add a private remote to the public checkout, fetch private objects into it,
or merge/cherry-pick private history for convenience.

Before an update:

1. Check both working trees for tracked changes and untracked source files. Fetch
   the public checkout's public remote and inspect new commits before integrating
   them; changes made directly on GitHub need to reach the private source too.
2. Review the changed files for private information, then transfer the reviewed
   content, including additions, removals and executable permissions. Commit each
   history separately. Do not copy whole working directories containing `.git`,
   local settings, caches, models or build output.
3. Compare the complete committed trees, not just `src/` or version numbers:

   ```sh
   git -C /path/to/private-checkout rev-parse 'HEAD^{tree}'
   git -C /path/to/public-checkout rev-parse 'HEAD^{tree}'
   ```

   With the same Git object format, matching tree IDs prove that all tracked
   paths, file contents and modes match, including Cargo.lock, tests, packaging,
   service units and documentation. This comparison does not import any history.
   If IDs differ, compare `git ls-tree -r HEAD` manifests and review each differing
   file; record any intentional difference rather than silently ignoring it.
4. Run checks appropriate to code changes on the reviewed source with identical
   feature selections and isolated test configuration. Documentation-only changes
   need link and whitespace checks. Equal source trees establish source parity;
   they do not establish that installed binaries or runtime configurations match.
5. Push the intended branches, then verify both remote heads and tree IDs. A
   rejected push requires inspecting and incorporating the newer remote work;
   do not force-push over it. Record the paired commit IDs, common tree ID,
   checks and deployment status in private maintenance notes.

Build revision strings will differ between the two histories even when their
source trees match. Keep collection namespace settings coordinated as described
in [AGENTS.md](../AGENTS.md#xmp-namespace); source synchronization alone does not
update existing installations or migrate their media.

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
