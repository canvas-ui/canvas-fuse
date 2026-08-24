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
reads take a byte window, writes replace the whole file on close.

Because context-bound browser tabs are just `.url` files, a file manager can
drive them: `rm reddit.url` closes the tab, writing a `.url` opens one, editing
one navigates it.

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

Writes buffer per open file and flush on close. Close-time errors reach the
application. Flush serializes against refresh so a server-driven resync cannot
drop or rename a file mid-save. Blob files (PDFs, images, …) are readable and
not writable.

Editors that truncate-then-write (Obsidian, VS Code), atomic tmp+rename
(`sed -i`, vim), append, `touch`, `mv`, and `rm` are the supported save
patterns. Point an Obsidian vault at a local dir and symlink a context (or a
`Trees/` path) into it — Obsidian wants a writable vault root for `.obsidian/`,
keep that outside the mount.

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
stability, workspace/home materialization). CI also runs `cargo fmt --check`
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

### mount flags

| Flag | Default | Description |
|------|---------|-------------|
| `<selector>` | - | Positional, before the mountpoint: `<workspace>` or `<workspace>/Contexts/<id>`. |
| `-c/--context <id>` | - | A context view as `<workspace>/<id>` (or a bare id alongside `-w`). Repeatable; a single one roots the mount at it. |
| `-w/--workspace <name>` | - | The workspace to mount, at `<mountpoint>/<name>/`. With `-c` it scopes the context mount instead. |
| `--root <selector>` | - | The selector as a flag, for when it comes from config or a script. |
| `-d/--detach` | false | Daemonize after pre-flight; logs written to the state dir. |
| `--no-ws` | false | Disable the websocket event bridge (poll-only). |
| `--no-nudge` | false | Disable the inotify nudge (see below). Also `CANVAS_FUSE_NO_NUDGE`. |
| `--resync <secs>` | 30 | Full resync interval in seconds. |
| `--data-dir <path>` | `~/.canvas/<remote>/fuse/…` | Per-mount state directory (sticky filename map). Also `CANVAS_FUSE_DATA_DIR`. |
| `--blob-cache-mb <n>` | 256 | In-memory cache budget for file content. |

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

## Internals

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
