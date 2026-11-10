# gentle-partitioner

A Rust terminal host that **partitions the terminal** and runs `gentle-shell`,
unchanged, inside its own pseudo-terminal. It is the visual/“supervisor” engine
described in the ODD: a larger engine that activates first when you launch the
shell and splits the work across concurrent threads so the user never perceives
a stall (S2). The existing gentle-shell structure is treated as a stable core
and is never modified — the partitioner only launches it as a child process (S1).

Each partition is a render of its own terminal, composed into a single visual
component (S3): when one pane needs to update, the whole terminal is not
repainted — only the changed cells of that fragment are written out.

## What it does

- **Chat mode (default).** Shows the `gentle-shell` chat exactly as it is today,
  plus a lateral **tool bar** (S7). Tools open an overlay on demand: the file
  tree, worktree changes, help, or a quit action.
- **Workspace mode.** A partitioned layout with a **sidebar** (folders, files,
  formats, sizes, git worktree status), a **center viewer** split into the file
  content in an **editable** buffer (left) and the agent’s changes from
  `git diff` (right, read-only), and the **chat** at the bottom. On small
  terminals the layout
  degrades back to chat mode so the chat stays usable (S11).
- **Prediction (S9).** A deterministic n-gram engine (no model weights, no
  network) runs in-process. In the editor it shows a dim **ghost suggestion**
  after the cursor at the end of a line; `Tab` or `Right` accept it. The same
  engine is served on a loopback HTTP endpoint (random port, bearer token) so
  the `gentle-shell` child can use it for its own prompt editor. Disable it all
  with `--no-predict`.

## Prediction engine & server

The engine (`predict.rs`) keeps unigram/bigram/trigram continuation counts with
trigram → bigram → unigram backoff, plus a frequency-ordered prefix index that
completes the partial current word. It is tokenized on Unicode words (letters,
digits and `_`, so Spanish accents and `ñ` work) plus single punctuation tokens.
`predict(text_before_cursor, mode)` returns **only the suffix** to insert:

- Inside a word (≥ 2 chars typed): the most frequent known longer word with that
  prefix, with the typed prefix's case preserved.
- At a boundary: up to three next words via backoff, stopping at a line end.
- Never on empty/whitespace input or when the best candidate count is below 2;
  suggestions are capped at 60 characters.

There are two independent models, `code` and `chat`. Memory is bounded by
pruning count-1 entries once the vocabulary passes ~200k n-gram entries. At
startup, on background threads: `code` is seeded from the repo's tracked text
files (`git ls-files`, skipping binaries and files > 256 KiB, capped at 20 MiB);
`chat` is seeded from gentle-shell's shared prompt history when present
(`~/.pi/agent/history/{history-global.jsonl, projects/<hash>/*.jsonl}`, read-only).

The server (`predict_server.rs`) is a std `TcpListener` bound to `127.0.0.1:0`
on its own thread, sharing the engine via `Arc<RwLock<..>>`. The child receives
`GENTLE_PREDICT_URL=http://127.0.0.1:<port>` and `GENTLE_PREDICT_TOKEN=<token>`
(32 random hex chars from `/dev/urandom`).

| Request | Response |
| --- | --- |
| `POST /predict {"text":"...","mode":"chat"\|"code"}` | `200 {"suggestion":"..."}` (empty when none) |
| `POST /learn {"text":"...","mode"?}` | `204` |
| missing / wrong `Authorization: Bearer <token>` | `401` |
| unknown path | `404` · wrong method `405` |
| body > 64 KiB or bad JSON | `400` |

JSON for these two shapes is parsed and serialized by hand (quotes, backslashes,
control characters and `\uXXXX` are handled).

## Build & run

Requires a Rust toolchain (`cargo`). From this directory:

```sh
cargo run --release
```

That launches the default command, `gentle-shell`. To wrap a different command,
pass it after `--`:

```sh
cargo run --release -- bash
cargo run --release -- sh -c 'echo hi; sleep 1'
```

Other flags:

```sh
gentle-partitioner --help        # usage
gentle-partitioner --version     # version
gentle-partitioner --no-predict  # disable the prediction engine and server
```

The child process inherits the current working directory, with
`TERM=xterm-256color` and `GENTLE_PARTITIONER=1` set (plus
`GENTLE_PREDICT_URL`/`GENTLE_PREDICT_TOKEN` unless `--no-predict`). If the command cannot be
found on `PATH`, the partitioner prints a clear error naming it and exits with
status `1` without disturbing the terminal.

## Keybindings

`Ctrl+]` is the prefix chord; the next key runs a partitioner command. Anything
else is forwarded to the chat.

