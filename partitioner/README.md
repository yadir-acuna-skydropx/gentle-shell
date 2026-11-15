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

## Open it with `gentle-shell` (optional, T26)

By default you launch the partitioner with `gentle-partitioner`. To make the
plain `gentle-shell` command open the partitioner instead (S2), enable the
launcher once per machine:

```sh
partitioner/scripts/install-gentle-shell-launcher.sh      # appends to ~/.bashrc or ~/.zshrc
source ~/.bashrc                                           # or ~/.zshrc, or open a new shell
```

It installs a shell function that runs `gentle-partitioner` for interactive
`gentle-shell`, while the partitioner still spawns the **real** engine as its
child — it resolves the child through `PATH` (not shell functions) and exports
`GENTLE_PARTITIONER=1`, so the function detects that and avoids any recursion.
Run `command gentle-shell` any time you want the raw engine without the
partitioner.

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
- **Icons (S4).** The sidebar and the Files overlay show a **Nerd Font** glyph
  per entry: open/closed folders and a per-language file icon in the language's
  brand colour (e.g. the Python glyph in blue for `.py`, Rust orange for `.rs`).
  See “Icons” below for the font requirement and the ASCII fallback.
- **Syntax highlighting (S5).** The editor pane highlights code per language,
  colours brackets by nesting depth (**rainbow brackets**) and emphasises the
  **bracket matching** the one at the cursor. See “Syntax highlighting” below.
- **Prediction (S9).** A deterministic n-gram engine (no model weights, no
  network) runs in-process. In the editor it shows a dim **ghost suggestion**
  after the cursor at the end of a line; `Tab` or `Right` accept it. The same
  engine is served on a loopback HTTP endpoint (random port, bearer token) so
  the `gentle-shell` child can use it for its own prompt editor. Disable it all
  with `--no-predict`.
