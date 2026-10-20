## Securing servers on localhost

Every process and every user on the device can reach a loopback server, and
the browser shares `localhost` among all local servers. Treat such a server as
exposed, and treat its port as something another program can take whenever
yours is not running.

### Binding

- Bind loopback only, never a wildcard address (`0.0.0.0`, `::`), which
  reaches beyond the device.
- Hold the port at both `127.0.0.1` and `[::1]`, or refuse to start. Chrome
  (tried) sends `localhost` and every `*.localhost` to `[::1]` first, so a
  server holding only `127.0.0.1` loses those names to whatever listens at
  `[::1]` on the same port. Bind both even if you print `127.0.0.1` links,
  since people type `localhost`. On macOS, a rival can still bind the
  wildcard addresses, but connections go to the more specific binding, so
  yours still answers. If the device has no IPv6 loopback, nobody can take
  `[::1]`, and `127.0.0.1` alone is fine.
- For "any free port", bind one family on port 0, bind the other on the port
  you got, and retry with a new port if that one is taken.
- To test it, have another process hold `[::1]` on your port and check that
  your server refuses to start.

### Names

- Check `Host` on every request. Accept only `127.0.0.1`, `[::1]`,
  `localhost` and `*.localhost`, each with your port. Browsers resolve
  `*.localhost` on the device, so any other name may be DNS rebinding.
- Subdomains of `localhost` are separate origins, and in Chrome (tried)
  separate sites, so they can keep pages served from one port apart. Route
  by `Host`, and answer only the subdomains you serve.
- Different loopback addresses (`127.0.0.2`, …) are separate sites too, but
  macOS needs root to add them (`ifconfig lo0 alias`), and the alias is lost
  on reboot.

### Sessions

- Require a secret for anything that reads or changes state. Hand it out
  through another channel, such as a link printed in a terminal. Make the
  token single-use and put it after `#`, which never reaches the server, its
  logs, or `Referer`. Remove it from the address with
  `history.replaceState`.
- Don't use cookies. Browsers scope cookies to a host, not a port, so every
  other server on that host receives them; `HttpOnly` hides them from
  scripts, not from servers. Pages on sibling ports can also set or
  overwrite them.
- Keep the session in `sessionStorage` (one origin, which includes the port,
  and one tab), and send it in a custom header. A custom header on a
  cross-origin request also forces a CORS preflight, which you don't answer.
- Share a session between tabs with a `BroadcastChannel`, which is scoped to
  the origin. Test an offered session against the server before taking it.
- Keep sessions only in the server's memory, so what a browser kept is
  worthless after a restart, even to a later program on the same port.
- Keep secrets out of `localStorage` and IndexedDB. They persist and pass to
  whatever serves that origin next. Treat what you read from them as
  untrusted.

### Requests from other pages

- A page on another port of the same host is same-site but cross-origin, so
  `SameSite` protects nothing here. For anything but GET and HEAD, require
  `Origin` to be exactly the address the request was sent to, and
  `Sec-Fetch-Site` to be `same-origin` (or absent, for non-browser clients).
- GET and HEAD must change nothing.
- Send no CORS headers you don't need.
- Send `X-Frame-Options: DENY`, `Content-Security-Policy: frame-ancestors
  'none'`, `X-Content-Type-Options: nosniff` and `Referrer-Policy:
  no-referrer`.

### Content

- Never serve content you don't control (files, user data, model output) as
  HTML, SVG or script from the app's origin, where it would run with the
  app's session. Serve it as JSON, and render it with `textContent`, never
  `innerHTML`.

### A new origin every run

- Whatever holds your port while you are not running can serve pages at
  your origin. Browsers count `localhost` and `127.0.0.1` as secure, so such
  a page can register a service worker. In Chrome (tried), the worker then
  answered every later request to that origin, including a link with
  `#token=…`, while the real server on the port saw none. It can also leave
  storage behind, or keep a page open that listens on your
  `BroadcastChannel` for sessions. A browser can be sent there by a stale
  tab, a typed address, or any website opening a window.
- So serve pages at a name made up for each run, with at least 128 random
  bits (`app-<hex>.localhost`). Print links there, and answer page requests
  at any other name with that address only. Nothing left at earlier names
  can reach the new one, and nobody can prepare the name in advance. Scripts
  can keep using the loopback addresses: tell them apart by the absent
  `Sec-Fetch-Site`.
- Open tabs still talk to whatever takes the port after the server stops,
  and send it anything they send by themselves (autosaves, drafts). Keep
  sessions in server memory so nothing they send is reusable. If that data
  matters, authenticate the server before sending it (below).

### Authenticating the server

- HTTPS helps only with a certificate whose key an impostor can't read.
  - A local CA (mkcert) works against other users and containers. It needs
    a root installed in the trust store (and Firefox's own store), and that
    root can forge any site unless it is restricted to local names.
  - A self-signed certificate that the user clicks through authenticates
    nothing.
  - A public certificate shipped in the app ships its key.
  - `http://localhost` is already a secure context, so HTTPS adds no
    browser features.
- Cheaper: the printed link is already a trusted path from the server to the
  browser. Put a secret `k` after `#` that never goes over the wire. Each
  request carries a nonce, and the server returns `HMAC(k, nonce ‖ status ‖
  body)`, which the page checks with WebCrypto before trusting anything.
  Requests are signed the same way, with a counter, so no bearer token is
  sent at all. Encrypt bodies with AES-GCM under a key derived from `k` if
  they are private. No public keys and no long-lived channel are needed:
  each `fetch` stands alone. One catch: WebCrypto is async, so a request
  sent while the page closes must be prepared in advance.
- Nothing protects a tab running code that an impostor served. Per-run
  origins keep that to dead origins.

### Debugging ports

- Remote debugging (`--remote-debugging-port`, `--inspect`, CEF's dev
  default) listens on loopback with no sign-in, so any local process can
  drive the page or the process. Keep it off by default, and turn it on for
  one run at a time.
