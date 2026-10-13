#!/usr/bin/env node
// Browser tests for the review page, driven through the Chrome DevTools
// Protocol with nothing but Node (22 or later) and an installed Chrome.
//
//   node scripts/ui-test.mjs [--headed] [--keep] [--only NAME]
//
// Builds a scratch root with many changed files, serves it from the current
// `web/` directory with a throwaway store, and checks the page in a browser.
// CHROME names the browser binary and PROJJ_BIN the server binary; by default
// the first Chrome found and target/release/projj (or target/debug/projj).

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SDK = dirname(dirname(fileURLToPath(import.meta.url)));
const args = process.argv.slice(2);
const headed = args.includes("--headed");
const keep = args.includes("--keep");
const only = args.includes("--only") ? args[args.indexOf("--only") + 1] : null;

const CHROMES = [
  process.env.CHROME,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
];
const SERVERS = [process.env.PROJJ_BIN, join(SDK, "target/release/projj"), join(SDK, "target/debug/projj")];

const found = (list, what) => list.find((path) => path && existsSync(path)) || fail(`no ${what} found`);
function fail(message) {
  throw new Error(message);
}

/** The link `projj serve` prints, and the address in it. */
const LINK = /projj: open (http:\/\/[^/\s]+)\/\?token=[0-9a-f]{64}/;

/** Starts a process and resolves with the first match of `pattern` in its output. */
function launch(command, argv, env, pattern) {
  const child = spawn(command, argv, { env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"] });
  return new Promise((resolve, reject) => {
    let seen = "";
    const timer = setTimeout(() => reject(new Error(`${command} did not start:\n${seen}`)), 20000);
    const look = (chunk) => {
      seen += chunk;
      const match = pattern.exec(seen);
      if (match) {
        clearTimeout(timer);
        resolve({ child, match });
      }
    };
    child.stdout.on("data", look);
    child.stderr.on("data", look);
    child.on("exit", (code) => reject(new Error(`${command} exited with ${code}:\n${seen}`)));
  });
}

/** The smallest DevTools Protocol client that will do: one page, by session. */
async function devtools(url) {
  const socket = new WebSocket(url);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = () => reject(new Error(`cannot connect to ${url}`));
  });
  let next = 0;
  const waiting = new Map();
  socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    const pending = waiting.get(message.id);
    if (!pending) return;
    waiting.delete(message.id);
    if (message.error) pending.reject(new Error(message.error.message));
    else pending.resolve(message.result);
  };
  const send = (method, params = {}, sessionId) =>
    new Promise((resolve, reject) => {
      const id = ++next;
      waiting.set(id, { resolve, reject });
      socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  return { send, close: () => socket.close() };
}

async function main() {
  const work = mkdtempSync(join(tmpdir(), "projj-ui-test-"));
  const processes = [];
  try {
    const root = join(work, "root");
    const made = spawnSync(join(SDK, "scripts/make-scratch-root.sh"), [root, "120"], { encoding: "utf8" });
    if (made.status !== 0) fail(`make-scratch-root.sh failed:\n${made.stderr}`);

    const server = await launch(
      found(SERVERS, "projj binary (build it first)"),
      ["serve", "--root", root, "--port", "0", "--web-dir", join(SDK, "web")],
      { PROJJ_DATA_DIR: join(work, "data") },
      LINK,
    );
    processes.push(server.child);
    const site = server.match[1];
    // A second server on the same device store, as on another port.
    const other = await launch(
      found(SERVERS, "projj binary (build it first)"),
      ["serve", "--root", root, "--port", "0", "--web-dir", join(SDK, "web")],
      { PROJJ_DATA_DIR: join(work, "data") },
      LINK,
    );
    processes.push(other.child);
    const site2 = other.match[1];

    const chrome = await launch(
      found(CHROMES, "Chrome (set CHROME)"),
      [
        ...(headed ? [] : ["--headless=new"]),
        "--remote-debugging-port=0",
        `--user-data-dir=${join(work, "profile")}`,
        "--no-first-run",
        "--no-default-browser-check",
        "--window-size=1280,800",
        "about:blank",
      ],
      {},
      /DevTools listening on (ws:\/\/\S+)/,
    );
    processes.push(chrome.child);
    const browser = await devtools(chrome.match[1]);
    // Each server prints one link, good once. The harness follows it itself
    // and gives the browser the same session, so both can use the server.
    const cookies = {};
    for (const { match } of [server, other]) {
      const response = await fetch(match[0].slice("projj: open ".length), { redirect: "manual" });
      const [pair] = (response.headers.get("set-cookie") || fail(`no session from ${match[1]}`)).split(";");
      const [name, value] = pair.split("=");
      cookies[match[1]] = pair;
      await browser.send("Storage.setCookies", { cookies: [{ name, value, domain: "127.0.0.1", path: "/", httpOnly: true }] });
    }
    const call = (site, path, init = {}) => fetch(`${site}${path}`, { ...init, headers: { ...init.headers, cookie: cookies[site] } });

    let failures = 0;
    for (const [name, run] of Object.entries(TESTS)) {
      if (only && !name.includes(only)) continue;
      const { targetId } = await browser.send("Target.createTarget", { url: "about:blank" });
      const { sessionId } = await browser.send("Target.attachToTarget", { targetId, flatten: true });
      const page = pageApi(browser, sessionId);
      // Every test starts with nothing marked reviewed, and stock settings.
      spawnSync("git", ["-C", root, "reset", "-q"]);
      const { settings } = await (await call(site, "/api/settings")).json();
      for (const key of Object.keys(settings)) {
        await call(site, "/api/settings", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ key, value: null }) });
      }
      try {
        await run(page, site, site2, { browser, server });
        console.log(`ok    ${name}`);
      } catch (err) {
        failures++;
        console.log(`FAIL  ${name}\n      ${err.message.replaceAll("\n", "\n      ")}`);
      } finally {
        if (!headed || !keep) await browser.send("Target.closeTarget", { targetId });
      }
    }
    browser.close();
    if (failures) {
      process.exitCode = 1;
      console.log(`${failures} failed`);
    }
  } finally {
    if (!keep) {
      // The browser must be gone before its profile can be removed.
      await Promise.all(processes.map((child) => new Promise((resolve) => (child.exitCode !== null ? resolve() : (child.once("exit", resolve), child.kill())))));
      rmSync(work, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } else {
      console.log(`kept ${work}; stop the server and browser yourself`);
    }
  }
}

