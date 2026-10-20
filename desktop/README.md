# projj desktop

projj's review page in an app of its own, for people who would rather not
run a server from a terminal. It is built with
[Electrobun](https://framework.blackboard.sh/electrobun) and draws the page
with Chromium (CEF), the engine `scripts/ui-test.mjs` checks the page in.

The page is the same one `projj serve` sends to a browser (`web` links to
`../web`), and the API behind it is the same `projj::api`, linked into the
app's Rust main process. What differs is how the page reaches it.

## Why it is safe to hand to someone

- **No server.** The page's requests go through the webview's message bridge
  to the main process and are answered there (`api::Local`). projj opens no
  port, so other programs and other users on the machine, and web pages,
  have nothing to connect to.
- **One page, nowhere else.** The window shows projj's own files from inside
  the app and nothing more: navigation rules block every address except
  `views://projj/*`, and the page's Content-Security-Policy lets only its own
  scripts run and its own files load, whatever text a diff carries.
  `window.open` and requests to other sites fail.
- **Electrobun's socket.** Electrobun's core always listens on a loopback
  port (from 50000 up) for pages that would rather send over a socket than
  the native bridge. projj's page never uses it. A message arriving there is
  only taken if it is sealed (AES-GCM) with the window's key, which the app
  makes from 32 random bytes per window, so another program can connect but
  not get anything done.
- **Packaged builds have no debugging port.** Dev builds (`hutch run dev`)
  turn on CEF's remote debugging at `127.0.0.1:9222`, through which anything
  on the machine can drive the page. That is what makes them testable; do
  not use them on a root you care about. Stable builds leave it off.

## Use

Open the app. The first time it shows the page for choosing a root, the
same one `projj serve` shows outside a repository: search the repositories
on the machine, type a path, or Choose folder… for the app's own folder
dialog, and start a new root in a folder that has none. Each window reviews
one root; the projj button at the top left opens a panel whose "Choose a
projj" goes back to choosing another;
File > New Window (⌘N) opens another chooser, and File > Open Folder… (⌘O)
goes straight to a folder. The roots open when you quit come back next
time. Edit > Read Aloud (⌘⇧U) reads the page aloud, as the page's own Read
aloud button does. Speech started from the menu is not started by a click
in the page, which Chromium allows only where media may play without one,
so the app lets its page do that; the page plays no media. Settings
and unsaved edits are kept in the same device store as for `projj serve`,
so both see the same ones.

jjent (see the main README) works the same in the app, given Codex on
the machine, which it looks for where installers put it, since the `PATH` of
an app started from the Finder is bare.

Nothing else needs installing: as with `projj`, jj is built in, and so is a
git, used when the machine has none. Apps started from the Finder get a bare
`PATH`, so the app looks for an installed git in the usual places too:
Homebrew, `/usr/local/bin`, `~/.cargo/bin`, `~/.local/bin` and Nix profiles.

## Build

Needs [Hutch](https://framework.blackboard.sh/electrobun/guides/quick-start/),
Electrobun's build tool, and Rust 1.92 (pinned in `electrobun.config.ts`,
since `jj-lib` needs 1.89 or later).

```sh
hutch electrobun sync          # once: fetches Electrobun's SDK into .hutch/
hutch run dev                  # build and open a dev build
hutch run build                # a stable build and .dmg, in build/ and artifacts/
```

Electrobun gives dev builds CEF's remote debugging port, which anything on the
device could use to drive the page, so `electrobun.config.ts` turns it off.
To debug, build with `PROJJ_CEF_DEBUG_PORT=9222 hutch run dev`, and build
again without it when done.

`Cargo.lock` is kept in step with `../Cargo.lock`, since Hutch builds with
`--locked`. To try the app on a scratch root without touching the real
store, set `PROJJ_DATA_DIR` and launch the built binary directly:

```sh
PROJJ_DATA_DIR=/tmp/projj-scratch-data \
  build/dev-macos-arm64/projj-dev.app/Contents/MacOS/launcher
```

## Known limits

- Built and tried on macOS (Apple silicon) only. Electrobun builds for the
  machine it runs on, so Windows and Linux builds need those machines.
- Not signed or notarized: macOS will warn whoever opens a copy downloaded
  from elsewhere. Electrobun can sign and notarize (`build.mac.codesign`,
  `notarize`) given an Apple Developer ID.
- No icon yet (`icon.iconset`).
- Running a stable build's `projj.app` installs it: it unpacks itself and
  keeps CEF's profile in `~/Library/Application Support/com.memoremix.projj/`.
- The page's Download button (in Tweaks) is hidden; Copy still works.
- The folder dialog (Choose folder… and Open Folder…) has not been tried
  yet: automation cannot click native dialogs. The rest of choosing roots
  was tried in a dev build: opening one, going back to choose, and reopening
  at the next start.
