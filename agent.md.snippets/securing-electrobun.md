## Securing Electrobun apps

This comes from reading Electrobun 2.0.4's source and testing a 2.0.2 build
on macOS with CEF. Electrobun's core ships as a prebuilt binary, so recheck
against the source of the version you use.

### Webviews and bridges

- **Give every non-sandboxed webview an `internal_bridge` handler of your
  own.** Use a no-op if you use no webview tags. Without one, Electrobun's
  core acts on the page's internal requests itself:
  - `webviewTagInit` opens a webview on any URL, with any preload script, any
    partition, `sandbox: false` by default, and whatever navigation rules the
    request names, or none.
  - The `webviewTag*` messages act on any webview id, with no check of who
    owns it: `ExecuteJavascript`, `UpdateSrc`, `UpdateHtml`,
    `UpdatePreload`, `SetNavigationRules`, `OpenDevTools`, and others.

  That was tried: a page opened a webview on a remote site, and from it ran
  script in the app's main page. The handler in the Bun SDK forwards the
  same messages without an ownership check either. If you need webview
  tags, handle them yourself, and act only on ids the sending webview owns.
- **Use `sandbox: true` for any webview showing content you don't control.**
  Non-sandboxed webviews get both the host bridge and the internal bridge.
- **Answer host messages only from webview ids you created.** The core
  queues every webview's messages together.
- **Treat every script in a page as able to read its encryption key**
  (`window.__electrobunSecretKeyBytes`) and use its bridges. A script
  injected into the page costs as much as the bridge can do.

### `views://`

- Pages on `views://` get every permission, camera and microphone included,
  without a prompt, under both CEF and WKWebView.
- CEF registers the scheme as bypassing CSP, so don't count on a CSP.
- Never render untrusted content as HTML in a `views://` page, and never
  load remote content into a webview that has bridges.
- Paths are safe: `..`, encoded separators and drive letters are refused.
  Leave `appdata://` (`allowed_protocols.app_data`) off unless you need it,
  since it serves the app's data directory.

### Navigation

- Rules are globs; the last one that matches decides, and a leading `^`
  blocks. Start with `"^*"`, then allow your own views, for example
  `["^*", "views://app/*"]`.
- The rules cover every frame. Popups and `window.open` are refused, and
  reported as `new-window-open` events.
- They don't cover `fetch`, XHR or WebSockets, so a page can still send data
  out.

### Debugging

- CEF remote debugging is on by default for dev builds, at
  `127.0.0.1:9222` with no sign-in, and any local process can drive the
  page. Set `build.<os>.chromiumFlags["remote-debugging-port"]` to `false`,
  and turn it on per build through an environment variable. Packaged builds
  have it off by default; set it explicitly anyway.

### Loopback socket

- The core listens at `127.0.0.1`, from port 50000, on
  `/socket?webviewId=N`. It checks no `Origin`, so any process or website can
  connect, name a webview, and knock the page's connection off. Messages to
  the page then fall back to injected script, so nothing is lost.
- Messages are accepted only if encrypted with the webview's `secret_key`
  (AES-256-GCM). Give each webview its own random 32-byte key.
- Never turn on the plaintext transport (`setWebviewPlaintextHostTransport`):
  it takes unencrypted messages from any connection.
- The encryption doesn't stop replayed messages, but capturing any means
  watching loopback traffic, which needs root.

### Storage

- `persist:*` partitions keep cookies and `localStorage` on disk, in the
  app's own profile (on macOS, `~/Library/Application Support/<identifier>/`).
  That suits preferences, not secrets.
