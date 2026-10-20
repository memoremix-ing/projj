# projj

## Security: projj is a server on localhost

`projj serve` can stage changes and write files under a root. It listens on
loopback (`127.0.0.1` and `[::1]`), which every process and every user on the
device can reach, and which the browser shares with every other local server. Keep these properties
when changing it:

- **The API needs a session.** `src/gate.rs` guards everything under `/api/`.
  A link carries a one-time token after `#`, so the token never travels as
  part of an address. The page trades the token for a session at
  `/api/session`. Put anything that reads or changes state under `/api/`.
  The page's own files are open to anyone, so they must hold no secrets.
- **No cookies.** A browser scopes cookies to a host, not a port, so every
  other server on `127.0.0.1` the browser visits would receive them.
  `HttpOnly` hides a cookie from scripts, not from servers. Sessions live in
  `sessionStorage`, which belongs to one origin (and so one port), and travel
  in the `x-projj-session` header. Tabs share a session over a
  `BroadcastChannel`, and try any session offered before taking it, since a
  tab left open from an earlier run may offer a dead one.
- **Sessions live only in the server's memory**, so whatever a tab keeps is
  worthless after a restart, even to a later server on the same port. Keep
  secrets out of `localStorage` and IndexedDB, which outlive the tab and pass
  to whatever serves that port next. `localStorage` holds only `projj-settings`,
  a copy of the display settings. Treat it as untrusted when read, since
  another server on the port could have written it.
- **Keep the `Origin` checks.** Writes must be same-origin (`Origin`,
  `Sec-Fetch-Site`). A page on another port of `127.0.0.1` is same-site,
  though not same-origin, so `SameSite` reasoning does not protect projj.
  Send no CORS headers, and keep `frame-ancestors 'none'` and `nosniff`.
- **Never let repository content run as a page.** Every API answer is JSON. A
  file from a root served as HTML from projj's origin would run with the
  tab's session. The page builds its DOM with `h()` and `textContent`; never
  use `innerHTML` with file contents, diffs, or jjent's replies.
- **Hold both loopback addresses, or none.** `gate::listen` binds the port at
  `127.0.0.1` and `[::1]`, and projj will not start if another program holds
  either. Chrome tries `::1` first for `localhost` and `*.localhost` (tried),
  so a server holding only `127.0.0.1` would leave those names to whatever
  listens at `[::1]` on the same port. Never bind one address alone (unless
  the device has no IPv6 loopback, which then nobody can take), and never a
  wildcard address, which would reach beyond the device.
- **Answer only to own names.** `Host` must be `127.0.0.1`, `[::1]`,
  `localhost`, or a subdomain of `localhost`, with projj's port; browsers keep
  those names on the device, so no other name is safe from DNS rebinding.
  Each subdomain is its own origin and its own site: a write's `Origin` must
  be the very host it was sent to.
- **Serve pages only at the run's own name.** Each run makes up
  `projj-<128 random bits>.localhost` and prints links there; a browser
  request (one with `Sec-Fetch-Site`) to any other name gets only that
  address (421). Whatever held the port between runs could have installed a
  service worker on the origin it served (tried: it then answered in place of
  the next server on the port, which saw no requests), or left storage or a
  page listening on its `BroadcastChannel`. A name nobody knew before the run
  is out of reach of all of it, so never serve pages at a fixed name, and
  never make the name guessable.
- **The desktop app opens no port for the API.** The page reaches it over
  Electrobun's in-process bridge, and can navigate only to `views://projj/*`.
  The navigation rules cover every frame, and popups are refused, but they
  do not cover `fetch`.
- **Keep projj's own `internal_bridge` handler** (`desktop/src/main.rs`).
  Without one, Electrobun's core acts on a page's internal requests itself.
  It would open webviews on any address, with any preload script and no
  navigation rules. It would also let any webview run script in, load a URL
  into, or change the navigation rules of any other webview, by id, with no
  check of who owns it. That was tried: a webview opened on `example.com` set
  the title of projj's page. projj answers only its own windows' webviews,
  but a script running in projj's page has the whole API.
- **A script injected into the desktop page costs more than one in the
  browser.** Electrobun grants `views://` pages every permission, camera and
  microphone included, without asking. It also registers the scheme as
  bypassing CSP, so a CSP is no safeguard there.
- **Electrobun turns on CEF's remote debugging for dev builds.** That port has
  no sign-in, and any local process could drive the page through it. So
  `desktop/electrobun.config.ts` turns it off. If you need it, build once with
  `PROJJ_CEF_DEBUG_PORT=9222 hutch run dev`, then rebuild without it when done.
- **Electrobun's loopback socket** (`127.0.0.1`, from port 50000) checks no
  `Origin`, so any process or website can connect, name a webview id, and
  displace the page's connection. Replies then fall back to injected script,
  so nothing is lost. Only messages encrypted with the window's key are taken
  (AES-256-GCM), so `secret_key()` must stay random per window. Never turn on
  Electrobun's plaintext transport.
- **jjent stays read-only.** Codex runs with `sandbox: "read-only"`, refuses
  every approval, and has web search, MCP servers, apps and plugins turned
  off. Its page tools only show and highlight. Codex can still read anything
  the user can, so do not give it tools that write, stage, or reach the
  network.