- **Apps panel (S12).** A collapsible **apps panel** sits bottom-left under the
  sidebar (and opens as an overlay in chat mode via the toolbar's **Apps** tool).
  It lists connectable apps with a coloured status dot and an item count;
  expanding an app shows its items. The expanded panel takes **40–70%** of the
  sidebar height (default 45%, `+`/`-` to adjust; the file tree keeps ≥ 5 rows).
  The connections run in a separate Rust process, `gentle-apps`, spoken to over
  JSON-lines on stdio. The apps are **Slack** (mentions + threaded replies, and
  *open in Slack Desktop*) and **GitHub** (repos, branches, trees, PRs, issues,
  local worktrees and **Actions workflows**, with simple actions). The panel's
  header row carries its own controls — `[_]` collapses it to the app list,
  `[□]` expands it, and clicking the **Apps** title toggles the two — all
  independent of the sidebar's own `[_]` minimize. A local **Arquitectura** row
  always trails the engine apps; its status dot comes from the `gentle-arch`
  engine (green when the key is set and the KB has chunks, yellow when the key is
  set but the KB is empty, gray without a key, red when the engine is offline),
  and selecting it (Enter or click) opens the architecture overlay.
  Disable the engine and panel with `--no-apps`.
- **Code explainer (S17).** Select code in the editor (Shift+arrows/Home/End, or
  drag the mouse; `Ctrl+C` copies the selection) and press **`e`**, **`Ctrl+E`**,
  or click the **✿ ?** mascot badge that appears at the end of the selection — or
  right-click the selection — to open a popup that **streams an explanation** of
  the selection and answers follow-up questions in the same conversation. With
  no selection it explains the current line ±10 lines. The badge (ASCII fallback
  `(?)`) stays inside the editor and never overwrites the selected text; the
  status line hints `✿ click or Ctrl+E to ask about the selection`. It uses **Claude Haiku**
  over the Anthropic API via a separate Rust process, `gentle-explain`; the API
  key lives in the system keyring. Disable it with `--no-explain`.
- **Architecture engine (S18).** Press **`A`** (or the **Arch** tool) to open an
  overlay that either **reviews this repo** (scans Airflow DAGs, CI and IaC,
  redacting secrets) or **designs from a brief** you type (Ctrl+Enter or
  Alt+Enter submits). It retrieves from a curated GCP + data knowledge base and
  streams a structured markdown report (pillar findings, cost analysis, Mermaid
  C4, MADR ADRs, sources) written to `./.gentle-arch/`; press **`o`** to open the
  saved report. It uses **Claude Sonnet** (or **opus** with `--deep`) over the
  Anthropic API via a separate Rust process, `gentle-arch`. Disable it with
  `--no-arch`.

## Code explainer (`gentle-explain`)

The partitioner spawns the independent `gentle-explain` binary (found next to
the partitioner, else on `PATH`), talks to it over NDJSON on stdio, and
restarts it with backoff if it exits. The popup shows a *thinking…* state while
waiting, the streamed text with word wrap and wheel/PgUp/PgDn scroll, inline
errors, and a one-line question input (Enter asks a follow-up; Esc closes and
cancels any in-flight request). When no key is stored it shows
`Set your Anthropic API key: `gentle-explain key set`` and sends nothing.

Manage the key (never printed):

```
gentle-explain key set     # read the key (no echo / stdin), validate it, store it
gentle-explain key status  # prints "API key: set" or "API key: not set"
gentle-explain key clear   # remove the stored key
```

The key is stored in the system keyring (service `gentle-engines`, account
`anthropic`) and only ever travels in the `x-api-key` header — never in logs,
errors, `Debug`, the protocol or panic messages. The model is resolved at
runtime (`GET /v1/models`, newest **haiku**); override it in
`~/.config/gentle-partitioner/engines.toml` under `[explain] model = "…"`, and
set the default output language with `lang` (default `"es"`). The Anthropic API
version header is pinned to `2026-10-21` (a single constant in
`engines/src/anthropic.rs`). The shared Anthropic client, keystore and NDJSON
protocol live in the `gentle-engines` library so the `gentle-arch` engine
reuses them.

## Architecture engine (`gentle-arch`)

The partitioner spawns the independent `gentle-arch` binary (found next to the
partitioner, else on `PATH`), talks to it over NDJSON on stdio, and restarts it
with backoff if it exits. It reuses the `gentle-engines` Anthropic client,
keystore (same `sk-ant-…` key as `gentle-explain`, keyring service
`gentle-engines`) and config. The default model line is **sonnet**; `--deep`
uses **opus**; override via `~/.config/gentle-partitioner/engines.toml` under
`[arch] model = "…"`.

### Knowledge base

A curated, versioned list of ~18 **official** source URLs (GCP + data first:
Airflow best practices and the BigQuery/Dataflow operators; Cloud Composer
overview, pricing and optimization; Cloud Scheduler; Cloud Run jobs; Workflows;
BigQuery cost/performance best practices; Dataflow; the Architecture Framework
pillars; and the GCP "Orchestrating your data workloads" blog) lives in
`engines/src/kb.rs`.

```
gentle-arch kb update   # fetch + strip HTML + chunk (~800 tokens, overlap) + BM25 index
gentle-arch kb status   # sources, chunks, age and failed fetches
```

The KB is stored under `~/.local/share/gentle-partitioner/kb/` (override with
`GENTLE_ARCH_KB_DIR`). Fetches use `ureq`; HTML is reduced to text by a small own
tag stripper (drops script/style/nav, keeps headings and code); chunks are
indexed with an own BM25 (`k1=1.2`, `b=0.75`, lowercase Unicode tokenization,
Spanish/English stopwords) serialized to JSON. Per source it records url, title,
`fetched_at` and sha256. A failed fetch is recorded and **does not abort** the
update.

### Review and design

```
gentle-arch review [PATH] [--deep] [--out FILE] [--no-tools]
gentle-arch design "<brief>" [--deep] [--out FILE] [--brief-file F]   # or pipe the brief on stdin
```

`review` scans `PATH` (default cwd) with bounded reads (≤ 200 files, ≤ 120 KiB,
DAGs/CI/IaC first) and a manifest of what was included/excluded. **Secrets never
leave the machine:** `.env*`, `*.pem`, `*.key`, `id_rsa*`, `*credentials*`,
service-account JSON (`private_key`), `*.tfstate*`, `.git/`, `node_modules`,
`target`, `venv` are never read; `*.tfvars` values are replaced with
`<redacted>`; and inline secrets (AWS/GCP keys, Slack/GitHub/Anthropic tokens,
`password|secret|token|api_key` assignments, PEM blocks) are redacted before any
text is sent. When on `PATH` (unless `--no-tools`), `infracost` and `checkov`
run with a 120 s timeout and only their summarized JSON enters the context;
missing tools are noted as "not installed". The top 12 BM25 chunks go into the
(cache-controlled) system prompt, cited as `[n]` with their URL. The model
returns a structured report (executive summary; architecture + Mermaid C4;
findings by Architecture Framework pillar; cost analysis; CI/CD proposals; MADR
ADRs; sources) streamed to the terminal and written to `--out` (default
`./.gentle-arch/<kind>-<timestamp>.md`, never overwriting). `design` runs the
same structure from a brief, answering in the brief's language.

With no args `gentle-arch` runs the stdio engine: `{"type":"review",…}`,
`{"type":"design",…}`, `{"type":"cancel",…}` and `shutdown` →
`delta`/`done{"path"}`/`error`/`status{key,model,kb_chunks}`.

## Apps engine (`gentle-apps`)

The partitioner spawns the independent `gentle-apps` binary (found next to the
current executable first, then on `PATH`) and talks to it over **NDJSON on
stdio** — one JSON object per line, flushed after every message, logs only on
stderr. Lines are capped at 1 MiB; an oversized or invalid line is skipped and
logged, never a panic. The child is restarted with exponential backoff
(1, 2, 4, 8, 16, max 30 s) and killed on quit; while it is down the panel shows
*apps engine offline*.

Protocol (shared types live in the `gentle-apps` library target, so both sides
use the same definitions):

- **engine → partitioner**
  - `{"type":"apps","apps":[{"id":"slack","name":"Slack","status":"disconnected|connecting|connected|error","detail":"…"}]}` — the full app list, on start and on any status change;
  - `{"type":"items","app":"slack","items":[{"id":"…","title":"#channel · author","text":"…","ts":"…","link":"…"}]}` — the full current item list for one app;
  - `{"type":"result","id":N,"ok":true|false,"error":"…"?}` — the correlated outcome of a `reply`/`refresh`/`expand` request;
  - `{"type":"children","id":N,"app":"github","item":"<item id>","items":[…]}` — the children of an `expand`;
  - `{"type":"actionresult","id":N,"ok":true|false,"message":"…"?,"error":"…"?}` — the outcome of an `action`.
- **partitioner → engine**
  - `{"type":"reply","id":N,"app":"slack","item":"<item id>","text":"…"}`;
  - `{"type":"refresh","id":N,"app":"slack"}`;
  - `{"type":"expand","id":N,"app":"github","item":"<item id>"}`;
  - `{"type":"action","id":N,"app":"github","item":"<item id>","action":"<action id>"}`;
  - `{"type":"shutdown"}`.
- An unknown message type gets a `result` error when it carries an `id`, and is
  ignored otherwise.

An **item** may carry three optional, backward-compatible fields: `kind` (an
icon hint), `expandable` (has children reachable via `expand`), and `actions`
(`[{"id","label","confirm"}]`). A flat item without them (Slack) parses and
serializes exactly as before.

Inside the engine each app is a **`Connector`** (`id`, `name`, `status`, `poll`,
`reply`, and the default-refusing `expand`/`action`) driven on its **own
thread**, so a slow app never blocks the others or the stdin loop. `gentle-apps`
with no args runs the stdio engine; `gentle-apps --help`/`--version` work, and
the `gentle-apps slack …` subcommands manage the Slack login (below). The
panel navigates a hierarchical app with a breadcrumb (Enter expands, Esc steps
up, `r` refreshes the level), and `a` (or Enter on a leaf) opens an action menu;
an action marked `confirm` prompts **Run <label>? y/n** before it runs.

## Connect Slack

The Slack connector shows the messages that mention you and lets you reply **in
the mention's thread, as yourself**. Authentication is a browser OAuth login
with PKCE — no token is ever pasted or printed.

### 1. Create the Slack app

1. Go to <https://api.slack.com/apps> → **Create New App** → **From an app
   manifest** → choose the workspace.
2. Paste `partitioner/apps/slack-manifest.yaml` and create the app.
3. Verify the redirect URL `http://localhost:47917/callback` and that **PKCE is
   enabled**. Installing to the workspace separately is **not** needed — the
   login performs the install.
4. Copy the **Client ID** from *Basic Information* (it is **not** a secret).

### 2. Log in

```sh
gentle-apps slack login --client-id <ID>
```

This opens your browser (via `xdg-open`, and also prints the URL so you can
paste it if the browser does not open), runs a one-shot loopback server on the
fixed port **47917**, validates the OAuth `state` in constant time, exchanges
the authorization code with the PKCE `code_verifier` (**no client secret**) and
stores the result. The loopback accepts callbacks for up to 5 minutes.

- `gentle-apps slack status` — prints whether Slack is connected and the
  workspace, never the token.
- `gentle-apps slack logout` — deletes the keyring token and the workspace/user
  metadata (the non-secret client id is kept so a re-login needs no
  `--client-id`).

### What is stored where

- **Secret** (keyring, service `gentle-apps`, account `slack`): the user token
  (`xoxp-…`) and, only when token rotation is enabled, the refresh token and
  expiry, as a small JSON blob. One workspace per machine.
- **Non-secret** (`~/.config/gentle-partitioner/apps.toml`, dir `0700`, file
  `0600`): `[slack] client_id`, `team_id`, `team_name`, `user_id`. The token
  never touches this file.

### Scopes and why each is needed

User scopes (the app acts as you, not as a bot):

- `search:read` — `search.messages` to find mentions of you;
- `chat:write` — `chat.postMessage` to reply in the thread;
- `users:read` — `users.info` to render `<@U…>` mentions as names;
- `channels:read`, `groups:read`, `im:read`, `mpim:read` — read channel/DM
  metadata (names, types) for the item titles;
- `channels:history`, `groups:history`, `im:history`, `mpim:history` — only for
  `conversations.replies`, the fallback that finds a mention's parent thread
  when the permalink has no `thread_ts`.

### Open a mention in Slack Desktop

Every mention carries an **open** action (reach it with `a`). When the official
Slack **Desktop** app is installed — a `slack` binary on `PATH` or an
`x-scheme-handler/slack` default (`xdg-mime query default
x-scheme-handler/slack`) — it opens the **channel deep link**
`slack://channel?team=<TEAM_ID>&id=<CHANNEL_ID>` (the team id comes from
`auth.test`). Slack's deep-linking reference
(<https://api.slack.com/reference/deep-linking>) documents only `team` and `id`
for `slack://channel` — there is **no documented `message` parameter** — so the
channel deep link is used, with the message's **https permalink** as the
fallback when the desktop app is not installed.

### How a reply finds the thread

Search results have no `thread_ts`, so the parent is derived in order: (1) the
`thread_ts` query param of the message permalink; else (2)
`conversations.replies?channel=…&ts=<ts>&limit=1` → `messages[0].thread_ts`;
else (3) the message's own `ts`. The reply always targets the mention's channel,
never another.

### Limits

- Mentions are **polled every 30 s** (real-time user mentions need a bot).
- Slack's **search index lags** a little, so a new mention can take a moment to
  appear.
- `search.messages` is a **legacy** method (migration path:
  `assistant.search.context`).
- Enabling PKCE marks the app as a **public client** and is **one-way**; it
  cannot be undone.

## Connect GitHub

The GitHub connector **reuses your `gh` CLI session**: it runs `gh auth token`
(no shell) when it starts and again once after a `401`, and keeps the token
**only in memory** — it is never logged, printed, `Debug`-formatted or sent over
the protocol. If `gh` is missing or logged out the app shows *disconnected* with
the detail ``run `gh auth login` ``. HTTP uses the shared ureq helper with
`Authorization: Bearer`, `Accept: application/vnd.github+json` and
`X-GitHub-Api-Version: 2022-11-28`; the base URL is overridable for tests via
`GENTLE_APPS_GITHUB_API`.

It **polls every 60 s** (respecting `X-RateLimit-Remaining`/`Reset` and
`403`/`429` with `Retry-After`, backing off up to 60 s). The top-level items are
sections, each titled with its count and expandable:

1. **Review requested** — `is:pr is:open review-requested:@me archived:false`; children are PRs `repo#123 · title · author`.
2. **My PRs** — `is:pr is:open author:@me archived:false`; children are PRs with the CI state from the GraphQL `statusCheckRollup` of the head commit: `✓` success, `✗` failure, `●` pending, `–` none (fetched lazily on expand, so a poll stays under ~10 requests). The GraphQL expand re-reads the `gh` token once on a `401`, like the REST calls.
3. **Assigned issues** — `is:issue is:open assignee:@me archived:false`.
4. **Repositories** — `GET /user/repos?affiliation=owner,collaborator,organization_member&sort=pushed&per_page=50` → `owner/name · default branch · pushed <relative time>`. Expand a repo → its branches (the default branch and branches with an open PR are marked); expand a branch → the root tree; expand a dir → its subtree, lazily per level; files are leaves showing their size. Branches are paged (`?per_page=100&page=N`) for up to **5 pages / 500 entries**, then a trailing `… more branches`; tree levels are capped at 500 with a trailing `… N more`.
5. **Worktrees (local)** — `git worktree list --porcelain` in the engine's cwd: one child per worktree with its path, branch (or *detached*) and *dirty*/*clean* state. Outside a git repo it shows *no local repository*.
6. **Workflows** — the latest GitHub Actions runs of the **local** repo (the `origin` of the engine's cwd), `GET /repos/{o}/{r}/actions/runs?per_page=20` (one request per poll). Each run shows `<workflow name> · <branch> · <glyph> · <relative time>` with `✓` success, `✗` failure, `●` in-progress/queued, `⊘` cancelled, `–` other; expand a run → its jobs with their conclusion. Outside a local GitHub repo it shows *no local GitHub repository*.

### In-terminal issue/PR reader (S27)

Instead of opening the browser, a GitHub **issue or PR opens inside the
partitioner**. In the apps panel, **`Enter`** (or a mouse click) on an issue/PR
leaf sends a read-only **`view`** request — fetched off the UI thread by
`gentle-apps` (the same `gh`-token HTTP client; the token is never logged) — and
opens a full **Reader overlay** with the title, `owner/name#N · issue|PR · state
· @author · labels`, the URL, the body and each comment (author + body),
**word-wrapped to the width** and scrollable. Body + comments are bounded
(~200 KB total, at most 50 comments). **`a`** still opens the action menu, where
**View in terminal** is offered first and **Open in browser** second.

Reader keys: `↑↓`/`j`/`k`/`PageUp`/`PageDown` and the **mouse wheel** scroll;
**`y`** copies the whole issue text (title + URL + body, capped ~100 KB) to the
system clipboard via the **OSC 52** escape sequence (works over SSH in terminals
that support it) and shows *copied*; **`o`** opens it in the browser as a
fallback; **`Esc`** closes. The overlay shows *loading…* until the detail
arrives and an inline error if the fetch fails.

### Actions (with confirmation in the panel)

- **view** — on an issue or PR: returns the reader content as an `EngineMsg::Detail` (plain markdown text) instead of running a side effect; opened first by `Enter`. No confirmation.
- **open** — on a PR, issue, repo, branch or file: opens the github.com URL with `xdg-open` (no shell). No confirmation.
- **worktree_add** — on a branch, only when the local repo's `origin` matches that GitHub repo (https/ssh URLs are normalized). Runs `git fetch origin <branch>`, then `git worktree add <repo-parent>/<repo-name>-<sanitized-branch> <branch>` (or `-b <branch> origin/<branch>` when there is no local branch). Refuses if the target dir exists; returns the created path. **Confirmation required.**
- **checkout** — on a branch, only for the local matching repo and only if `git status --porcelain` is empty, else it refuses with *working tree has changes — commit or stash first*. Never uses `--force` and never stashes or discards. **Confirmation required.**
- **rerun_failed** — on a workflow run, offered **only when the run's conclusion is `failure`**: `POST /repos/{o}/{r}/actions/runs/{id}/rerun-failed-jobs`. **Confirmation required.**

All git calls use `std::process::Command` with args (no shell), a ~60 s timeout,
and capture stderr into the error message.

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

## Icons

The sidebar tree and the Files overlay draw a **Nerd Font** glyph before every
entry (`icons.rs`): a closed-folder glyph, an open-folder glyph when the folder
is expanded, and a file glyph chosen by extension or by well-known name
(`Dockerfile`, `Makefile`, `Cargo.toml` → Rust, `package.json` → npm,
`.gitignore` → git, …), coloured with the language's usual brand colour. Each
glyph is a single Private Use Area codepoint from the
[nerd-fonts cheat sheet](https://www.nerdfonts.com/cheat-sheet) (`nf-fa-folder` /
`nf-fa-folder_open`, `nf-seti-*`, `nf-dev-*`, `nf-custom-*`) that `unicode-width`
(the crate ratatui uses to measure cells) reports as exactly **one column**.

- **Font requirement.** Install a Nerd Font and select it in your terminal
  (the ODD choice L19 is **JetBrainsMono Nerd Font** in `~/.local/share/fonts`).
  Without a Nerd Font the glyphs render as tofu boxes.
- **ASCII fallback.** Set `GENTLE_PARTITIONER_ICONS=ascii` for terminals without
  the font: folders show `▸` / `▾` and files show no marker, exactly as before.
  Any other value (or unset) uses the Nerd Font set (the default).

## Syntax highlighting

The editor pane (`highlight.rs`) runs a small **hand-written per-language
tokenizer** — deliberately not `syntect` (which pulls the `onig` C regex engine
or a large fancy-regex payload, growing the binary and startup cost) nor
`tree-sitter-highlight` (a grammar crate and C build per language). The lexer is
dependency-free, keeps the release binary small, starts instantly, and is fast
enough to highlight far more than a screenful per frame (benched at well under
50 ms for 1000 lines in release). Highlighting is computed **only for the
visible lines**, with a per-line cache that carries the end-of-line state and is
invalidated from the edited line down (S11).

- **Languages:** Python, Rust, JS/TS (+JSX/TSX), JSON, TOML, YAML, shell, SQL,
  Markdown, C, C++, Go, Java, HTML and CSS.
- **Token classes & colours:** keyword (magenta), string (green), number (cyan),
  comment (dark gray), function call (**light blue** `Rgb(110,170,255)`), type /
  capitalised name (yellow) and punctuation (gray) — readable on a dark
  background. Colours come from the central **`theme.rs`** palette so they stay
  consistent; the former dark `Color::Blue` (hard to read on dark) was replaced
  by `theme::BLUE`, a lighter blue, for function names and the git-graph lanes
  (S28). The ASCII/no-colour path is unchanged. A guard test in `theme.rs`
  keeps dark `Color::Blue` from reappearing as a foreground outside the palette.
- **Rainbow brackets:** `()`, `[]`, `{}` are coloured by nesting depth on a
  four-colour cycle; brackets inside strings and comments are ignored, and an
  unbalanced closing bracket is shown in **red**.
- **Matching bracket:** when the cursor is on or right after a bracket, it and
  its pair are **bold + underlined**, even across lines (searched within a
  bounded ±2000-line window so it never stalls).
- **Limitations (documented):** multi-line Python triple-quoted strings, JS/TS
  template backticks and `/* */` block comments are tracked across lines; a
  normal single/double-quoted string left open at end of line is treated as
  ending there; Rust lifetimes like `'a` may read as a string; the
  matching-bracket search pairs by nesting only and does not skip brackets
  inside strings or comments.

The editor's existing behaviour is preserved alongside highlighting: tab
expansion, horizontal scroll, the cursor, the dim ghost suggestion, line
numbers, and the dirty / read-only markers.

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
gentle-partitioner --no-apps     # disable the apps engine and the apps panel
gentle-partitioner --no-explain  # disable the code-explainer engine
gentle-partitioner --no-arch     # disable the architecture engine
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
| `a` | Apps panel (toggle expand/collapse in workspace · overlay in chat) |
| `+` / `-` | (panel focused) resize the expanded panel in 5% steps, 40–70% |
| `z` | Zoom (maximize) the focused pane; press again to restore |
| `m` | Minimize the focused pane to its title bar; press again to restore |
| `d` | Changes only — focus and zoom the agent-changes pane |
| `g` | Git graph overlay — commit/branch DAG + local worktrees + open-PR markers (S20) |
| `n` | Conversations manager — new / switch / close gentle-shells (S22) |
| `?` | Help overlay |
| `Tab` | Next pane (skips minimized) · `1`-`4` focus sidebar / editor / changes / chat |
| `q` | Quit (stops gentle-shell) |
| `]` | Send a literal `Ctrl+]` to the chat |

`z`, `m` and `d` are workspace-only; in chat mode they report *available in workspace mode*.
With the diff, terminal or helper focused, `z`/`m` act on that Agent-changes
sub-box instead of the pane (S26, see “Agent changes” below).

- **Sidebar:** `↑↓` (or `k`/`j`) move · `Enter`/`→` open · `←` collapse / go to
  parent · `Esc` back to chat.
- **Editor (left pane):** type to insert text (UTF-8 and tabs); `Enter` splits a
  line, `Backspace`/`Delete` join lines at the edges, `Tab` inserts a literal
  tab. Move with the arrows, `Home`/`End`, `PageUp`/`PageDown`,
  `Ctrl+Home`/`Ctrl+End`. `Ctrl+S` saves, `Ctrl+R` reloads from disk
  (discarding edits). A `●` before the path marks unsaved edits. `Esc` returns
  to the chat.
  - Code is **syntax-highlighted** per language, with rainbow brackets and the
    matching bracket emphasised at the cursor (see “Syntax highlighting”).
  - With prediction on, a dim **ghost suggestion** appears after the cursor at
    the end of a line; `Tab` or `Right` accept it. Without a ghost, `Tab` and
    `Right` behave exactly as above. Saving learns the buffer (mode `code`).
  - Saving writes atomically (temp file + rename, preserving permissions) and
    refuses if the file changed on disk since it was loaded — `Ctrl+R` reloads.
  - Opening another file or quitting with unsaved edits warns first; repeat the
    action to discard.
  - Binary, unreadable and oversized files stay read-only.
- **Agent changes (right pane, S24):** splits into three — the **diff** on top
  (`↑↓` `PgUp` `PgDn` scroll · `Esc` back to chat, or restores the zoom when this
  pane is zoomed), an interactive **`$SHELL` terminal** (bottom-left) for system
  commands and background jobs, and a **command helper** (bottom-right) that
  recommends commands with Claude Haiku from the terminal's recent output.
  Click the terminal or helper to focus it; in the helper, type a question and
  press `Enter`; `Esc` returns to the diff.
  - **Sub-box zoom & minimize (S26):** each sub-box carries its own `[_]`
    minimize and `[□]` zoom buttons on its top border (click them even when the
    box is unfocused). With the **diff**, **terminal** or **helper** focused,
    `Ctrl+]` `z` **maximizes** that box to fill the whole Agent-changes pane
    (and zooms the pane itself so the box fills the screen; `z` again restores
    both), and `Ctrl+]` `m` **minimizes** it to a one-row title bar while the
    others grow — click the bar (or press `m` again) to restore it. The three
    boxes are never all minimized at once. A minimized or hidden terminal keeps
    its PTY at its last size, so the shell does not reflow. The pane's own
    `[_]`/`[□]` buttons still minimize/zoom the whole Agent-changes pane (S14).
- **Git graph (`Ctrl+]` `g`, S20):** a full overlay drawing the commit/branch
  DAG with branch/tag labels and the local worktrees; `↑↓`/`PgUp`/`PgDn` scroll,
  `Esc` closes. The data (git log + worktrees + open PRs) is collected on a
  background thread, so the overlay opens instantly showing *loading…* and fills
  when ready — the UI thread never blocks on git/`gh` (S32). Any branch label
  with an **open pull request** is marked ` ⇡PR #123` in a distinct amber
  (fetched off-thread via `gh pr list`; absent when `gh` is missing,
  unauthenticated or the remote is not GitHub) (T21b).
- **Conversations (`Ctrl+]` `n` or the *Conversaciones* apps row, S22):** manage
  several gentle-shell conversations, each its own child process; `↑↓` select,
  `Enter` switch, `n` new, `x` close, `Esc` back. The active one shows as
  `[n/total]` in the chat title.
- **Zoom & minimize (S14):** every pane's top border carries two small buttons at its right end — `[_]` minimize and `[□]` zoom (`[_]`/`[^]` with `GENTLE_PARTITIONER_ICONS=ascii`). A click runs that pane's action even when it is not focused. A **zoomed** pane fills the whole body and hides the others; `z` again, or `Esc` outside an editing context, restores it. A **minimized** pane collapses to a one-row title bar (the sidebar to a 3-column icon strip) while the rest grow; several panes may be minimized but never all four. Click a minimized title bar or strip to restore it. The status line shows the zoomed pane and the minimized ones. While the chat is minimized its PTY keeps its last size, so gentle-shell does not reflow.
- **Apps panel:** `↑↓` select an app or item · `Enter` drills in (app → items →
  reply box) · type to edit the reply, `Enter` sends it, `a` opens the action
  menu (e.g. *open*), `Esc` steps back one level and then to the chat. On a
  **GitHub issue/PR**, `Enter` opens the **in-terminal reader** overlay (wrapped,
  scrollable; `y` copies via OSC 52, `o` opens in the browser, `Esc` closes). **Click
  an app row to expand the panel and open that app.** The header row has its own
  controls: `[_]` collapses the panel to the app list, `[□]` expands it, and
  clicking the **Apps** title toggles the two — these are independent of the
  sidebar's `[_]` minimize (which collapses the whole sidebar). The local
  **Arquitectura** row trails the engine apps and opens the architecture overlay
  on `Enter`/click; its dot reflects the `gentle-arch` status. When expanded,
  `+`/`-` resize the panel between **40% and 70%** of the sidebar (default 45%,
  shown in the panel title); the file tree always keeps at least five rows.
- **Code selection badge:** while text is selected the editor shows a small
  **✿ ?** mascot badge after the selection (ASCII fallback `(?)`); click it, or
  press `Ctrl+E`, to ask the explainer about exactly that selection.
- **Overlays:** `↑↓` move · `Enter` open · `Esc` close.
- **Shift+Enter = newline (S28):** in the **chat** (gentle-shell child),
  `Shift+Enter` inserts a new line instead of sending — it is encoded as the
  CSI-u sequence `\x1b[13;2u`, which the Pi/gentle-shell editor maps to
  *newline* (`tui.input.newLine` defaults to `shift+enter`), while plain `Enter`
  still submits (`\r`). In every multi-line partitioner input — the **Arch
  brief**, the **command-helper** question, the **explain** follow-up and the
  **Slack reply** — `Shift+Enter` inserts a newline and never sends; the
  single-line **conversation title** ignores it. The Arch brief still submits on
  `Alt+Enter`/`Ctrl+Enter` (S23).
- **Mouse:** click selects panes and places the editor cursor, opens files and
  tools. The **wheel scrolls one line per event** (smooth; terminals emit
  several events per notch) over the chat, the editor, the diff, the sidebar,
  the **Agent-changes `$SHELL` terminal** (its own PTY scrollback) and the
  command helper, and over the **explain**, **reader**, **git-graph** and
  **arch-report** overlays — the open overlay captures the wheel so it never
  scrolls the panes behind it. The editor used to jump three lines per event
  and felt over-sensitive (S28).

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
- **`icons.rs`** — the Nerd Font (and ASCII fallback) glyph + colour for each
  tree entry (S4).
- **`highlight.rs`** — the per-language lexical highlighter, rainbow brackets
  and matching-bracket search for the editor pane (S5).
- **`predict.rs`** — the deterministic n-gram prediction engine, the hand-written
  JSON helpers, and the repo/history seeding (S9).
- **`predict_server.rs`** — the loopback HTTP server exposing the engine to the
  `gentle-shell` child with a bearer token (S9).
- **`apps_client.rs`** — supervises the independent `gentle-apps` process:
  spawn, a reader thread that decodes NDJSON into UI events, a writer with
  incrementing request ids, exponential-backoff restart, and kill-on-quit (S12).
- **`apps_panel.rs`** — the apps-panel state (app list, per-app items, reply box
  with UTF-8 horizontal scroll) and its message/ result handling (S12).
- **`explain_client.rs`** — supervises the independent `gentle-explain` process
  (spawn, NDJSON reader thread, writer, backoff restart, kill-on-quit), mirroring
  `apps_client.rs` (S17).
- **`arch_client.rs`** — supervises the independent `gentle-arch` process the
  same way, feeding review/design deltas, `done{path}`, errors and status into
  the UI channel (S18).
- **`keys.rs`** — encodes crossterm key events as the bytes a terminal sends to
  a PTY child.
- **`engines/`** — the `gentle-engines` workspace member: the Anthropic Messages
  client (`anthropic` — SSE streaming, model resolution, bounded retries), the
  keyring `keystore`, the NDJSON `protocol`, selection/context caps (`context`),
  the stdio explainer `engine`/`backend` and the `key` CLI. It also holds the
  architecture engine: the knowledge base (`kb` — HTML strip, chunking, BM25),
  the repo `scan` with secret redaction, the optional external `tools`, the
  `arch` review/design core and its `arch_engine`/`arch_protocol`. Builds the
  `gentle-explain` and `gentle-arch` binaries (S17, S18).
- **`apps/`** — the `gentle-apps` workspace member: the shared `protocol`
  (NDJSON message types), the `Connector` trait, the threaded stdio `engine`,
  and the live Slack connector with its browser OAuth/PKCE login (`pkce`,
  `oauth`, `http`, `store`, `config`, `slack`). Builds the `gentle-apps` binary
  (S12, S13).

## Roadmap

- **T7 (S8)** — translator engine: a pivot-through-English middleware around Pi,
  so the user always reads their native language while everything flows through
  English.
- **T8a (S9)** — prediction engine, local server and editor ghost text: **done**.
- **T8b (S9)** — gentle-shell prompt editor ghost suggestion from
  `GENTLE_PREDICT_URL` (dim inline, `Tab`/`Right` accept, learn submitted
  prompts): **done**.
- **T9 (S4)** — clicking an expanded folder collapses it again (click toggles
  like `Enter`): **done**.
- **T10 (S4)** — Nerd Font icons in the sidebar and overlays: **done**.
- **T11 (S5)** — editor syntax highlighting, rainbow brackets and matching
  bracket: **done**.
- **T12 (S12)** — apps engine (`gentle-apps`), the JSON-lines stdio protocol, the
  `Connector` trait and the bottom-left collapsible apps panel with a Slack
  placeholder: **done**.
- **T13 (S12, S13)** — the live Slack connector (browser OAuth + PKCE login,
  token in the system keyring, mentions of the user, reply in the thread):
  **done**.
- **T17 (S17)** — code explainer engine (`gentle-explain`): editor text
  selection, a streaming explanation popup with follow-up Q&A, Claude Haiku over
  the Anthropic API with the key in the keyring: **done**.

## Fast development loop

`Cargo.toml` optimizes dependencies once (`[profile.dev.package."*"] opt-level = 3`)
and keeps this crate unoptimized, so an edit rebuilds in under a second while the
UI stays fluid. Linux x86_64 links with `lld` by default since Rust 1.90.

- Iterate with `cargo run` (dev profile); use `cargo run --release` for the final binary.
- Optional: `cargo install bacon` and run `bacon test` for rebuild-on-save.
- Only if linking becomes slow: install `mold` and add `-C link-arg=-fuse-ld=mold` in `.cargo/config.toml`.
