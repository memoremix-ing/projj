# projj

Tools for "projj-style" roots: a git repository that humans use to review
code, on top of jj repositories that agents use to write it.

```
<root>/
  .git/                                  review state (human)
  track/<host>/<tenant>/<repo>.git/
    .repo/                               the jj repository; ignored by git, never edited
    sdk-0/                               jj workspace tracking major version 0
    sdk-1/                               jj workspace tracking major version 1
```

Today projj is one thing, a review tool for such a root, in two forms: the
`projj serve` command, which serves it to a browser, and a desktop app (in
[`desktop/`](desktop/README.md)) that needs no server and no terminal.

## Review model

The root git index is the review record.

| In git | In projj |
| --- | --- |
| Changed in the worktree, not staged | To review |
| Staged | Reviewed |
| Committed | Done; no longer shown |

Marking a hunk as reviewed stages exactly that hunk (`git apply --cached`), and
sending it back unstages it. The button for it, "Mark above reviewed", sits at
the foot of the hunk, where the reader is once they have read it; afterwards
the next hunk moves up into its place. Hunks alternate between two colored
stripes down their right edge, and the button wears its hunk's stripe, so it
is plain which hunk it marks. Selected lines within a hunk can be marked on
their own. projj never commits: committing the reviewed changes stays a
deliberate step in a terminal, and everything projj does to the index is
visible to, and undoable with, plain `git`.

Reviewing never changes a file. The only way projj writes to the worktree is
the editor described under Views, when you save in it, and a save stages
nothing: the edit is simply one more change to review.

jj history is read through `jj-lib`, without snapshotting working copies or
writing operations, so browsing cannot disturb an agent working in the same
workspace. The one exception is the "Snapshot working copy" button, which runs
`jj util snapshot` when you ask for it.

## Run

```sh
cargo run --release -- serve       # prints a link to open: http://127.0.0.1:4747/?token=…
cargo run -- list                  # repositories and workspaces under the root
cargo run -- drafts                # unsaved editor text kept on this device
cargo run -- jj log                # the jj built into projj
```

The root is found by asking git (`git rev-parse --show-toplevel`) which
repository contains the current directory, so `serve` works from anywhere
inside a root. If that repository has no `track/` directory (it is the git
store behind a `.repo`, say), the search continues from its parent. From
outside a root, pass `--root <path>` with any path inside one. `--port`
changes the port.

The server can change the git index and files under the root, so it lets in
only the browser it was meant for. It listens on `127.0.0.1` only, and prints
a link carrying a one-time token (32 random bytes). The first visit trades the
token for a session cookie, and the link stops working: nothing else on the
device, no other user of it, and no web page can use the server without a
link of its own. Press Enter in the terminal for a new link, to let in another
browser. Sessions last as long as the server, so a restarted server needs its
new link. On top of that, it refuses requests whose `Host` or `Origin` is not
its own address (DNS rebinding, cross-site writes), and asks browsers never to
show it in another site's frame.

`--data-dir` (or `PROJJ_DATA_DIR`) says where unsaved editor text and display
settings are kept; see "What is kept on the device".

`scripts/build-mac-universal.sh` builds one binary for both Apple-silicon and
Intel Macs (macOS 11 and 10.12 or later, respectively), in
`target/universal-apple-darwin/release/projj`. The page itself needs a browser
from 2023 or later: Chrome 111, Safari 16.2, Firefox 113.

