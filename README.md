<p align="center">
  <img src="https://raw.githubusercontent.com/canvas-ai/.github/main/banners/canvas-banner_1200x480.jpg" alt="Canvas" width="100%" />
</p>

# canvas-fuse

FUSE mount for a Canvas workspace: live folders for the home drive, trees,
trash, and context views. Usable from a shell, from an agent container, or as a
library (the desktop UI).

One mount is one workspace. Contexts are addressed inside it, never across
workspaces.

Linux only — `fuser`'s pure-Rust backend (no libfuse) does not exist on macOS
or Windows. Runtime dependency is `fusermount3` (`fuse3` package). The binary
itself does not link libfuse.

## Layout

`canvas-fuse mount <workspace> <mountpoint>` — same shape as the WebDAV view
(`docs/data-representation.md` in canvas-server). The kernel mount lands at
`<mountpoint>/<workspace>/`:

```
<mountpoint>/<workspace>/
├── Home/                       # workspace drive, 1:1, listed on demand
│   └── <your files>
├── Trees/
│   └── <tree-name>/
│       └── <folder>/document.md
└── Trash/                      # flat; what a delete parked here
    └── document.md
```

The `backends` tree is excluded by default: its connector and storage listings
(such as IMAP mailboxes) can be large. It is neither mounted nor fetched during
startup or refresh. `Home/` and `--mirror` still use the workspace home drive.
To include `Trees/backends/` in a full workspace mount:

```sh
canvas-fuse mount universe ~/MyWorkspace --include-backends
```

Use `--no-backends` to make exclusion explicit (also compatible with `--mirror`
and `--backend workspace:home`). It conflicts with `--include-backends` and
`--tree backends`.

A single context (`<workspace>/Contexts/<id>`, or `-c`) roots the mount at that
view. A context is **flat**: its documents are its files. `.by-schema/` is a
derived, read-only grouping — dropping a file into a schema folder does not
change what it is. Its folders are derived from the schema ids themselves, so a
schema this build has never heard of still gets its own folder rather than
falling into a catch-all.

Every document is rendered as the thing it already is, byte for byte the same as
the WebDAV mount serves it:

| Schema | File |
| --- | --- |
| `file` | the bytes themselves, under their own name — lazily fetched |
| `note` | `<title>.note.md`, the note's content verbatim |
| `tab`, `link` | `<title>.url`, a `[InternetShortcut]` body |
| `task` | `<title>.todo.json` |
| `message/email` | `<from address>-<subject>.eml`, RFC 822 — a mailbox slot like `INBOX;UID=56909` never names a file |
| anything else | `<schema>_<id>.json`, the record itself |

```
<mountpoint>/<context-id>/
├── .context.json               # metadata, including the current URL
├── reddit.url
├── notes.md
├── report.pdf                  # blob, lazy-fetched
└── .by-schema/                 # derived, read-only
    ├── Tabs/  Notes/  Files/  Emails/  …
```

Several `-c` flags materialize those contexts under `Contexts/<id>/` at the
mountpoint instead.

Context contents follow the context's current URL. A bound browser, the CLI, or
an agent switching the URL updates the folder in place within ~1s (socket.io
`context.url.set` / `document.*`, plus kernel invalidation). A full resync
(default 30s) covers missed events. Workspace mounts do the same on
`workspace:<id>`.

