# Concurrent edits and deployment

Added 2026-09-26 after reproducing two lost updates: vocabulary upload replaced a newer
server copy, and a file write based on old bytes erased a completed tag write.

## Guarantees and boundaries

- Vocabulary direction still uses normalized rule fingerprints. Transport replacement
  additionally compares SHA-256 of the exact fetched bytes while holding a permanent
  `<canonical>.lock` on the server. Seed requires absence. A changed revision refuses
  the write; retrying sync recomputes direction or shows the conflict. An incomplete
  upload fails its own checksum before replacement. Requires `sh`, `flock`, `mktemp`,
  `sha256sum` and standard file utilities on the SSH server.
- Local sync and Save share a permanent `vocab.toml.lock`. They fail promptly when busy.
  Saves compare the original bytes with the current file under that lock. A stale draft
  stays unsaved: reopen the Implications card and reapply the intended change. Merge
  refuses unresolved/stale alias choices and commits the server before replacing local
  rules; an upload failure leaves local rules and base unchanged. Disk/connection errors
  are still possible between the server, local rules and base writes; rerun sync to
  reconcile them, rather than assuming the reported failure implies nothing happened.
- Media writes lock the media inode, reread and compare the original bytes, and only
  then create a journal entry and write. A stale caller must reload and retry. Recovery
  uses the same inode lock. Permanent journal-entry locks are in sibling `journal.locks`,
  outside the entries cleanup removes; legacy entry locks are also honoured.
- Metadata writes to a configured remote collection (including editor, CLI tag and OCR)
  run on its server through `_batch-worker`. A single-file request carries XMP plus
  SHA-256 of the full original and replacement. **Content IDs ignore XMP and must never
  be used as edit revisions.** The server verifies both revisions and runs the same
  journaled writer as a batch. It returns a verified report; media are not uploaded.
- `/proc/self/mountinfo` identifies SSHFS/NFS/CIFS/SMB3 mounts; direct writes there are
  refused. The same pathname on a native server filesystem works locally. Existing desktop
  journals pointing to network files are retained for deliberate server-side recovery,
  never automatically replayed through the mount. Keep their original bytes until
  resolved, with other writers stopped.
- Advisory locks coordinate cooperating memetag processes, not arbitrary external
  applications. Already-running older binaries cannot acquire the new protections.
  File reads/viewers still use the mount and can wait on a stalled mount.
- A completed background pull checks whether rules changed even if media pulling failed.
  The grid refreshes current rules, all indexed tag sets, completions and the search;
  it does not rely on an empty second `reimply` delta or an outdated worker snapshot.
  Refresh waits while the file editor/batch is open. Unsaved rule drafts remain guarded.

## Upgrade order

1. Run `cargo test --locked -p memetag --features gui` in the local clone. Build/test release locally;
   never build on the SSHFS mirror. Preserve the previous release for rollback.
   If the launcher symlinks into `target/release`, temporarily point it at that saved
   binary before building, then restore the link after helper deployment.
2. With old write helpers idle, stage the CLI release on each file server over SSH using
   its configured account. Replace the executable used by both helper commands.
   Retain backups and verify the commands configured in each network source.
   Old batch requests still work. Old helpers cannot execute the new single-file
   protocol, and clients do not fall back to an unsafe direct write.
3. Install matching CLI and GUI versions on other clients, and reopen existing windows. Avoid overlapping old and new writers
   while rolling out. Updating an executable does not replace a running process.
4. Verify versions/checksums and exercise writes only on disposable fixtures first.
   A code deployment does not require a full collection reindex or a live tag edit.
5. Rollback: restore the prior binaries on helpers and clients together, or revert the
   source commit and rebuild. Rules, media formats, SQLite schema and sync base remain
   compatible. Keep journals; permanent lock files can remain unused by older versions.

## Regression checks and investigation notes

`vocabsync::tests::concurrent_server_edit_survives_seed_push_and_merge` changes the
canonical after fetch. The old implementation reported success and lost that edit.
The shell transport test executes the actual upload script against a temporary file,
including stale revisions and truncated input. Local draft tests check both stale
rejection and repeated saves by one editor.

`writer::tests::stale_writers_and_separate_journals_cannot_erase_completed_tags` gives
two writers the same original, completes one, then attempts the other. It also holds
the inode lock while switching journal roots. The protocol test exercises manual OCR,
tags, animated media, timestamp/inode preservation, stale revisions and invalid paths.
The grid test simulates a rules-only pull after the worker already reimplied SQLite.

The debug headless conflict-card test must clear the end-pass texture deltas, as the
other headless GUI tests already do. Release-only tests hid its previous assertion.
During this fix, the first remote-write draft mistakenly used `byte_id` (which strips
XMP) for revisions; the stale-revision regression caught it before deployment. The
replacement explicitly uses SHA-256 of the entire file.

The first release test run also caught a short-lived busy lock on an immediate second
sync. A descriptor inherited by a concurrent subprocess can outlive its parent's close
until exec. Lock guards now explicitly `LOCK_UN` on drop; a duplicate-descriptor test
proves that another open-file reference cannot prolong a completed operation's lock.

## Fresh editor reads (2026-09-28)

The single-file editor reads the configured server directly over SSH when opening
an image and immediately before saving. This bypasses SSHFS data/attribute caches,
which can otherwise show missing tags for ten minutes after a successful remote
save. Failure to reach the server is an editor error; cached bytes are not used as
a fallback. The existing revision checks still reject intervening edits. Local
collections continue to read locally, and shared browsing/thumbnail cache settings
are unchanged. The read uses standard `cat` and needs no helper upgrade.

`editor_reads_server_bytes_despite_stale_mount_and_fails_closed` checks repeated
fresh reads with an unchanged stale local copy, shell-sensitive filenames, missing
server files, and invalid paths.

### Publication audit (Codex, 2026-09-28)

- Only editor load/save call the fresh reader; CLI tagging, batch operations, OCR,
  pull, verification, thumbnails, and query logic retain their existing routes.
- Both reads run on editor background threads. Each transfers the whole file;
  large videos and slow links can increase open/save latency. SSH connection and
  keepalive limits apply, and failures retain the draft for retry.
- The draft SHA-256 comparison and the server writer's locked revision check
  remain in place. A server change between either read and write is refused.
- Reads perform no writes or recovery on the server. Local collections use normal
  filesystem reads. Index updates still use verified replacement bytes after save.
- Display timestamps and inherited tags still come from filesystem/index metadata;
  this change guarantees fresh embedded tags/text, not fresh thumbnail caches.
- Regression coverage includes apostrophes, Unicode, newlines, shell metacharacters,
  invalid paths, missing server files, and partial output followed by SSH failure.
