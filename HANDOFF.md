# Maintainer notes

Build and test in a local checkout. Collection paths, SSH accounts, helper commands,
OCR engines and models belong in runtime configuration, outside this repository.
See [installation](docs/installation.md), [concurrent edits and deployment](docs/concurrency.md)
and [release preparation](docs/releasing.md).

## Components and versions

The CLI, desktop GUI and inference helper are separate workspace packages.
All three support `--version` without configuration, services, a display or models.
Install matching versions together and update file-server helpers with clients.
Restart running processes after upgrading; replacing an executable does not update them.

`memetag serve` provides a browser grid with search suggestions, clipboard copying
and file sharing. It refreshes sources on opening and builds thumbnails on demand.
Test it against a disposable collection, including `/`, `/suggest` and `/png` routes.
Phone clipboard and file sharing depend on browser support; see installation.md.

## Data and writes

Tags and embedded OCR live in files. SQLite and thumbnails are local caches.
Inference vectors and proposal decisions live in SQLite. Back up a live index with
SQLite's backup API or `.backup`, rather than copying it during WAL writes.
Keep source IDs when transferring a cache between clients.

Media writes preserve inode and timestamps, verify media payloads, reject stale
revisions and journal interrupted operations. Recovery skips active locked entries.
Run recovery on the machine that owns the journal. Never delete journals during
an upgrade or write directly through a network mount.

Vocabulary saves reject stale drafts; server sync uses compare-and-swap under a lock.
Content IDs exclude XMP and cannot serve as full-file edit revisions.

## Isolated checks

Use temporary XDG config, data and cache directories and copied/generated media.
Run `cargo test --locked -p memetag` and the GUI tests with `--features gui`.
`tools/check-gui.sh` creates a private Xvfb display for interactive checks; it must
not send input to an existing desktop. Longer fuzz campaigns require a scratch
corpus and an external timeout; see the command reference.