function pageApi(browser, sessionId) {
  const send = (method, params) => browser.send(method, params, sessionId);
  const page = {
    send,
    /** Evaluates `source` (an expression, or an async function body) in the page. */
    async run(source) {
      const expression = `(async () => { ${source} })()`;
      const { result, exceptionDetails } = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (exceptionDetails) fail(exceptionDetails.exception?.description || exceptionDetails.text);
      return result.value;
    },
    async open(url, { latency = 0, anchoring = true } = {}) {
      await send("Page.enable");
      // Diffs of a real root take a while to compute; a delay makes the
      // page's handling of late arrivals show.
      if (latency) {
        await send("Page.addScriptToEvaluateOnNewDocument", {
          source: `{
            const real = window.fetch;
            window.fetch = async (input, init) => {
              if (String(input).includes("/api/review/diff")) await new Promise((r) => setTimeout(r, ${latency} * (0.5 + Math.random())));
              return real(input, init);
            };
          }`,
        });
      }
      // Safari, for one, has no scroll anchoring: content growing above the
      // view moves what is in view.
      if (!anchoring) {
        await send("Page.addScriptToEvaluateOnNewDocument", {
          source: `addEventListener("DOMContentLoaded", () => document.head.append(Object.assign(document.createElement("style"), { textContent: "* { overflow-anchor: none !important; }" })));`,
        });
      }
      await send("Page.navigate", { url });
      await page.waitFor(`document.querySelectorAll("#tree .file-row").length > 0`);
    },
    async waitFor(condition, timeout = 10000) {
      const until = Date.now() + timeout;
      while (Date.now() < until) {
        if (await page.run(`return Boolean(${condition});`)) return;
        await sleep(50);
      }
      fail(`timed out waiting for ${condition}`);
    },
    /** Clicks the middle of the first element matching `selector`, as a person would. */
    async click(selector, text) {
      const box = await page.run(`
        const el = [...document.querySelectorAll(${JSON.stringify(selector)})].find((el) => el.textContent.includes(${JSON.stringify(text || "")}));
        if (!el) return null;
        el.scrollIntoView({ block: "nearest" });
        const r = el.getBoundingClientRect();
        return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
      `);
      if (!box) fail(`nothing to click: ${selector} ${text || ""}`);
      for (const type of ["mousePressed", "mouseReleased"]) {
        await send("Input.dispatchMouseEvent", { type, ...box, button: "left", clickCount: 1 });
      }
    },
    async press(key) {
      for (const type of ["keyDown", "keyUp"]) await send("Input.dispatchKeyEvent", { type, key, text: type === "keyDown" ? key : undefined });
    },
    /** Drags the middle of `selector` sideways by `dx`, in a few steps. */
    async drag(selector, dx) {
      const box = await page.run(`const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 };`);
      await send("Input.dispatchMouseEvent", { type: "mousePressed", ...box, button: "left", clickCount: 1 });
      for (let step = 1; step <= 5; step++) {
        await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: box.x + (dx * step) / 5, y: box.y, button: "left", buttons: 1 });
      }
      await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: box.x + dx, y: box.y, button: "left", clickCount: 1 });
    },
    async wheel(selector, deltaY) {
      const box = await page.run(`const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 };`);
      await send("Input.dispatchMouseEvent", { type: "mouseWheel", ...box, deltaX: 0, deltaY });
    },
  };
  return page;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---- scrolling to a file from the sidebar