| Key (after `Ctrl+]`) | Action |
| --- | --- |
| `w` | Toggle workspace (files · editor · agent changes · chat) |
| `t` | Files tool (overlay) |
| `c` | Worktree changes tool (overlay) |
| `?` | Help overlay |
| `Tab` | Next pane · `1`-`4` focus sidebar / editor / changes / chat |
| `q` | Quit (stops gentle-shell) |
| `]` | Send a literal `Ctrl+]` to the chat |

- **Sidebar:** `↑↓` (or `k`/`j`) move · `Enter`/`→` open · `←` collapse / go to
  parent · `Esc` back to chat.
- **Editor (left pane):** type to insert text (UTF-8 and tabs); `Enter` splits a
  line, `Backspace`/`Delete` join lines at the edges, `Tab` inserts a literal
  tab. Move with the arrows, `Home`/`End`, `PageUp`/`PageDown`,
  `Ctrl+Home`/`Ctrl+End`. `Ctrl+S` saves, `Ctrl+R` reloads from disk
  (discarding edits). A `●` before the path marks unsaved edits. `Esc` returns
  to the chat.
  - With prediction on, a dim **ghost suggestion** appears after the cursor at
    the end of a line; `Tab` or `Right` accept it. Without a ghost, `Tab` and
    `Right` behave exactly as above. Saving learns the buffer (mode `code`).
  - Saving writes atomically (temp file + rename, preserving permissions) and
    refuses if the file changed on disk since it was loaded — `Ctrl+R` reloads.
  - Opening another file or quitting with unsaved edits warns first; repeat the
    action to discard.
  - Binary, unreadable and oversized files stay read-only.
- **Agent changes (right pane):** `↑↓` `PgUp` `PgDn` scroll · `Esc` back to chat.
- **Overlays:** `↑↓` move · `Enter` open · `Esc` close.
- **Mouse:** click selects panes and places the editor cursor, opens files and
  tools; the wheel scrolls the chat, the editor, the diff, or the sidebar
  depending on where it is over.

## Architecture

- **`main.rs`** — the binary: terminal setup (raw mode, alternate screen, mouse
  capture, bracketed paste, restored on exit, error and panic), and the event
  loop. Three threads feed a single channel (S2): an **input** thread
  (`crossterm::event::read`), the **PTY reader** thread (inside `PtyPane::spawn`,
  coalescing wake-ups through an `AtomicBool`), and a **git-status** worker that
  runs `git status` off the UI thread. The UI thread blocks on the channel and
  repaints only after an event — it never polls and never repaints while idle
  (S11). PTY output schedules a git refresh at most once every ~800 ms.
- **`app.rs`** — all application state and the pure `handle_key` / `handle_mouse`
  transitions (unit-tested without a terminal), plus `draw`, which renders every
  partition into its own rectangle.
- **`layout.rs`** — the pure layout engine mapping the terminal area to
  independent partitions; degrades to chat mode on small terminals.
- **`pane.rs`** — a partition backed by its own PTY (`portable-pty` +
  `vt100`): spawn, resize, key→bytes write, scrollback, and copying only the
  emulator cells into ratatui’s buffer so render diffing writes just what
  changed (S3).
- **`tree.rs`** — the sidebar model: directory scan, sizes, formats and a
  `git status --porcelain` parser.
- **`viewer.rs`** — the right half of the center viewer: a classified `git diff`
  against `HEAD`, plus the read-only file reader used for previews.
- **`editor.rs`** — the left half of the center viewer: a small editable buffer
  that preserves the file’s bytes (tabs, CRLF/LF, trailing newline) and saves
  atomically with an on-disk-change guard.
- **`predict.rs`** — the deterministic n-gram prediction engine, the hand-written
  JSON helpers, and the repo/history seeding (S9).
- **`predict_server.rs`** — the loopback HTTP server exposing the engine to the
  `gentle-shell` child with a bearer token (S9).
- **`keys.rs`** — encodes crossterm key events as the bytes a terminal sends to
  a PTY child.

## Roadmap

- **T7 (S8)** — translator engine: a pivot-through-English middleware around Pi,
  so the user always reads their native language while everything flows through
  English.
- **T8a (S9)** — prediction engine, local server and editor ghost text: **done**.
- **T8b (S9)** — gentle-shell prompt editor ghost suggestion from
  `GENTLE_PREDICT_URL` (dim inline, `Tab`/`Right` accept, learn submitted
  prompts): next.

## Fast development loop

`Cargo.toml` optimizes dependencies once (`[profile.dev.package."*"] opt-level = 3`)
and keeps this crate unoptimized, so an edit rebuilds in under a second while the
UI stays fluid. Linux x86_64 links with `lld` by default since Rust 1.90.

- Iterate with `cargo run` (dev profile); use `cargo run --release` for the final binary.
- Optional: `cargo install bacon` and run `bacon test` for rebuild-on-save.
- Only if linking becomes slow: install `mold` and add `-C link-arg=-fuse-ld=mold` in `.cargo/config.toml`.
