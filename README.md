<p align="center">
  <img src="https://raw.githubusercontent.com/canvas-ai/.github/main/banners/canvas-banner_1200x480.jpg" alt="Canvas" width="100%" />
</p>

# canvas-fuse

FUSE-based Canvas mount - materializes context views and workspace trees as live
folders. A universal helper: usable by users from a shell, by agent containers,
and as a sidecar/library by other apps (canvas desktop UI).

One mount is one workspace — contexts are addressed inside it, never across
workspaces. Point it with a selector before the mountpoint (or with `-w`/`-c`).

**A workspace** (`canvas-fuse mount <workspace> <mountpoint>`) — the same shape
the WebDAV view serves (see `docs/data-representation.md` in canvas-server):

```
<mountpoint>/<workspace-name>/
├── Home/                       # the workspace drive, 1:1, listed on demand
│   └── <your files>
├── Trees/
│   └── <tree-name>/
│       ├── <folder>/
│       │   ├── <subfolder>/
│       │   │   └── document.md
│       │   └── document.md
│       └── document.md
└── Trash/                      # flat; what a delete parked here
    └── document.md
```

**Contexts** (`<workspace>/Contexts`) — materializes that workspace's context
views; `<workspace>/Contexts/<id>` roots the mount at one of them.

A context is FLAT: its documents are its files. Grouping by schema is a derived,
read-only `.by-schema/`, so nothing about a file changes depending on which
folder you drop it in.

```
<mountpoint>/
└── Contexts/
    └── <context-id>/
        ├── .context.json          # context metadata incl. current url
        ├── reddit.url             # data/schema/tab
        ├── notes.md               # a file, like anywhere else
        ├── report.pdf             # blob content, lazy-fetched
        └── .by-schema/            # derived, read-only
            ├── Tabs/  Notes/  Files/  Emails/  …
```

Because the tabs of a context-bound browser are just files here, a file manager
can drive it: `rm reddit.url` closes that tab, writing a `.url` opens one, and
editing one navigates it.

Context folder contents are a function of the context's current URL. When the
URL is switched - by a browser bound to the context, the CLI, an agent,
anything - the view updates in place within ~1s: the daemon subscribes to the
canvas-server socket.io bridge (`context.url.set`, `document.*`) and pushes
kernel invalidations via FUSE reverse notification. A periodic full resync
(default 30 s) covers missed events and discovers new contexts. Workspace
mounts reconcile the same way over the `workspace:<id>` channel.

`Home/` is the workspace's file drive, passed straight through: real files, no
document layer. Directories are listed the first time something looks into them
(a home drive can be enormous, so it is never walked at mount), reads take a
byte window, and writes replace the whole file on close — the same shape the
document write path already uses.

### What the filesystem verbs mean

The rules live server-side, so this mount and WebDAV agree by construction:

- **`rm` detaches** the document from that folder — it survives in the store and
  in every other path it is filed into. If that was its LAST placement, the
  server files it into `Trash/` rather than letting it become reachable only
  through the flat workspace-wide list. Nothing a mount does deletes.
- **`mv` re-tags.** Moving a file between folders (or trees) links it at the
  destination and unlinks it at the source — two small requests, no bytes
  through the mount, so a 4GB blob moves as fast as a note. Moving a folder is a
  tree operation: the documents filed under it come along.
- **Deleting from a context** only detaches it from that view; a context is a
  view, not a place.
- **Writing a file** stores its bytes: `.todo.json` and `.url` keep their canvas
  meaning, everything else — markdown included — is a file (its bytes go to the
  workspace blob store, and the document references them). Saving over a
  document that already exists updates it in its own schema, so editing a note
  edits the note.
- **Under `Home/` the rules are the filesystem's own**: `rm` deletes the file,
  `mkdir`/`rmdir` create and remove real directories. No trash, no detach — the
  workspace trash is for documents, and your file manager's own warning is the
  safety net.

## Install

Prebuilt binaries are attached to each [GitHub Release](../../releases) (tag
`v*`). Linux: `x86_64`/`aarch64` glibc, plus a fully static `x86_64` musl build
that runs on any distro (only the `fusermount3` helper is needed at mount time).
Linux only - `fuser`'s pure-Rust backend (no libfuse) is not supported on macOS
or Windows. Build from source with `cargo build --release` (no `libfuse` dev
package needed).

## CLI

```sh
canvas-fuse mount universe ~/MyWorkspace                    # the workspace: Trees/ + Trash/
canvas-fuse mount universe/Contexts ~/ctx                  # that workspace's context views
canvas-fuse mount universe/Contexts/foo ~/MyFooContext     # one context, rooted
canvas-fuse mount -d universe ~/MyWorkspace                # detached; logs to the state dir
canvas-fuse mount -w universe ~/wrk                        # flag form of the first
canvas-fuse mount -c universe/foo ~/ctx/foo                # flag form of the third
canvas-fuse unmount ~/MyWorkspace                           # SIGTERM daemon, escalates if needed
canvas-fuse status [--json]                                # known mounts + health (ok/orphaned/...)
canvas-fuse ping [--json]                                   # server reachability, version, auth check
canvas-fuse contexts [--json]                               # list accessible contexts
```

### mount flags