/**
 * Where a file's card sits after its name is clicked in the sidebar, once
 * everything has finished loading: the gap between the top of the diff
 * pane and the top of the card, or null if the pane could not scroll that
 * far because the list ends first.
 */
const PLACE = `
  const stream = document.querySelector("#stream");
  const card = [...stream.children].find((el) => el.dataset.key === path);
  const gap = Math.round(card.getBoundingClientRect().top - stream.getBoundingClientRect().top);
  const atEnd = stream.scrollTop + stream.clientHeight >= stream.scrollHeight - 1;
  return { gap, atEnd, pending: stream.querySelectorAll(".file-body.pending").length };
`;

/** Waits for the card to stop moving, then says where it ended up. */
async function settledPlace(page, path) {
  let last = null;
  let still = 0;
  for (let i = 0; i < 200 && still < 8; i++) {
    await sleep(50);
    const place = await page.run(`const path = ${JSON.stringify(path)}; ${PLACE}`);
    still = last && place.gap === last.gap ? still + 1 : 0;
    last = place;
  }
  return last;
}

/** Clicks a button inside the diff card of `path`, as a person would. */
async function clickInCard(page, path, label) {
  const box = await page.run(`
    const card = [...document.querySelector("#stream").children].find((el) => el.dataset.key === ${JSON.stringify(path)});
    const button = [...card.querySelectorAll("button")].find((b) => b.textContent === ${JSON.stringify(label)} && b.getBoundingClientRect().height);
    if (!button) return null;
    // As a reader would scroll to it.
    button.scrollIntoView({ block: "nearest", behavior: "instant" });
    const r = button.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  `);
  if (!box) fail(`no "${label}" button in ${path}`);
  for (const type of ["mousePressed", "mouseReleased"]) {
    await page.send("Input.dispatchMouseEvent", { type, ...box, button: "left", clickCount: 1 });
  }
}