Nothing else needs installing. projj uses the installed `git` when there is
one that runs (on a Mac, `/usr/bin/git` counts only once the developer tools
are installed), so it does whatever that git would do, filters and hooks
included. On a machine without one it uses its own, built on
[grit-lib](https://github.com/gitbutlerapp/grit): `src/git_builtin.rs`
answers each question projj asks of git as git would, and writes the index
as git would, holding `index.lock` throughout. The startup line and
`/api/root` (`"git"`) say which is in use; `PROJJ_GIT=installed` or
`PROJJ_GIT=built-in` picks one. Likewise jj needs no installing: projj has jj 0.43.0 built
in, the same version as the `jj-lib` it reads repositories with, and uses it
for the snapshot button. `projj jj …` runs it like any `jj`, for a machine
that has none. Agents may use a `jj` of their own; keep it at 0.43.0 too, so
that it and projj agree on the repository format.

## Which jj workspaces are shown

Nothing is registered or configured. A jj workspace is any directory under the
root that contains `.jj/` and that git does not ignore, wherever it sits; the
`track/` layout is a convention, not a requirement. The list is rebuilt from
disk as the page is used, so a workspace added or removed in a terminal
appears or disappears on its own.

Git does the looking (`git ls-files`), which makes its ignore rules the
filter: `.gitignore` files at every level, `.git/info/exclude`, and your
global excludes. Nothing inside an ignored directory is found, so
`node_modules/`, `target/`, and an ignored workspace are all left alone.

Each workspace is grouped under the jj repository it belongs to, read from
its `.jj/repo`: either the repository lives right there, or the file points at
the workspace that holds it. When the repository is kept somewhere that is not
itself listed, such as an ignored `.repo/` or a directory outside the root,
`projj list`, `/api/root` (`"detached": true`), and the History view say where
it is. That directory is not offered for browsing, because git ignores it; a
repository with no workspace outside its ignored store is therefore not shown
at all.

A directory with its own `.git` (a colocated jj repository) is listed if it is
not ignored, but git reports it as a single untracked entry and does not look
inside, so its files cannot be reviewed from the root and anything nested
deeper is not found.

## Views

**Review** lists changed files by workspace, with a diff stream beside them.
Within a workspace the files are in name order, a directory's own files ahead
of its subdirectories, with new files among the changed ones rather than
after them. Clicking a workspace's heading, in that list or above its diffs,
folds the workspace away in both places, or brings it back.
"To review" shows the worktree against the index, "Reviewed" shows the index
against `HEAD`. Edits made by agents while the page is open show up within a
few seconds. "History" on a file opens the jj changes that touched it.

Selecting text in a diff offers an Edit button, which opens the file in an
editor drawer below the diff, with that text selected. The diff stays in view
and usable above it, so what was there, what is there, and what you are about
to write can be read together; drag the drawer's top edge to resize it. The
Editor button in the top bar opens the drawer without a selection. The review
file list and the drawer's file explorer are as wide as you drag their right
edges (or move them with the arrow keys once focused); double-clicking an
edge puts the usual width back.

The drawer has a file explorer: the files you have open, then every file git
does not ignore, with a filter box and markers for files that have changes to
review or unsaved edits. Each open file keeps its own text, undo history and
scroll position, so switching between files loses nothing. Unsaved edits also
survive hiding the drawer, reloading the page, and restarting the server on
another port (see "What is kept on the device"). Saving writes the file to
disk, stages nothing, and refreshes the diff.

The editor is syntax highlighted too. The text area does the editing while a
colored copy of the visible lines sits behind it. The colors are worked out in
the page (`web/highlight.js`), a line at a time, so they keep up with typing
without asking the server. These grammars are small and hand-written: they
know comments, strings, numbers, keywords, and the names of functions and
types for some two dozen languages, and are less exact than the ones behind the
diffs. A file of a kind they do not know stays plain.

When something else writes to a file you have open:

- A file without unsaved edits simply shows the new content.
- A file with unsaved edits is never changed behind your back, and cannot be
  saved until you have looked. A dialog shows a three-way merge, with every
  diff taken against the content your edits started from: your edits, what
  changed on disk, and, when the two do not touch the same lines, both
  combined (merged by `jj-lib`). You can accept the combined version, keep
  only yours, take the one on disk, or decide later. Accepting puts the result
  in the editor, still unsaved.
- If the file changes again while the dialog is open, or in the instant before
  you accept, the dialog is redrawn for the new state and nothing is applied
  until you choose again.
- The dialog opens by itself when the file is the one you are editing, and
  when a save finds the file has moved. For other open files a notice waits on
  the file, so drafts restored after a long time do not ambush you at page
  load.

"Unsaved edits" in the editor header lists everything unsaved, including
edits brought back from an earlier session: when each was last edited, how
much it changes, and whether its file has changed on disk since. From there
you can review any of them as a diff, or discard them one by one or all at
once.

Line endings are kept as they were; files that are binary, not UTF-8, over
2 MB, or of mixed line endings are left to a real editor. The explorer opens
existing files only; it does not create, rename, or delete.

The Layout control switches every diff between unified and side by side. Side
by side puts each run of removed lines next to the lines that replaced it;
files that are entirely new or entirely deleted stay full width. Marking
lines and selecting text to edit work the same in both.

Within a changed line, the words that changed are marked more strongly,
however many there are. A line that is wholly new or gone, or shares no word
with the line it replaced, is marked from its first character to its last,
including every line of a file that is entirely new or deleted.

Tweaks, in the top bar, opens a panel beside the page for adjusting how diffs
read: how strongly changed lines are tinted and changed words marked, whether
changed words are underlined too, how much syntax color shows, and the size
and spacing of code in diffs and the editor. A sample diff in the panel shows
each setting, and the page behind it changes as you go. Light and dark each
have their own settings; the panel tunes whichever theme is showing, and can
switch between them. "Export or import as JSON" shows both themes' settings
in full, to copy, download, or edit and import again; an import may give
only some settings, and values out of range are brought within it.

Diffs are syntax highlighted by the server from the file's name, using the
grammars bundled with `syntect` and `two-face`. Each side of a diff is tokenized as a whole
file, so a hunk inside a block comment or a fenced code block is colored
correctly.

**History** shows a workspace's jj change graph for any revset (the default is
`revsets.log`), each change's diff, how a change evolved, and the operation
log. Shift-click a change, or use "Compare from here", to diff between two
changes.