| Flag | Default | Description |
|------|---------|-------------|
| `<selector>` | - | Positional, before the mountpoint: `<workspace>`, `<workspace>/Contexts`, or `<workspace>/Contexts/<id>`. |
| `-c/--context <id>` | - | A context view as `<workspace>/<id>` (or a bare id alongside `-w`). Repeatable; a single one roots the mount at it. |
| `-w/--workspace <name>` | - | The workspace to mount, at `<mountpoint>/<name>/`. With `-c` it scopes the context mount instead. |
| `--root <selector>` | - | The selector as a flag, for when it comes from config or a script. |
| `-d/--detach` | false | Daemonize after pre-flight; logs written to the state dir. |
| `--no-ws` | false | Disable the websocket event bridge (poll-only mode). |
| `--resync <secs>` | 30 | Full resync interval in seconds. |
| `--data-dir <path>` | `~/.canvas/<remote>/fuse/…` | Override the per-mount state directory (sticky filename map). Also `CANVAS_FUSE_DATA_DIR`. |
| `--blob-cache-mb <n>` | 256 | In-memory cache budget for file content. |

### Connection resolution

All commands resolve server/token in this order, so they work flag-less on any
machine where canvas-cli is logged in:

1. `--server` / `--token` flags
2. `CANVAS_SERVER` / `CANVAS_API_TOKEN` env vars
3. `--remote <name>` from `~/.canvas/config/remotes.json`
4. `boundRemote` from `~/.canvas/config/cli-session.json`

Agent containers typically use env vars + one context:

```sh
CANVAS_SERVER=https://canvas.example CANVAS_API_TOKEN=canvas-... \
  canvas-fuse mount -d universe/Contexts/mbag /workspace/context
```

Requires `fusermount3` (present on any desktop distro; `fuse3` package in
containers). No libfuse linkage - `fuser` is built with
`default-features = false` and speaks the kernel protocol directly, so the
binary is self-contained.

## Design notes

- **File blobs are real files, shown as-is.** A file doc's name comes from its
  location URL basename (real extension preserved, so players/editors open it),
  size from `metadata.size`. When the doc carries no size, the file is still
  shown as-is and the size is resolved lazily from the blob on first `stat`
  (cached thereafter) - never a `.json` stub. Bytes are fetched lazily through
  `GET /workspaces/:id/documents/:docId/content` (the server resolves
  `stored://` / `file://{WORKSPACE_ROOT}` locations) on first read, via a fetch
  pool - the FUSE session loop never blocks on the network, concurrent kernel
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
  (docId order); the map is per-device - lifting it to server-side
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
  listing may need a nudge - a desktop app can deliver one from the same ws
  events. Entries *entering* a view are never push-notified (no FUSE create
  notification exists); they appear on the next readdir.
- **Daemon lifecycle.** `mount -d` daemonizes after pre-flight (so config and
  connectivity errors still reach the terminal), writes a state file under
  `~/.local/state/canvas-fuse/mounts/`, and exits hard on SIGTERM after
  unmounting - rust_socketio's reconnect thread otherwise outlives
  `disconnect()` and would pin the process. `unmount`/`status` operate on the
  state files; stale entries from crashes are cleaned up automatically and
  stale kernel mounts are recovered with `fusermount3 -uz` on the next mount.

## Embedding

`canvas_fuse::mount(MountOptions) -> MountHandle` - dropping the handle (or
calling `unmount()`) tears down ws client, threads, and the kernel mount.
`MountOptions.contexts` filters which contexts are materialized.

## Tests

`cargo test` covers the view diff logic: skeleton stability, sticky collision
names, inode stability across URL switches, content invalidation, context
removal.

## Write path

**Context mode (Notes/, Todos/):** create/edit/rename/delete markdown files;
daemon maps to document operations (notes: file = `data.content`, title
untouched; todos: `- [x] title` + description body round-trips). Verified
against real editor save patterns: in-place truncate+write (Obsidian, VS Code),
append, atomic tmp+rename (sed -i, vim), mid-edit stat, touch, mv, rm.

**Workspace mode:** full read/write over the workspace's tree hierarchy.

- `mkdir` → creates a tree path (folder) on the server.
- `rmdir` → removes the path (non-recursive; `rm -r` still works via POSIX
  layer: files unlinked first, then empty dirs removed bottom-up).
- create file → new document inserted into the tree at that path.
- edit/save → document content updated; synapsd mints a new doc id (content-
  addressed versioning) and the daemon rebinds the inode transparently.
- `mv` within a dir → document filename rename. `mv` across dirs → tree path
  move (folder) or EXDEV (file; `mv` falls back to copy+unlink).
- `rm` → document detached from the tree (never destroys user data; only
  transient tmp files created by the mount itself are hard-deleted).

Common to both modes:

- Writes are buffered per open file and flushed on close (flush/fsync/release);
  close-time errors reach the application. Requires a device or JWT token.
- Flush chains serialize against refresh cycles (shared lock) and in-flight
  entries are frozen out of view diffs, so server-driven refreshes never drop
  or rename a file mid-save.
- Obsidian: point the vault at a local dir and symlink
  `Contexts/<id>/Notes` into it, or point a workspace mount directly (Obsidian
  needs a writable vault root for `.obsidian/`; keep it outside the mount).

## Not yet

- Editing file blobs (`Files/` is read-only)
- Global `Workspaces/` umbrella in the all-contexts mount (only rooted single `-w` is supported)
- Eager per-path document refresh (workspace mode issues one request per tree path; fine for wiki scale, optimize later)

## Licence

Copyright (C) 2026 Jozef Melich.

Canvas FUSE is licensed under the **[AGPL-3.0-or-later](LICENSE)** and under no
other terms. No commercial exemption is offered for this component, to anyone.
The Canvas clients stay free software in all cases.

Contributing needs no CLA here, only a DCO sign-off (`git commit -s`). See
[CONTRIBUTING.md](CONTRIBUTING.md). The dual-licensed Canvas components are
listed in [NOTICE](NOTICE).
