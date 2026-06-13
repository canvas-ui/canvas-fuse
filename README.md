# canvas-fuse

FUSE-based Canvas context mount — materializes **context views** as live folders.
A universal helper: usable by users from a shell, by agent containers, and as a
sidecar/library by other apps (canvas desktop UI).

```
<mountpoint>/
└── Contexts/
    └── <context-id>/
        ├── .context.json          # context metadata incl. current url
        ├── Tabs/    *.url         # data/abstraction/tab
        ├── Notes/   *.md          # data/abstraction/note
        ├── Todos/   *.md          # data/abstraction/todo
        ├── Links/   *.url         # data/abstraction/link
        ├── Files/   real files     # data/abstraction/file — blob content, lazy-fetched
        ├── Emails/  *.json
        └── Other/   *.json        # any unmapped schema
```

Folder contents are a function of the context's current URL. When the URL is
switched — by a browser bound to the context, the CLI, an agent, anything —
the view updates in place within ~1s: the daemon subscribes to the
canvas-server socket.io bridge (`context.url.set`, `document.*`) and pushes
kernel invalidations via FUSE reverse notification. A periodic full resync
(default 30 s) covers missed events and discovers new contexts.

## Install

Prebuilt binaries are attached to each [GitHub Release](../../releases) (tag
`v*`). Linux: `x86_64`/`aarch64` glibc, plus a fully static `x86_64` musl build
that runs on any distro (only the `fusermount3` helper is needed at mount time).
Linux only — `fuser`'s pure-Rust backend (no libfuse) is not supported on macOS
or Windows. Build from source with `cargo build --release` (no `libfuse` dev
package needed).

## CLI

```sh
canvas-fuse mount ~/Canvas                 # foreground; ctrl-c unmounts
canvas-fuse mount -d ~/Canvas              # detached daemon, logs to state dir
canvas-fuse mount -d -c work ~/ctx/work    # only specific context(s)
canvas-fuse unmount ~/Canvas               # SIGTERM daemon, escalates if needed
canvas-fuse status [--json]                # known mounts + health (ok/orphaned/...)
canvas-fuse ping [--json]                  # server reachability, version, auth check
canvas-fuse contexts [--json]              # list accessible contexts
```

### Connection resolution

All commands resolve server/token in this order, so they work flag-less on any
machine where canvas-cli is logged in:

1. `--server` / `--token` flags
2. `CANVAS_SERVER` / `CANVAS_API_TOKEN` env vars
3. `--remote <name>` from `~/.canvas/config/remotes.json`
4. `boundRemote` from `~/.canvas/config/cli-session.json`

Agent containers typically use env vars + `-c <context>`:

```sh
CANVAS_SERVER=https://canvas.example CANVAS_API_TOKEN=canvas-... \
  canvas-fuse mount -d -c mbag /workspace/context
```

Requires `fusermount3` (present on any desktop distro; `fuse3` package in
containers). No libfuse linkage — `fuser` is built with
`default-features = false` and speaks the kernel protocol directly, so the
binary is self-contained.

## Design notes

- **File blobs are real files, shown as-is.** A file doc's name comes from its
  location URL basename (real extension preserved, so players/editors open it),
  size from `metadata.size`. When the doc carries no size, the file is still
  shown as-is and the size is resolved lazily from the blob on first `stat`
  (cached thereafter) — never a `.json` stub. Bytes are fetched lazily through
  `GET /workspaces/:id/documents/:docId/content` (the server resolves
  `stored://` / `file://{WORKSPACE_ROOT}` locations) on first read, via a fetch
  pool — the FUSE session loop never blocks on the network, concurrent kernel
  readahead of the same blob is deduplicated into one download, and blobs are
  cached in memory by checksum (LRU, `--blob-cache-mb`, default 256).
  `Files/` is read-only.