| Key | Action |
| --- | --- |
| `j` `k` | Next or previous hunk (Review), change (History) |
| `r` | Mark the focused hunk, or its selected lines, reviewed |
| `e` | Edit the file at the selected text |
| `u` | Send the focused hunk back to review |
| `]` `[` | Next or previous file |
| `t` | Switch between To review and Reviewed |
| `1` `2` | Review, History |
| `/` | Edit the revset |

## What is kept on the device

Text typed in the editor and not yet saved, and display settings (theme,
layout, context, the Tweaks, the widths of the file lists, the editor's size
and whether its file list shows), are kept by
the server, not by the browser, in one [Turso](https://docs.turso.tech/)
database for the whole device:

| Platform | Default location |
| --- | --- |
| macOS | `~/Library/Application Support/projj/projj.db` |
| Linux | `$XDG_DATA_HOME/projj/projj.db`, or `~/.local/share/projj/projj.db` |
| Windows | `%LOCALAPPDATA%\projj\projj.db` |

`$XDG_DATA_HOME`, where it is set, wins on every platform. Every projj
server opens the same file. Settings belong to the person and are the same
for every root, port and browser; the browser keeps a copy only to paint the
right theme before the server answers. Each edit is filed under the
resolved path of the root it was made in. So edits follow the root: they come
back whichever port the root is served on and whichever browser opens it, and
two servers on two roots never see each other's. `serve` prints the location
when it starts, and `--data-dir` or `PROJJ_DATA_DIR` changes it.

`projj drafts` lists what is kept for every root, including roots that have
since been moved or deleted, which the web page can no longer reach.
`projj drafts --discard <root>` throws away what is kept for one root.

An edit reaches the store about a third of a second after the last keystroke.
Closing the page inside that window still hands the edit over; the browser
only asks before leaving when the latest edits are too large to hand over that
way, or the store is failing. If the store cannot be used, review carries on,
and the page says that edits are not being kept.

## HTTP API

Everything the web client does goes through this JSON API. From a script,
redeem a link as a browser would (follow `/?token=…` once and keep the
`projj-<port>` cookie it sets), then send that cookie. The desktop app makes
the same calls, in process. Errors are `{"error": "..."}` with status 400 (bad
input), 404 (unknown workspace), or 409 (the repository changed underneath the
request; refetch and retry).

| Method and path | Parameters | Result |
| --- | --- | --- |
| `GET /api/root` | | Root path, repositories, workspaces |
| `GET /api/review/status` | | Branch, `HEAD`, and changed files with line counts per side |
| `GET /api/review/diff` | `path`, `side` (`unstaged`\|`staged`), `context` | One file's hunks, or `null` |
| `POST /api/review/apply` | `{action, path, hunk, lines?, context?}` | Stage or unstage a hunk or some of its lines |
| `POST /api/review/apply-files` | `{action, paths}` | Stage or unstage whole files |
| `GET /api/file` | `path` | A worktree text file: `content`, `eol`, `version` |
| `POST /api/file` | `{path, content, eol, base?}` | Rewrite an existing file; 409 if it is no longer at version `base` |
| `POST /api/file/rebase` | `{path, base, content}` | Three-way merge of edited `content` and the file on disk, both started from `base`: `status` is `unchanged`, `merged` (with the result in `content`), or `conflict`, plus the diffs `mine`, `theirs`, `merged` against `base`. Writes nothing |
| `GET /api/settings` | | Display settings: `{settings}`, each a JSON value by name |
| `POST /api/settings` | `{key, value}` | Keep one setting; a `null` value forgets it |
| `GET /api/drafts` | | Unsaved edits kept for this root: `{store, drafts}`, each draft `{path, eol, version, original, content, edited}` |
| `POST /api/drafts` | `{path, eol, version, content, edited?, original?}` | Keep the latest unsaved text of a file. `original`, the content the edits started from, may be left out once it is stored for that `version`; 409 if it is needed |
| `DELETE /api/drafts` | `path` | Forget the unsaved edits to a file |
| `GET /api/files` | | Every file git does not ignore |
| `GET /api/jj/workspace` | `ws` | Working-copy change, latest operation, default revset |
| `GET /api/jj/log` | `ws`, `revset?`, `limit?` | Changes with graph edges |
| `GET /api/jj/file-log` | `ws`, `path`, `limit?` | Changes that touched a file |
| `GET /api/jj/diff` | `ws`, `to`, `from?`, `path?`, `context?` | Diff of a change, or between two |
| `GET /api/jj/evolution` | `ws`, `rev`, `limit?` | Earlier versions of a change |
| `GET /api/jj/operations` | `ws`, `limit?` | Operation log |
| `POST /api/jj/snapshot` | `ws` | Runs `jj util snapshot` |

`action` is `stage` or `unstage`. `ws` is a workspace path relative to the
root, as listed by `/api/root`. jj `path` values are relative to the workspace;
review `path` values are relative to the root. `hunk` is the `id` of a hunk
from `/api/review/diff`, fetched with the same `context`; `lines` are indexes
into that hunk's `lines`.

## Layout

| Path | Role |
| --- | --- |
| `src/root.rs` | Finds the root, its jj workspaces, and the repository behind each |
| `src/git.rs` | Review state: status, per-file diffs, patch building, staging |
| `src/git_builtin.rs` | The built-in git, for machines without one, and its tests against real git |
| `src/jj.rs` | jj history through `jj-lib`: revsets, graph, diffs, evolution |
| `src/diff.rs` | Diff model shared by both, with intra-line highlights |
| `src/syntax.rs` | Syntax tokens for diff lines |
| `src/files.rs` | Reading and saving worktree files for the editor |
| `src/bundled_jj.rs` | jj's command line, built in: `projj jj` and the snapshot button |
| `src/store.rs` | The device-wide store: unsaved editor text and display settings |
| `src/api.rs` | The API's routes, and `Local` for calling them in process |
| `src/gate.rs` | Who may use `projj serve`: one-time links, sessions, origin checks |
| `web/` | Client, embedded in the binary; plain HTML, CSS, and JS |
| `web/highlight.js` | Syntax tokens for the editor, computed in the page |
| `desktop/` | The desktop app: Electrobun, with projj in its Rust main process |
| `spikes/git-engines/` | Not built with projj: gitoxide and grit-lib writing the index, compared with git |

## Develop

```sh
cargo test
node --test web/highlight.test.mjs     # the editor's grammars; needs only Node
node scripts/ui-test.mjs               # the page itself, in Chrome; needs only Node
PROJJ_GIT=built-in node scripts/ui-test.mjs   # the same, on the built-in git
scripts/make-scratch-root.sh /tmp/projj-scratch-root [MANY]
PROJJ_DATA_DIR=/tmp/projj-scratch-data \
  cargo run -- serve --root /tmp/projj-scratch-root --port 4751 --web-dir web
```

Staging changes a real git index. Try anything that marks or unmarks hunks
against a scratch root, not against a root someone is reviewing in, and point
`PROJJ_DATA_DIR` somewhere disposable so that test edits stay out of the real
store. With `--web-dir`, the client is read from disk on each request, so
edits to `web/` show up on reload.

The client has no build step and no dependencies. `web/highlight.test.mjs` is
not served; it runs under Node's own test runner.

`scripts/ui-test.mjs` builds a scratch root with many changed files, serves it
from `web/` with a throwaway store, and drives an installed Chrome through the
DevTools Protocol, with no packages to install. It covers scrolling the diff
stream: that a file clicked in the sidebar lands at the top however far away
it is and however slowly the diffs around it arrive, that scrolling by hand
afterwards is left alone, that what is being read stays put as diffs above
it load, and that marking a hunk or a whole file reviewed brings up what
followed it in its place. Most run with and without the browser's scroll
anchoring, which Safari lacks. Others check that Tweaks apply at once, follow
to a second server on another port, stay apart for light and dark, and export
and import as JSON, and that a browser not given the link is shut out. The
harness redeems each server's link itself and hands the session to the
browser. `--headed` shows the browser, `--only <name>` picks
tests, `CHROME` and `PROJJ_BIN` choose the binaries.

## Known limits

- The built-in git (used only when no git is installed) can draw a diff's
  hunks differently from git: its line diff, from grit-lib, does not always
  find the smallest set of changes git's own does, so a file may show a few
  more changed lines than `git diff` would. What it stages is still exactly
  what was selected. It cannot stage a whole nested repository, and a clean
  filter that runs a program (git-lfs, say) still needs that program. It is
  also slower: status on a large root takes about twice as long.
- Sharing one database file between several server processes relies on a
  Turso mode that is still marked experimental
  (`experimental_multiprocess_wal`). Its coordination file, `projj.db-tshm`,
  may change format between Turso releases, so `turso` is pinned to an exact
  version: servers built against different versions should not run at the
  same time. The database file itself is in SQLite's format. The mode needs a
  local filesystem on a 64-bit Unix; elsewhere a second server cannot open
  the store while the first is running.
- Unsaved edits are filed by the root's path. Moving a root leaves its edits
  behind under the old path, where `projj drafts` still lists them.
- Two pages open on the same root do not see each other's unsaved edits until
  they are reloaded, and if both edit the same file the store keeps whichever
  typed last.

- Repository-level jj configuration is not read; only the user configuration
  (`JJ_CONFIG`, `~/.jjconfig.toml`, `~/.config/jj/config.toml`).
- Git renames are shown as a deletion plus an addition.
- Files whose type changed, and unmerged paths, can only be staged whole.
- The Evolution tab compares two versions of a change tree to tree; it does
  not rebase the older version first, as `jj evolog -p` does.
