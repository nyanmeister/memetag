# Network sources and extension plan

Implemented in 0.3.1: every network source has a permanent source ID, a mounted
folder on the desktop, and an SSH host, server folder and two helper commands.
The host can be any SSH-capable file server. Reusing a server copies its host and
commands; the new folder becomes the server root unless explicitly overridden.
It never inherits the previous collection's root by accident.

Bulk scans and metadata writes run against the server's filesystem. Workers use
relative paths; the client namespaces results with the source ID before importing
them into its local index. A failed source retains its cached rows while other
sources refresh independently. Batch edits group requests by source. Single-file
writes verify the expected revision on the server and enforce root containment.
Thumbnails, original viewing and copying still use mounted paths. This works with
SSHFS, SMB or NFS mounts when their file server also offers SSH and memetag helpers.

The shared vocabulary has one authority: the original `main` server. Additional
servers receive the client's vocabulary with scan/write requests; they never
pull or push competing global vocabularies. SSH authentication stays in the user's
SSH configuration and agent, outside the source registry. Helper commands are
trusted configuration, executed by the remote shell; paths and tag operations
travel as JSON rather than shell fragments.

## Shares without SSH: planned, not implemented

The next extension should introduce an explicit backend choice per source:
local filesystem, SSH worker, and a mounted share without a worker. Existing
`remote` descriptors can migrate into the SSH variant without changing source
IDs, `SOURCE_ID/relative/path` keys, scope rules, OCR, vectors or query syntax.
An adapter would own listing, authoritative reads, validated metadata writes and
availability reporting. The indexing and batch layers would consume those
operations rather than branch on protocol names. Introduce this abstraction with
the second network backend; a general plugin system is unnecessary now.

For an SMB/NFS-only share, start with explicit read-only indexing and bounded
scans on the mount. Such scans need cancellation and timeouts that actually stop
filesystem work, not just hide an unresponsive thread. Cache pruning must require
a completed, trustworthy listing; disconnects must never become empty collections.
Original reads also need a way to avoid stale client caches.

Writable support should follow evidence that the backend can preserve the current
guarantees: revision checks, serialized writes, journal recovery, media payloads,
timestamps and inode identity. Client-side mounted writes alone do not establish
those guarantees. Until verified, show the source as read-only and disable edits.
Protocol-specific credentials belong in a credential provider, not sources.toml.
Connection profiles could later let several sources share transport settings;
source identity and folder selection remain independent of those profiles.

Backend acceptance tests should cover disconnects during listing and writing,
stale mounted bytes, concurrent edits, interrupted journal recovery, identical
relative filenames across sources, and relocation without loss of cached OCR or
vectors. The current fake-SSH integration tests provide a reusable reference for
the worker backend.