`Home/` is a passthrough drive: real files, no document layer. Directories are
listed the first time something looks into them (a home drive can be enormous),
reads take a byte window, writes replace the whole file on close. With
`--mirror` it is a real folder on disk instead, synced with the hub and still
there when nothing is mounted — see [Mirror mode](#mirror-mode---mirror) below.

Because context-bound browser tabs are just `.url` files, a file manager can
drive them: `rm reddit.url` closes the tab, writing a `.url` opens one, editing
one navigates it.

A folder in a context tree lists everything filed at **or below** its path, so
several documents can answer to one name in one folder. The document filed at
the folder you are standing in keeps the plain name; the ones showing through
from deeper paths take a `_<id>` suffix. Walking from `mbag://` to
`mbag://dc-migration` therefore hands `CLAUDE.md` to the document filed there —
the name follows the path, not the creation order.

Notes are `.note.md`. The compound suffix is what distinguishes a note from a
markdown FILE — a bare `.md` is a file, since markdown is a general format and
guessing would make "what does saving this mean" unanswerable. It still ends in
`.md` so a notes app pointed at the mount reads it as a note: Obsidian only
treats `.md` that way, and anything else is an attachment it will not render.

### What the verbs mean

Rules live server-side, so this mount and WebDAV agree:

- **`rm` detaches** a document from that folder. It stays in the store and in
  every other path it is filed into. If that was its last placement, the server
  files it into `Trash/` rather than dropping it from every view. Nothing a
  mount does destroys a document.
- **`mv` re-tags.** Link at the destination, unlink at the source — no bytes
  through the mount. A folder move takes its documents with it.
- **Deleting from a context** only detaches it from that view.
- **Writing a file** stores its bytes. `.note.md`, `.todo.json` and `.url` keep
  their canvas meaning; everything else (a bare `.md` included) is a file. Saving over an
  existing document updates it in its own schema. This holds in a context too:
  its bytes go to the backing workspace's blob store, addressed through the
  context (`POST /contexts/:id/blobs`).
- **Under `Home/`**: `rm` deletes the file, `mkdir`/`rmdir` are real
  directories. No trash, no detach.

Live views buffer writes per open file and flush on close. Close-time errors
reach the application. Flush serializes against refresh so a server-driven
resync cannot drop or rename a file mid-save. Blob documents (PDFs, images, …)
are readable and not writable. Mirrored Home files use the disk-backed write
path described below.

Editors that truncate-then-write (Obsidian, VS Code), atomic tmp+rename
(`sed -i`, vim), append, `touch`, `mv`, and `rm` are the supported save
patterns. Point an Obsidian vault at a local dir and symlink a context (or a
`Trees/` path) into it — Obsidian wants a writable vault root for `.obsidian/`,
keep that outside the mount.

## Mirror mode (`--mirror`)

`canvas-fuse mount -w <workspace> <mountpoint> --mirror` turns `Home/` from a
live passthrough into a **real folder on disk**, kept in sync with the hub's
`workspace:home` backend — Dropbox/iCloud style. `Trees/`, `Trash/` and
context mounts are unchanged. Wire contract: `docs/sync-protocol.md` in
canvas-server; design: `docs/sync.md`.

Only Home is mirrored onto disk; backend trees are virtual views and are excluded
by default. To make this explicit when mirroring:

```sh
canvas-fuse mount -w universe ~/Workspaces --mirror --no-backends
```

The folder is `<mountpoint>/<workspace>/Home` (the mountpoint itself for a
`--backend workspace:home` mount). While the daemon runs, the FUSE view sits
on top of it and serves the same files. When the daemon is down — or the
mount is gone, or the hub is unreachable — **the folder is simply there**:
every file at its path, editable with anything. That is the point: work
from a plane with `~/Workspaces/mbag/Home`, then mount again and let it
reconcile. The daemon reaches the real files through a directory handle it
opens before mounting, so the two never disagree about where bytes live.

What the daemon does:

- **Validate the destination (0.13.1+).** The first mount requires an empty
  destination and creates a local `.workspace.json` before scanning. For
  `-w Universe ~/Workspaces`, this is `~/Workspaces/Universe/.workspace.json`;
  the surrounding `~/Workspaces` directory may contain other workspaces.
  Subsequent mounts check the server URL, workspace UUID, backend, Home layout,
  physical directory identities and the replica ID bound to the sync database.
  A non-empty unmarked folder, mismatched/corrupt/copied marker, replaced Home,
  or unrelated database is refused before reconciliation. Marker loss while
  running pauses sync. The marker is local metadata and is never uploaded.
  Each laptop/workstation creates its own marker and database for the same
  remote workspace; multiple devices are supported. Do not copy markers or
  databases between replicas. A changed hub instance ID also pauses sync.
- **Full local mirror.** The first mount lists the hub and downloads
  everything (verified by digest, written atomically next to its target).
  Remote changes land in the folder within a second on a socket nudge
  (`backend.changed`), or on the poll (`--poll`, default 30 s), and carry the
  hub's mtime. Remote-only files become visible after their download; this
  mode does not expose placeholders or fetch missing files on open.
- **Local disk first (0.13+).** Creating, writing and truncating a Home file
  operate on its real backing inode immediately, with bounded memory instead
  of a whole-file RAM buffer. Open write handles follow local renames. Flush
  and fsync commit local bytes and a durable dirty intent; they do not wait
  for the hub. Close wakes the background uploader. Recovery scans discover
  files left behind by an interrupted copy or daemon exit.
- **Uploads are write-back.** The engine hashes closed files and uploads a
  temporary immutable disk snapshot with `If-Match` on the base version
  (`If-None-Match: *` for new files). A newer local save cannot change an
  in-flight request's body. Snapshots use temporary space under the mount's
  data directory and are removed after the request or on restart. `mv` and
  `rm` update disk and queue their upstream operation the same way.
- **Network waits do not hold Home locks.** Virtual tree refreshes have a
  separate lock; uploads, downloads and remote metadata requests release local
  namespace locks while waiting. A delayed download rechecks the file identity,
  open handles and queued moves before replacing anything. Upload results
  follow local renames and do not mark newer edits as synced.
- **Folder moves are one operation.** Renaming a synced folder sends one
  directory rename request. The local folder and the hub folder each use a
  native filesystem rename; file contents and inodes stay in place. The
  mirror re-keys its ledger and queued edits together, then refreshes metadata
  through a paged listing rather than querying every child. A persistent
  operation ID makes retries safe after a lost response or restart. Pending
  moves protect both paths from reconciliation; a refused move stays queued
  with its dependent writes. Further local renames and editor saves remain
  usable while the hub is stalled; the queue preserves their remote order.
  Use canvas-server 2.16.2 (or runtime-core 0.2.1), canvas-stored 1.9.6 and
  canvas-synapsd 3.23.5 together for the server folder/index consistency fixes.
  Older hubs may leave directory operations pending.
- **Local changes wake uploads through inotify.** Recursive watches follow
  the real backing directory even while FUSE covers it. Saves are observed
  on close-write; files and populated folders moved into Home are discovered
  too. Ordinary events inspect only the affected path or incoming subtree.
  Download staging files and the mirror's own atomic landings are suppressed.
  Fresh user work takes priority over offline discoveries and background
  repair, while rename/mkdir dependencies and retry backoff remain ordered.
  Uploads run before remote listing/downloads and between background requests;
  an already-running request finishes first. New folder names are published
  before their contents, and optimistic write preconditions still apply.
- **Recovery scans** run at every mount, on `sync now`, hourly, and after an
  inotify queue overflow. The normal disk scan frequency has not increased.
  Files edited, added or removed while no daemon was running are hashed
  and become pushes and deletes under the same rules as live edits. A
  touched-but-unchanged file costs one hash and nothing on the wire.
- **Conflicts never overwrite.** If a file changed here *and* on the hub
  since the last sync, the hub's version keeps the name and yours is saved
  next to it as `name (conflict from <device> <YYYY-MM-DD HHmm>).ext`
  (`--conflicts rename`, the default). Repeated conflicts in the same minute
  receive distinct names when their content differs; retries reuse the persisted
  copy name and digest. Use `--conflicts prompt` to send yours
  to the hub's conflict inbox and resolve in *Workspace settings › Sync* or
  the CLI instead. Existing configurations that explicitly select `prompt`
  keep that policy; change them to `rename` to use conflict copies.
  `canvas-fuse conflicts <mountpoint>` lists what this device recorded. A
  copy of your bytes is kept under the data dir (`conflicts/<sha256>`)
  until the conflict is resolved.
- **Hub deletes go to a local trash.** A file deleted on the hub leaves the
  folder but is kept for 30 days in `<data dir>/trash/<path>`:
  `canvas-fuse trash list|restore <mountpoint> [<key>]` (restore = push as a
  new file). An edit on one side beats a delete on the other, both ways.
  `--deletes keep` makes a local `rm` drop only the local copy.
  On the hub, deleting the final location removes the document from the index
  by default (server 2.16.0 / runtime-core 0.2.0). Workspace administrators can
  set `orphanPolicy: "keep"` using `PATCH /rest/v2/workspaces/:id/sync/settings`
  to retain its metadata and curation. Copies on other backends keep the
  document indexed under either policy.
- **Never uploaded:** dotfiles and everything the hub excludes (its
  `effectiveExclusions`, e.g. `node_modules/`), plus your own `--ignore
  <glob>`s. Such files stay in the folder and count as `skipped` in the
  status.

**Offline.** The mount stays up (it also *mounts* without the hub, once it
has seen the workspace once), and every file reads and writes normally —
there is no cache to miss. Reconnect (socket re-auth, the poll, or
`canvas-fuse sync now <mountpoint>`) uploads pending local work before
catching up on background changes. Files added while the daemon was stopped
are discovered by the startup scan; while it is running, inotify wakes it
without waiting for `--poll`.

**Identity and locations.** The device id is `deviceId` from
`~/.canvas/device.json` (canvas-cli) or a stable hash of hostname + user;
it is sent as `X-Canvas-Origin` so the mirror recognizes its own echoes.
State lives in the mount's data dir
(`~/.canvas/<remote>/fuse/workspaces/<ws>/mounts/<mount-path>.<hash>/`
or `--data-dir`): `mirror.redb` (index, base ledger, cursor, jobs,
conflicts, trash records), `trash/` and `conflicts/`. The bytes themselves
are only ever in `Home/`. Hub document ids are never stored — keys and
digests are the identity.

**Server recovery is separate from the mirror trash.** With the server's
default `CANVAS_RETENTION_DAYS=30`, displaced local-backend bytes are retained
under `<workspace-home>/.stored-tmp/retained/<sha256>`. They are outside the
indexed tree and do not appear in the mounted `Trash/`. The retention index
maps digests to original paths; identical contents can share one blob. Inspect
`GET /rest/v2/workspaces/<uuid>/backends/file/workspace%3Ahome/retained?limit=5000`.
Restore individual retained versions through that endpoint's
`/<sha256>/restore` route (POST with `key`; occupied paths are protected by
preconditions). Preserve both the retained bytes and Stored's metadata database
before incident recovery. Local mirror trash contains only local copies
preserved after hub deletions, so it is not a complete server recovery archive.

**Status and control.** `canvas-fuse status [--json]` shows a `mirror`
block per mirror mount (`state` idle|syncing|offline|paused, `home`,
`cursor`/`head`, `pending`, `failed`, `conflicts`, `skipped`, `entries`,
`lastSync`, `lastError`), read from a status file the daemon writes next
to its state file (`~/.local/state/canvas-fuse/mounts/*.status.json`).
The `sync`, `conflicts` and `trash` subcommands talk to the daemon over a
unix socket in the same directory (`<hash>.sock`; one JSON request, one
JSON reply). The daemon also reports to the hub
(`POST /workspaces/:id/mirrors/:deviceId/status`) so *Settings › Devices*
shows its lag.

```sh
canvas-fuse mount -w myws ~/Workspaces --mirror --conflicts rename
canvas-fuse status --json
canvas-fuse sync now ~/Workspaces/myws
canvas-fuse conflicts ~/Workspaces/myws
canvas-fuse trash list ~/Workspaces/myws
canvas-fuse unmount ~/Workspaces/myws      # ~/Workspaces/myws/Home stays, as files
```

**Upgrading an existing mirror to 0.13.1:** old folders and ledgers have no
verifiable local replica binding, so they are deliberately not adopted
automatically. Keep the old folder and its data directory (including queued
changes, conflict copies, trash and any old content cache) for review/recovery.
Start a new mirror at an empty destination with its own data directory, then
reconcile any outstanding local work explicitly. Do not manufacture a marker,
copy another device's marker, or reuse the old database to bypass this check.
`--pin` and `--cache-budget-mb` remain accepted and ignored.

Without `--mirror`, `Home/` keeps its passthrough behaviour. `mv` inside
`Home/` uses the hub's rename route for either a file or a whole directory.

## Install

Prebuilt binaries are attached to each [GitHub Release](../../releases) (tag
`v*`):

| Target | Notes |
| --- | --- |
| [`x86_64-unknown-linux-musl`](../../releases/latest/download/canvas-fuse-x86_64-unknown-linux-musl.tar.gz) | fully static — download-and-run on any distro |
| [`x86_64-unknown-linux-gnu`](../../releases/latest/download/canvas-fuse-x86_64-unknown-linux-gnu.tar.gz) | dynamic glibc |
| [`aarch64-unknown-linux-gnu`](../../releases/latest/download/canvas-fuse-aarch64-unknown-linux-gnu.tar.gz) | dynamic glibc, arm64 |

```sh
curl -L https://github.com/canvas-ui/canvas-fuse/releases/latest/download/canvas-fuse-x86_64-unknown-linux-musl.tar.gz \
  | tar xz --strip-components=1 -C ~/.local/bin canvas-fuse-x86_64-unknown-linux-musl/canvas-fuse
```

Install `fuse3` so `fusermount3` is on `$PATH`. That is the only runtime
dependency (OpenSSL is vendored into the binary).

## Build

Needs a Rust stable toolchain (edition 2021), a C compiler, and `perl` + `make`
— `rust_socketio` pulls native-tls, so OpenSSL is compiled from source into the
binary. No `libfuse-dev`. First build is slow; later ones are not.

```sh
# Debian / Ubuntu
sudo apt install fuse3 build-essential pkg-config perl make

git clone https://github.com/canvas-ui/canvas-fuse.git
cd canvas-fuse
cargo build --release --locked
install -Dm755 target/release/canvas-fuse ~/.local/bin/canvas-fuse
```

`cargo test --all` is the test suite (view diffs, sticky names, inode
stability, workspace/home materialization). The mirror suite covers two-device
convergence, offline changes, editor atomic saves, priority/inotify behavior,
1,000-file directory renames, conflict races, retry/restart, stale cursors,
corrupted/resumed downloads, permissions, trash failures and delete/edit races.
Run `cargo test --test mirror_real_server -- --ignored` with the sibling
canvas-server dependencies installed for the isolated real HTTP/database test;
`CANVAS_TEST_RUNTIME_CORE=1` selects the shared canvas-common runtime instead.
These engine tests do not mount FUSE. Mounted application workloads, disk-full
faults, forced termination and power-loss durability still need dedicated
release validation on a host with `/dev/fuse`. CI also runs `cargo fmt --check`
and `cargo clippy --all-targets -- -D warnings`.

### Cross targets

Same three targets the release workflow ships. `x86_64-unknown-linux-gnu`
builds with host `cargo`; musl and aarch64 go through
[`cross`](https://github.com/cross-rs/cross) so the per-target C toolchain
(and perl/make for vendored OpenSSL) live in the image — see `Cross.toml`.

```sh
cargo install cross --locked
cross build --release --locked --target x86_64-unknown-linux-musl
cross build --release --locked --target aarch64-unknown-linux-gnu
```

Binaries land at `target/<triple>/release/canvas-fuse`.

## CLI

```sh
canvas-fuse mount universe ~/MyWorkspace                 # Home/ + Trees/ + Trash/
canvas-fuse mount universe/Contexts/foo ~/MyFooContext   # one context, rooted
canvas-fuse mount -c universe/foo -c universe/bar ~/ctx  # those two under Contexts/
canvas-fuse mount -d universe ~/MyWorkspace              # detached; logs to the state dir
canvas-fuse mount -w universe ~/wrk                      # flag form of the first
canvas-fuse mount -c universe/foo ~/ctx/foo              # flag form of the second
canvas-fuse unmount ~/MyWorkspace                        # SIGTERM daemon, escalates if needed
canvas-fuse status [--json]                              # known mounts + health
canvas-fuse ping [--json]                                # server reachability, version, auth
canvas-fuse contexts [--json]                            # list accessible contexts
```

### Mount only selected sources

Use `--backend workspace:home` to mount just the home drive:

```sh
canvas-fuse mount -w universe --backend workspace:home ~/CanvasHome
# As a real, offline-capable folder (~/CanvasHome itself holds the files):
canvas-fuse mount -w universe --backend workspace:home ~/CanvasHome --mirror
```

The drive's files appear directly in `~/CanvasHome`, with no workspace or
`Home/` wrapper. Unselected trees and trash are neither exposed nor fetched.

`--tree <name>` selects a virtual tree by its exact workspace tree name (not
its type or a context view id). Repeat it to include several trees, and combine
it with `--backend` when needed:

```sh
canvas-fuse mount universe ~/Directory --tree directory
canvas-fuse mount universe ~/Selected --tree context --tree directory
canvas-fuse mount universe ~/Selected --tree directory --backend workspace:home
# Explicitly opt in to just the backend mirror tree:
canvas-fuse mount universe ~/Backends --tree backends
```

One distinct source mounts its contents directly at the supplied mountpoint.
Multiple sources expose `Trees/<name>/` and, when selected, `Home/` at that
same mountpoint. Duplicate selectors are ignored. Explicit selections omit
`Trash/`; tree deletes still follow the server's usual detach/trash rules.
Only `workspace:home` is currently supported as a backend. `--mirror` requires
Home to be included. Source selectors cannot be combined with `--context`.
Unknown tree names fail before mounting when the hub is available. Offline
mirror mounts defer tree loading until reconnect.

Without source selectors, workspace mounts expose Home, Trash, and all trees
except `backends`. Use `--include-backends` to include it in that layout; this
flag cannot be combined with source selectors or context mounts. For selected
sources, add `--tree backends` instead. Context layouts are unchanged.
Selected mounts get separate state directories per workspace and mountpoint,
so they can run alongside a full workspace mount. Use the exact supplied path
for `unmount` and `sync` commands.

### mount flags

| Flag | Default | Description |
|------|---------|-------------|
| `<selector>` | - | Positional, before the mountpoint: `<workspace>` or `<workspace>/Contexts/<id>`. |
| `-c/--context <id>` | - | A context view as `<workspace>/<id>` (or a bare id alongside `-w`). Repeatable; a single one roots the mount at it. |
| `-w/--workspace <name>` | - | The workspace to mount, at `<mountpoint>/<name>/`. With `-c` it scopes the context mount instead. |
| `--tree <name>` | - | Include this virtual tree; repeatable. |
| `--backend workspace:home` | - | Include the home drive; repeatable. |
| `--include-backends` | false | Include `Trees/backends/` in a full workspace mount. With source selectors, use `--tree backends` instead. |
| `--no-backends` | false | Explicitly exclude the backend tree (already the default); Home and mirroring remain enabled. |
| `--root <selector>` | - | The selector as a flag, for when it comes from config or a script. |
| `-d/--detach` | false | Daemonize after pre-flight; logs written to the state dir. |
| `--no-ws` | false | Disable the websocket event bridge (poll-only). |
| `--no-nudge` | false | Disable the inotify nudge (see below). Also `CANVAS_FUSE_NO_NUDGE`. |
| `--resync <secs>` | 30 | Full resync interval in seconds. |
| `--data-dir <path>` | `~/.canvas/<remote>/fuse/…` | Per-mount state directory (sticky filename map). Also `CANVAS_FUSE_DATA_DIR`. |
| `--blob-cache-mb <n>` | 256 | In-memory cache budget for file content. |
| `--mirror` | false | Keep `Home/` as a real folder mirrored with the hub (see [Mirror mode](#mirror-mode---mirror)). |
| `--conflicts prompt\|rename` | rename | Mirror: inbox (hub keeps the name) or Dropbox-style conflict copy. |
| `--deletes propagate\|keep` | propagate | Mirror: whether a local `rm` deletes on the hub. |
| `--ignore <glob>` | - | Mirror: never upload matching keys; repeatable. |
| `--poll <secs>` | 30 | Mirror: change-feed poll interval (socket nudges arrive sooner). |

### Connection

Commands resolve server/token in this order, so they work flag-less wherever
canvas-cli is logged in:

1. `--server` / `--token`
2. `CANVAS_SERVER` / `CANVAS_API_TOKEN`
3. `--remote <name>` from `~/.canvas/config/remotes.json`
4. `boundRemote` from `~/.canvas/config/cli-session.json`

Agent containers typically use env vars + one context:

```sh
CANVAS_SERVER=https://canvas.example CANVAS_API_TOKEN=canvas-... \
  canvas-fuse mount -d universe/Contexts/mbag /workspace/context
```

## Embedding

`canvas_fuse::mount(MountOptions) -> MountHandle` — dropping the handle (or
calling `unmount()`) tears down the ws client, threads, and the kernel mount.
`MountOptions.contexts` filters which contexts are materialized.
`MountOptions.selection` accepts a `WorkspaceSelection { trees, home, include_backends }`;
`WorkspaceSelection::default()` includes Home, Trash, and non-backend trees.
Set `include_backends: true` to include the backend tree in that layout, or
explicitly name `backends` in `trees` when selecting sources. Set
`workspace` for source selection and leave context options unset.

## Internals

Document requests negotiate gzip, which reduces transfer size for large email
listings while preserving the full message content. Older servers can still
return uncompressed responses. Body-transfer failures are reported separately
from malformed JSON; requests retain their 30-second timeout.

- **Blobs are real files.** Name from the location URL basename, size from
  `metadata.size` (or the blob, lazily, on first `stat`). Bytes come from
  `GET /workspaces/:id/documents/:docId/content` on first read. The FUSE loop
  never blocks on the network; concurrent readahead of the same blob is one
  download; cache is LRU by checksum (`--blob-cache-mb`).
- **Hot path is local.** `lookup`/`readdir`/`getattr`/`read` hit an in-memory
  tree behind a `parking_lot::RwLock`. The network is the refresh worker.
  Kernel TTLs are short (1s); correctness is explicit invalidation.
- **Sticky filenames.** Constructed names are persisted in redb keyed by
  `(context, docId)`. Collisions get a docId suffix (`Meeting_2.md`) and never
  silently swap back, so Obsidian links stay valid. Per-device for now. The
  table name carries a generation: when the renderer changes what a document is
  CALLED, bumping it retires the old assignments in one step instead of pinning
  every existing document to its old name forever.
- **Inode stability.** A document keeps its inode across URL switches inside a
  context, so open handles survive a view swap. Documents that leave the view
  follow unlink semantics.
- **Directory watchers.** FUSE reverse invalidation does not emit `IN_DELETE`
  to parent-directory watchers. After a remote-driven change the daemon
  creates and unlinks a virtual `.canvas-tmp` in each affected directory
  (`--no-nudge` disables) so Obsidian/Dolphin/chokidar rescan. `.canvas-tmp`
  is reserved: virtual, hidden from `readdir`, never a server document.

  A marker alone only reaches watchers that re-list a directory on any signal.
  One that handles events per FILE — Obsidian — discards it: an unknown path
  that no longer exists by the time it stats. So the daemon also emits events
  that name the real documents: a `utimensat` on each file that appeared or
  changed (`IN_ATTRIB`), and a real `unlink()` for each one that left, which is
  the only way the kernel names it in an `IN_DELETE`. A departed document is
  held in the tree as a tombstone purely so that unlink can find it; collecting
  a tombstone is resolved entirely in the tree and never reaches the server, so
  the document itself is untouched.

  Two more things decide whether a client actually notices. The directory's **mtime**
  moves whenever its entries change, because a watcher that gets the event still
  re-stats before re-listing and skips the work when the timestamp is unchanged
  (KDE's lister does exactly this — it is why remote changes needed an F5). And
  the marker's **name** is what a watcher judges the event by: the default is
  hidden, which clients that ignore dotfiles — Obsidian excludes them from a
  vault outright — discard along with the only notification they were going to
  get. `--nudge-name canvas-refresh.tmp` gives it a visible one.
- **Daemon.** `mount -d` daemonizes after pre-flight, writes state under
  `~/.local/state/canvas-fuse/mounts/`, and exits hard on SIGTERM after
  unmounting — rust_socketio's reconnect thread otherwise outlives
  `disconnect()`. `unmount`/`status` use those state files; crash leftovers
  are cleaned up, stale kernel mounts recovered with `fusermount3 -uz`.

## Not yet

- Writing file blobs (they show as-is; reads work)
- A global `Workspaces/` umbrella (only a rooted single `-w` is supported)
- Eager per-path document refresh in workspace mode (one request per tree
  path; fine at wiki scale)

## Licence

Copyright (C) 2026 Jozef Melich.

Canvas FUSE is licensed under the **[AGPL-3.0-or-later](LICENSE)** and under no
other terms. No commercial exemption is offered for this component, to anyone.
The Canvas clients stay free software in all cases.

Contributing needs no CLA here, only a DCO sign-off (`git commit -s`). See
[CONTRIBUTING.md](CONTRIBUTING.md). The dual-licensed Canvas components are
listed in [NOTICE](NOTICE).

### Client certificate authentication

FUSE reads `tls: { certFile, keyFile }` from the selected CLI remote before
connecting. Configure it with `canvas remote tls set`. Direct connections can use:

```sh
canvas-fuse ping --server https://canvas.example.org --token YOUR_TOKEN --tls-cert /absolute/path/client-chain.crt --tls-key /absolute/path/client.key
```

Connection flags override environment; `CANVAS_TLS_CERT` and `CANVAS_TLS_KEY`
override the selected remote as a pair. An explicit named remote is still read
when server/token flags are supplied. Inheriting its identity across a server
origin change is rejected. HTTP and Socket.IO use the same validated identity,
including mirror uploads/downloads and reconnects. Server verification is enabled.
Use a protected unencrypted PEM RSA/EC key and a leaf-first certificate chain.
Renewal requires restarting/remounting; a running mount retains its loaded identity.

`cargo build --example tls-smoke` plus `node examples/check-tls.mjs` runs native
HTTP/mirror/upload/Socket.IO checks through a generated nginx fixture. The test
requires a sibling canvas-common checkout with test dependencies installed,
OpenSSL, nginx, and permission to bind loopback ports; no kernel mount is needed.