- **Hot path is local.** `lookup`/`readdir`/`getattr`/`read` are served from an
  in-memory tree behind a `parking_lot::RwLock`; the network is only touched by
  the refresh worker thread. Kernel TTLs are short (1 s) but correctness comes
  from explicit invalidation.
- **Sticky filenames.** Constructed names (`slug(title).ext`) are persisted in
  redb keyed by `(context, schemaDir, docId)`. Title collisions get a docId
  suffix (`Meeting.2.md`) and never silently swap back to the clean name, so
  external references (Obsidian links) stay valid. Assignment is deterministic
  (docId order); the map is per-device — lifting it to server-side
  document-in-context metadata is the planned path to cross-device identical
  names.
- **Inode stability.** A document keeps its inode across context URL switches
  within a context, so open file handles survive a view swap; documents that
  leave the view follow unlink semantics (open handles keep working, new
  lookups fail).
- **Invalidation semantics (tested on kernel 7.0):** `notify_delete`
  invalidates the dentry and emits `IN_DELETE_SELF` to watchers of the *file*,
  but no `IN_DELETE` reaches watchers of the *parent directory* (the fsnotify
  hook for FUSE reverse invalidation was lost in the ~5.3 refactor). Practical
  effect: `ls`/`cat`/agents always see fresh data with no manual refresh;
  editors watching files notice removals; file managers showing a directory
  listing may need a nudge — a desktop app can deliver one from the same ws
  events. Entries *entering* a view are never push-notified (no FUSE create
  notification exists); they appear on the next readdir.
- **Daemon lifecycle.** `mount -d` daemonizes after pre-flight (so config and
  connectivity errors still reach the terminal), writes a state file under
  `~/.local/state/canvas-fuse/mounts/`, and exits hard on SIGTERM after
  unmounting — rust_socketio's reconnect thread otherwise outlives
  `disconnect()` and would pin the process. `unmount`/`status` operate on the
  state files; stale entries from crashes are cleaned up automatically and
  stale kernel mounts are recovered with `fusermount3 -uz` on the next mount.

## Embedding

`canvas_fuse::mount(MountOptions) -> MountHandle` — dropping the handle (or
calling `unmount()`) tears down ws client, threads, and the kernel mount.
`MountOptions.contexts` filters which contexts are materialized.

## Tests

`cargo test` covers the view diff logic: skeleton stability, sticky collision
names, inode stability across URL switches, content invalidation, context
removal.

## Write path (Notes/, Todos/)

Notes and Todos are writable: create/edit/rename/delete markdown files and the
daemon maps them to document operations (notes: file = `data.content`, title
untouched; todos: `- [x] title` + description body round-trips). Verified
against real editor save patterns: in-place truncate+write (Obsidian, VS Code),
append, atomic tmp+rename (sed -i, vim), mid-edit stat, touch, mv, rm.

- Writes are buffered per open file and flushed on close (flush/fsync/release);
  close-time errors reach the application. Requires a device or JWT token
  (server `authenticateClient`).
- synapsd mints a NEW doc id on every content change (content-addressed
  versioning). The daemon rides this: rebinds the inode, re-pins the filename,
  and detaches the superseded version from the context — the folder always
  shows exactly the latest version; history stays in the DB.
- Flush chains serialize against refresh cycles (shared lock) and in-flight
  entries are frozen out of view diffs, so server-driven refreshes can never
  drop or rename a file mid-save.
- `rm` detaches the document from the context (never destroys user data);
  only transient docs the mount itself created (atomic-save tmp files) are
  destroyed. `mv` within a dir is a sticky-name rename; cross-dir returns
  EXDEV so `mv` falls back to copy+unlink.
- Obsidian: point the vault at a local dir and symlink
  `Contexts/<id>/Notes` into it (Obsidian needs a writable vault root for
  `.obsidian/`; the mount only accepts note files).

## Not yet

- mkdir = attach layer, rmdir = detach (tree-structure writes)
- Editing file blobs (Files/ is read-only)
- Workspace/directory-type tree mounts