/** The card after `path` in the stream, and how many hunks `path` has. */
async function cardFacts(page, path) {
  return page.run(`
    const cards = [...document.querySelector("#stream").children];
    const at = cards.findIndex((el) => el.dataset.key === ${JSON.stringify(path)});
    return { next: cards[at + 1]?.dataset.key, hunks: at < 0 ? 0 : cards[at].querySelectorAll(".hunk").length };
  `);
}

/** Waits until `path` has left the stream. */
async function gone(page, path) {
  await page.waitFor(`![...document.querySelector("#stream").children].some((el) => el.dataset.key === ${JSON.stringify(path)})`);
}

async function sidebarPaths(page) {
  return page.run(`return [...document.querySelectorAll("#tree .file-row")].map((row) => [row.dataset.path, row.textContent]);`);
}

/** Clicks each of `picks` (indexes into the sidebar) and checks where it lands. */
async function jumpAround(page, picks) {
  const paths = await sidebarPaths(page);
  const misses = [];
  for (const index of picks) {
    const [path, label] = paths[(index + paths.length) % paths.length];
    await page.click("#tree .file-row", label);
    const place = await settledPlace(page, path);
    // The card's top edge lands just under the top of the pane, unless the
    // list ran out first.
    if (!(place.gap >= 0 && place.gap <= 12) && !(place.atEnd && place.gap > 0)) misses.push(`${path}: ${JSON.stringify(place)}`);
  }
  if (misses.length) fail(`landed in the wrong place:\n${misses.join("\n")}`);
}

/** The first thing in view in the diff stream, and where it is on screen. */
const READING = `
  const stream = document.querySelector("#stream");
  const top = stream.getBoundingClientRect().top;
  const el = [...stream.children].find((el) => el.getBoundingClientRect().bottom > top + 1);
  return { key: el.dataset.key, top: Math.round(el.getBoundingClientRect().top) };
`;
const WHERE = `
  const el = [...document.querySelector("#stream").children].find((el) => el.dataset.key === key);
  return el && Math.round(el.getBoundingClientRect().top);
`;

async function setTweak(page, label, value) {
  await page.run(`
    const input = [...document.querySelectorAll("#tweaks .tweak")].find((el) => el.textContent.includes(${JSON.stringify(label)})).querySelector("input");
    input.value = ${value};
    input.dispatchEvent(new Event("input"));
  `);
}

async function tweakShown(page, label) {
  return page.run(`return [...document.querySelectorAll("#tweaks .tweak")].find((el) => el.textContent.includes(${JSON.stringify(label)})).querySelector("output").textContent;`);
}

async function importJson(page, json) {
  await page.run(`
    const text = document.querySelector("#tweaks-json-text");
    text.value = ${JSON.stringify(json)};
    text.dispatchEvent(new Event("input"));
  `);
  await page.click("#tweaks-apply");
}

const FAR = [-1, 0, -2, 40, 3, 90, 10, -10, 60];

