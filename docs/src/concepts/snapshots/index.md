# Snapshots

A snapshot is a reusable checkpoint of a sandbox. It preserves the sandbox's
filesystem and runtime state so that you can later start a new sandbox from the
same point instead of rebuilding the environment and rerunning setup work.

- **Templates** are stored as snapshots. A template build commits one snapshot;
  the template ID is an alias that resolves to it.
- **Warm-started sandboxes** restore the state of a committed template or snapshot.
- **Running sandboxes** can produce new snapshots, capturing their current
  state for later reuse or branching.

## What a Snapshot Preserves

A snapshot preserves:

- VM state and memory, allowing running processes to continue from the captured point.
- The root filesystem and its changes.
- Attached-drive contents and attachment metadata.
- Mounted-volume contents, mount paths, sizes, and access modes. Starting from
  the snapshot creates new volumes from the captured contents rather than
  reusing the source volume identities.
- CPU, memory, and disk settings, together with command context such as
  environment variables, working directory, user, and startup commands.

## Storage Backends

`snapshot.repository_backend` selects where committed snapshots live:

- `posix_fs` — a shared filesystem.
- `oss` — an S3-compatible object store (for example Alibaba OSS, MinIO, or a
  site-local RustFS). The `[backend.oss]` section configures the endpoint,
  bucket, prefix, region, and credentials.

An `oss` backend may declare an optional read-only mirror in
`[backend.oss.fallback]`. The mirror is expected to hold the same keys, for
example through asynchronous bucket replication from a site-local primary to a
durable remote service. When the primary is unreachable, reads (including
metadata, catalogs, and managed layer blobs) are served from the mirror so a
node can keep starting and resuming sandboxes read-only; writes and deletes
always go to the primary. Immutable content-addressed layer blobs may also fall
back to the mirror when the primary does not have them, while mutable records
(snapshot records, volume records, aliases, and heads) never do, so a stale
mirror cannot resurrect deleted state. See
[`[backend.oss]`](../configuration/reference.md#backendoss) in the
configuration reference for the full semantics and configuration keys.

---

Where to Go Next:

- [Create a Snapshot](./create.md) — capture a reusable checkpoint from a running sandbox.
- [Manage Snapshots](./manage.md) — list, inspect, and delete snapshots.
- [Rootfs as an OCI Image](./use.md) — publish or export only the captured root filesystem.
- [Optional P2P Visibility](./p2p.md) — accelerate committed snapshot artifact distribution between nodes.
