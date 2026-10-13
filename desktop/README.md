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

Open the app. The first time it asks for a folder: pick a projj root, or any
folder inside one. Each window reviews one root; File > Open Folder… (⌘O)
opens another. The roots open when you quit come back next time. Settings
and unsaved edits are kept in the same device store as for `projj serve`,
so both see the same ones.

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
hutch run dev                  # build and open a dev build (with the debugging port)
hutch run build                # a stable build and .dmg, in build/ and artifacts/
```

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
- Open Folder…'s dialog, and the message shown for a folder outside any
  root, have not been tried yet: automation cannot click native dialogs.