const TESTS = {
  "a browser that was not given the link is shut out": async (page, site, site2, { browser }) => {
    // A browser context of its own has none of the harness's cookies.
    const { browserContextId } = await browser.send("Target.createBrowserContext");
    const { targetId } = await browser.send("Target.createTarget", { url: "about:blank", browserContextId });
    const { sessionId } = await browser.send("Target.attachToTarget", { targetId, flatten: true });
    const stranger = pageApi(browser, sessionId);
    try {
      await stranger.send("Page.enable");
      await stranger.send("Page.navigate", { url: site });
      await stranger.waitFor(`document.readyState === "complete" && document.body`);
      const seen = await stranger.run(`
        const api = await fetch("/api/root");
        const write = await fetch("/api/settings", { method: "POST", headers: { "content-type": "application/json" }, body: '{"key":"theme","value":"dark"}' });
        return { text: document.body.innerText, api: api.status, write: write.status, app: Boolean(document.querySelector("#stream")) };
      `);
      if (seen.app || !seen.text.includes("Open the link")) fail(`the app showed: ${JSON.stringify(seen)}`);
      if (seen.api !== 401 || seen.write !== 401) fail(`the API answered ${seen.api} and ${seen.write}`);
      // A spent link is no use to it either.
      await stranger.send("Page.navigate", { url: `${site}/?token=${"0".repeat(64)}` });
      await stranger.waitFor(`document.body?.innerText.includes("used already")`);
    } finally {
      await browser.send("Target.closeTarget", { targetId });
      await browser.send("Target.disposeBrowserContext", { browserContextId });
    }
  },
  "clicking a far-off file scrolls it to the top": async (page, site) => {
    await page.open(site, { latency: 120 });
    await jumpAround(page, FAR);
  },
  "... without scroll anchoring (as in Safari)": async (page, site) => {
    await page.open(site, { latency: 120, anchoring: false });
    await jumpAround(page, FAR);
  },
  "... with diffs that arrive slowly": async (page, site) => {
    await page.open(site, { latency: 600, anchoring: false });
    await jumpAround(page, [70, 5, -3]);
  },
  "scrolling by hand after a click is not undone": async (page, site) => {
    await page.open(site, { latency: 400, anchoring: false });
    const paths = await sidebarPaths(page);
    const [, label] = paths[80];
    await page.click("#tree .file-row", label);
    // Turn the wheel while diffs are still arriving.
    await sleep(150);
    await page.wheel("#stream", 900);
    await sleep(100);
    const reading = await page.run(READING);
    await sleep(2000);
    const later = await page.run(`const key = ${JSON.stringify(reading.key)}; ${WHERE}`);
    if (Math.abs(later - reading.top) > 1) fail(`${reading.key} moved from ${reading.top} to ${later} after the person scrolled`);
  },
  "what is being read stays put while diffs above it arrive": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    const [path, label] = paths[60];
    await page.click("#tree .file-row", label);
    await settledPlace(page, path);
    // Scroll up through files that have not loaded yet.
    for (let i = 0; i < 6; i++) {
      await page.wheel("#stream", -500);
      await sleep(30);
    }
    const reading = await page.run(READING);
    await sleep(1500);
    const later = await page.run(`const key = ${JSON.stringify(reading.key)}; ${WHERE}`);
    if (Math.abs(later - reading.top) > 1) fail(`${reading.key} moved from ${reading.top} to ${later}`);
  },
  "the next-file key lands on the next file": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    await page.click("#tree .file-row", paths[50][1]);
    await settledPlace(page, paths[50][0]);
    for (const next of [51, 52, 53]) {
      await page.press("]");
      const place = await settledPlace(page, paths[next][0]);
      if (!(place.gap >= 0 && place.gap <= 12)) fail(`${paths[next][0]}: ${JSON.stringify(place)}`);
    }
  },
  "folding the workspace being read puts its heading in view": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    await page.click("#tree .file-row", paths[40][1]);
    await settledPlace(page, paths[40][0]);
    await page.click("#tree .group-row", "widgets");
    const head = await settledPlace(page, "group:track/example.com/acme/widgets.git/sdk-0");
    if (!(head.gap >= 0 && head.gap <= 12) && !head.atEnd) fail(`heading at ${JSON.stringify(head)}`);
    // Folded, the list may end before the heading can reach the top; once
    // unfolded, it goes there, with the workspace's first file below it.
    await page.click("#stream .group-toggle", "widgets");
    const first = await settledPlace(page, "group:track/example.com/acme/widgets.git/sdk-0");
    if (!(first.gap >= 0 && first.gap <= 12)) fail(`heading at ${JSON.stringify(first)} after unfolding`);
  },

  // ---- reading settings

  "tweaks change the diffs at once, follow to another port, and reset": async (page, site, site2) => {
    const ready = `document.querySelector("#stream .line.add mark")`;
    await page.open(site);
    await page.waitFor(ready);
    const look = `
      const style = (selector) => getComputedStyle(document.querySelector(selector));
      return { line: style("#stream .line.add").backgroundColor, mark: style("#stream .line.add mark").backgroundColor, font: style("#stream .lines").font };
    `;
    const stock = await page.run(look);
    await page.click("#tweaks-open");
    await setTweak(page, "Changed line background", 20);
    await setTweak(page, "Code size", 15);
    const tweaked = await page.run(look);
    if (tweaked.line === stock.line || !tweaked.font.startsWith("15px")) fail(`no change: ${JSON.stringify({ stock, tweaked })}`);
    if (tweaked.mark !== stock.mark) fail("the word marks changed with the line background");
    // Settings reach the server once they settle.
    await sleep(600);
    await page.open(site2);
    await page.waitFor(ready);
    const elsewhere = await page.run(look);
    if (JSON.stringify(elsewhere) !== JSON.stringify(tweaked)) fail(`not on the other port: ${JSON.stringify({ tweaked, elsewhere })}`);
    await page.click("#tweaks-open");
    await page.click("#tweaks-reset");
    const reset = await page.run(look);
    if (JSON.stringify(reset) !== JSON.stringify(stock)) fail(`not reset: ${JSON.stringify({ stock, reset })}`);
  },
  "the file lists drag wider, stay so on another port, and double-click back": async (page, site, site2) => {
    const widths = `return { side: document.querySelector("#review .side").getBoundingClientRect().width, explorer: document.querySelector("#explorer").getBoundingClientRect().width };`;
    const open = async (url) => {
      await page.open(url);
      await page.waitFor(`document.querySelector("#tree .file-row")`);
      await page.click("#editor-toggle");
      await page.waitFor(`!document.querySelector("#editor").hidden`);
    };
    await open(site);
    const stock = await page.run(widths);
    await page.drag("#review .side .width-handle", 120);
    await page.drag("#explorer .width-handle", 90);
    const dragged = await page.run(widths);
    if (Math.abs(dragged.side - stock.side - 120) > 1 || Math.abs(dragged.explorer - stock.explorer - 90) > 1) fail(`dragged from ${JSON.stringify(stock)} to ${JSON.stringify(dragged)}`);
    // However far it is dragged, the diffs keep room beside the list.
    await page.drag("#review .side .width-handle", 5000);
    const stream = await page.run(`return document.querySelector("#stream").getBoundingClientRect().width;`);
    if (stream < 300) fail(`the diffs were squeezed to ${stream}px`);
    // And it can be brought back by dragging the other way.
    await page.drag("#review .side .width-handle", dragged.side - (await page.run(widths)).side);
    const settled = await page.run(widths);
    await sleep(600);
    await open(site2);
    const elsewhere = await page.run(widths);
    if (JSON.stringify(elsewhere) !== JSON.stringify(settled)) fail(`not on the other port: ${JSON.stringify({ settled, elsewhere })}`);
    for (const handle of ["#review .side .width-handle", "#explorer .width-handle"]) {
      const box = await page.run(`const r = document.querySelector(${JSON.stringify(handle)}).getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 };`);
      for (const clickCount of [1, 2]) {
        for (const type of ["mousePressed", "mouseReleased"]) await page.send("Input.dispatchMouseEvent", { type, ...box, button: "left", clickCount });
      }
    }
    const reset = await page.run(widths);
    if (JSON.stringify(reset) !== JSON.stringify(stock)) fail(`not reset: ${JSON.stringify({ stock, reset })}`);
  },
  "light and dark keep their own tweaks": async (page, site) => {
    await page.open(site);
    await page.click("#tweaks-open");
    await page.click("#tweaks-theme button", "Light");
    await setTweak(page, "Changed line background", 20);
    await page.click("#tweaks-theme button", "Dark");
    const dark = await tweakShown(page, "Changed line background");
    if (dark !== "100%") fail(`dark started at ${dark}, not its own default`);
    await setTweak(page, "Changed line background", 60);
    await page.click("#tweaks-theme button", "Light");
    const light = await tweakShown(page, "Changed line background");
    if (light !== "20%") fail(`light shows ${light} after dark was changed`);
    await sleep(600);
    await page.open(site);
    const kept = await page.run(`return document.documentElement.dataset.theme;`);
    if (kept !== "light") fail(`the theme came back as ${kept}`);
  },
  "tweaks export and import as JSON": async (page, site) => {
    await page.open(site);
    await page.click("#tweaks-open");
    await page.click("#tweaks-theme button", "Light");
    await page.click("#tweaks-json summary");
    const exported = JSON.parse(await page.run(`return document.querySelector("#tweaks-json-text").value;`));
    for (const theme of ["light", "dark"]) {
      if (Object.keys(exported[theme] || {}).length !== 6) fail(`export is missing ${theme} settings: ${JSON.stringify(exported)}`);
    }
    const changed = { light: { syntax: 40, underline: true, nonsense: 1 }, dark: { codeSize: 99 } };
    await importJson(page, JSON.stringify(changed));
    const note = await page.run(`return document.querySelector("#tweaks-json-note").textContent;`);
    if (!note.startsWith("Imported")) fail(`import said: ${note}`);
    if ((await tweakShown(page, "Syntax color")) !== "40%") fail("syntax color not imported");
    const underline = await page.run(`return document.documentElement.hasAttribute("data-mark-underline");`);
    if (!underline) fail("underline not imported");
    const after = JSON.parse(await page.run(`return document.querySelector("#tweaks-json-text").value;`));
    if (after.dark.codeSize !== 18) fail(`out-of-range size not kept in range: ${after.dark.codeSize}`);
    await importJson(page, "{ not json");
    const refused = await page.run(`return document.querySelector("#tweaks-json-note").textContent;`);
    if (!refused.startsWith("Not imported")) fail(`bad JSON was not refused: ${refused}`);
    if ((await tweakShown(page, "Syntax color")) !== "40%") fail("bad JSON changed something");
  },

  // ---- marking things reviewed

  "hunks alternate stripes, and each one's button wears its own": async (page, site) => {
    await page.open(site);
    const paths = await sidebarPaths(page);
    const [path, label] = paths[45];
    await page.click("#tree .file-row", label);
    await settledPlace(page, path);
    const stripes = await page.run(`
      const card = [...document.querySelector("#stream").children].find((el) => el.dataset.key === ${JSON.stringify(path)});
      return [...card.querySelectorAll(".hunk")].map((hunk) => [getComputedStyle(hunk).borderRightColor, getComputedStyle(hunk.querySelector("button.mark")).borderColor]);
    `);
    if (stripes.length < 3) fail(`${path} has too few hunks to show anything`);
    stripes.forEach(([stripe, button], i) => {
      if (button !== stripe) fail(`hunk ${i}'s button is ${button}, not its stripe ${stripe}`);
      if (stripe !== stripes[i % 2][0]) fail(`hunk ${i} is ${stripe}, not ${stripes[i % 2][0]}`);
    });
    if (stripes[0][0] === stripes[1][0]) fail(`neighbouring hunks share the stripe ${stripes[0][0]}`);
  },
  "marking a hunk brings the next one up in its place, and finishing the file the next file": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    const [path, label] = paths[45];
    await page.click("#tree .file-row", label);
    await settledPlace(page, path);
    let facts = await cardFacts(page, path);
    if (facts.hunks < 3) fail(`${path} has too few hunks to show anything`);
    while (facts.hunks > 1) {
      // The first hunk's button is at its foot; the reader scrolls to it.
      await clickInCard(page, path, "Mark above reviewed");
      const marked = await page.run(`
        const card = [...document.querySelector("#stream").children].find((el) => el.dataset.key === ${JSON.stringify(path)});
        const stream = document.querySelector("#stream");
        return { top: card.querySelector(".hunk").getBoundingClientRect().top, floor: stream.getBoundingClientRect().top + card.querySelector(".file-head").offsetHeight };
      `);
      const before = facts.hunks;
      await page.waitFor(`[...document.querySelector("#stream").children].find((el) => el.dataset.key === ${JSON.stringify(path)}).querySelectorAll(".hunk").length < ${before}`);
      await sleep(400);
      const next = await page.run(`return document.querySelector(".hunk.focused")?.getBoundingClientRect().top;`);
      const want = Math.max(marked.top, marked.floor);
      if (next == null || Math.abs(next - want) > 2) fail(`the next hunk is at ${next}, not ${want}, where the marked one was`);
      facts = await cardFacts(page, path);
    }
    await clickInCard(page, path, "Mark above reviewed");
    await gone(page, path);
    const place = await settledPlace(page, facts.next);
    if (!(place.gap >= 0 && place.gap <= 12)) fail(`${facts.next}, which followed, ended up at ${JSON.stringify(place)}`);
  },
  "... or from the middle of a long file, by its header": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    const [path, label] = paths[30];
    await page.click("#tree .file-row", label);
    await settledPlace(page, path);
    // Read a way into the file; its header stays in view, stuck to the top.
    await page.wheel("#stream", 700);
    await sleep(400);
    const { next } = await cardFacts(page, path);
    await clickInCard(page, path, "Mark file reviewed");
    await gone(page, path);
    const place = await settledPlace(page, next);
    if (!(place.gap >= 0 && place.gap <= 12)) fail(`${next}, which followed, ended up at ${JSON.stringify(place)}`);
  },
  "... with the browser's own scroll anchoring": async (page, site) => {
    await page.open(site, { latency: 150 });
    const paths = await sidebarPaths(page);
    const [path, label] = paths[60];
    await page.click("#tree .file-row", label);
    await settledPlace(page, path);
    await page.wheel("#stream", 300);
    await sleep(400);
    const { next } = await cardFacts(page, path);
    await clickInCard(page, path, "Mark file reviewed");
    await gone(page, path);
    const place = await settledPlace(page, next);
    if (!(place.gap >= 0 && place.gap <= 12)) fail(`${next}, which followed, ended up at ${JSON.stringify(place)}`);
  },
  "finishing a file lower on screen leaves the rest where it was": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    // A file short enough that the one after it is on screen too.
    let next = null;
    for (let i = 10; i < 60 && !next; i++) {
      await page.click("#tree .file-row", paths[i][1]);
      await settledPlace(page, paths[i][0]);
      const facts = await cardFacts(page, paths[i][0]);
      const onScreen = await page.run(`
        const stream = document.querySelector("#stream");
        const card = [...stream.children].find((el) => el.dataset.key === ${JSON.stringify(facts.next)});
        return card.getBoundingClientRect().top + 60 < stream.getBoundingClientRect().bottom;
      `);
      if (onScreen) next = facts.next;
    }
    if (!next) fail("no short file found");
    const reading = await page.run(READING);
    await clickInCard(page, next, "Mark file reviewed");
    await gone(page, next);
    await sleep(800);
    const later = await page.run(`const key = ${JSON.stringify(reading.key)}; ${WHERE}`);
    if (Math.abs(later - reading.top) > 1) fail(`${reading.key} moved from ${reading.top} to ${later}`);
  },
  "finishing the last file leaves the one before it in view": async (page, site) => {
    await page.open(site, { latency: 150, anchoring: false });
    const paths = await sidebarPaths(page);
    const [path, label] = paths[paths.length - 1];
    const [before] = paths[paths.length - 2];
    await page.click("#tree .file-row", label);
    await settledPlace(page, path);
    await clickInCard(page, path, "Mark file reviewed");
    await gone(page, path);
    const place = await settledPlace(page, before);
    // Its end, at least, is on screen.
    const seen = await page.run(`
      const stream = document.querySelector("#stream");
      const card = [...stream.children].find((el) => el.dataset.key === ${JSON.stringify(before)});
      const r = card.getBoundingClientRect(), s = stream.getBoundingClientRect();
      return r.bottom > s.top && r.top < s.bottom;
    `);
    if (!seen) fail(`${before} is off screen: ${JSON.stringify(place)}`);
  },
};

main().catch((err) => {
  console.error(err.message);
  process.exitCode = 1;
});
