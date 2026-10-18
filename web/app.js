// projj web client. No build step: this file is served as written.

import { grammarFor } from "/highlight.js";

const $ = (selector, parent = document) => parent.querySelector(selector);

/** Creates an element. `props` keys starting with "on" are listeners. */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value == null || value === false) continue;
    if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "class") el.className = value;
    else if (key === "dataset") Object.assign(el.dataset, value);
    else el.setAttribute(key, value === true ? "" : value);
  }
  el.append(...children.flat().filter((child) => child != null && child !== false));
  return el;
}

/** Browsers only let a closing page finish requests up to about this size. */
const KEEPALIVE_LIMIT = 60_000;

/**
 * In the desktop app there is no server to fetch from: the page hands each
 * request to the app through its webview's message bridge, and the app
 * answers through `window.__electrobun.receiveMessageFromHost`.
 */
const desktop = window.__electrobunHostBridge ? desktopBridge() : null;

function desktopBridge() {
  let next = 0;
  const waiting = new Map();
  const receive = (message) => {
    // The app is quitting: hand over unsaved edits now, as when a tab closes.
    if (message?.type === "projj-flush") return void flushDrafts();
    if (message?.type !== "projj" || !waiting.has(message.id)) return;
    waiting.get(message.id)(message);
    waiting.delete(message.id);
  };
  window.__electrobun.receiveMessageFromHost = receive;
  // Anything that arrived before this script ran was held for it.
  for (const message of window.__electrobunPendingHostMessages?.splice(0) || []) receive(message);
  return {
    /** Resolves with `{ status, body }`. Handing over is immediate, so a closing page loses nothing. */
    send(method, path, root, body) {
      return new Promise((resolve) => {
        const id = ++next;
        waiting.set(id, resolve);
        window.__electrobunHostBridge.postMessage(JSON.stringify({ type: "projj", id, method, path, root, body: body ?? "" }));
      });
    },
    /** Tells the app which root the window shows (`null` while choosing), for its title and for reopening. */
    showing(root, title) {
      window.__electrobunHostBridge.postMessage(JSON.stringify({ type: "projj-window", root, title }));
    },
  };
}

async function request(method, path, params, body) {
  // The desktop page's own address is not one the API lives at.
  const url = new URL(path, desktop ? "http://projj.invalid" : location.origin);
  for (const [key, value] of Object.entries(params || {})) {
    if (value != null && value !== "") url.searchParams.set(key, value);
  }
  const payload = body ? JSON.stringify(body) : undefined;
  // Every request says which root it is about: pages on different roots can
  // be open at once, so the server keeps no "current" one.
  const root = state.rootPath ? encodeURIComponent(state.rootPath) : null;
  let status, text;
  if (desktop) {
    ({ status, body: text } = await desktop.send(method, url.pathname + url.search, root, payload));
  } else {
    const headers = {};
    if (body) headers["content-type"] = "application/json";
    if (root) headers["x-projj-root"] = root;
    const response = await fetch(url, {
      method,
      headers,
      body: payload,
      // Lets a small write finish even if the page is closed under it.
      keepalive: method !== "GET" && (payload?.length || 0) < KEEPALIVE_LIMIT,
    });
    ({ status } = response);
    text = await response.text();
  }
  let data = null;
  try {
    data = JSON.parse(text);
  } catch {
    // Non-JSON bodies only come with error statuses.
  }
  if (status < 200 || status > 299) {
    const error = new Error((data && data.error) || text || `request failed (${status})`);
    error.status = status;
    throw error;
  }
  return data;
}

const api = (path, params) => request("GET", path, params);
const post = (path, body, params) => request("POST", path, params, body);

let toastTimer;
function toast(message, isError = false, lasting = isError) {
  const el = $("#toast");
  el.textContent = message;
  el.classList.toggle("error", isError);
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), lasting ? 9000 : 3500);
}

const fail = (err) => toast(err.message || String(err), true);

function relativeTime(ms) {
  const seconds = Math.round((Date.now() - ms) / 1000);
  const units = [["year", 31536000], ["month", 2592000], ["day", 86400], ["hour", 3600], ["minute", 60]];
  for (const [unit, size] of units) {
    if (Math.abs(seconds) >= size) {
      const count = Math.round(seconds / size);
      return `${count} ${unit}${count === 1 ? "" : "s"} ago`;
    }
  }
  return "just now";
}

const when = (time) =>
  h("span", { class: "when", title: new Date(time.ms).toLocaleString() }, relativeTime(time.ms));

function splitPath(path) {
  const cut = path.lastIndexOf("/") + 1;
  return [path.slice(0, cut), path.slice(cut)];
}

function pathLabel(path) {
  const [dir, name] = splitPath(path);
  return [dir && h("span", { class: "dir" }, dir), name];
}

function tally(added, removed) {
  if (added == null && removed == null) return h("span", { class: "tally" }, "binary");
  return h(
    "span",
    { class: "tally" },
    h("span", { class: "plus" }, `+${added}`),
    " ",
    h("span", { class: "minus" }, `−${removed}`),
  );
}

const state = {
  // The path of the root under review, from the page's address; null while choosing one.
  rootPath: null,
  root: null,
  view: "review",
  context: 3,
  layout: "unified",
  // Review
  side: "unstaged",
  status: null,
  statusText: "",
  sections: new Map(),
  focused: null,
  // Workspaces folded away, in the sidebar and in the diff stream alike.
  folded: new Set(),
  // History
  ws: null,
  revset: "",
  mode: "changes",
  filePath: null,
  log: null,
  selected: null,
  base: null,
  tab: "diff",
  allFiles: false,
};

// ---------------------------------------------------------------- diffs

const STATUS_LETTER = { added: "A", deleted: "D", modified: "M" };

/**
 * Splits a line into nodes where either the syntax class or the changed-word
 * marking differs. Both come as ranges of offsets into the text.
 */
function renderText(line) {
  const { text } = line;
  const tokens = line.syn || [];
  const marks = line.hl || [];
  if (!tokens.length && !marks.length) return [text];
  const cuts = new Set([0, text.length]);
  for (const [start, end] of [...tokens, ...marks]) {
    cuts.add(Math.min(start, text.length));
    cuts.add(Math.min(end, text.length));
  }
  const edges = [...cuts].sort((a, b) => a - b);
  const nodes = [];
  let token = 0;
  let mark = 0;
  for (let i = 0; i + 1 < edges.length; i++) {
    const [start, end] = [edges[i], edges[i + 1]];
    while (token < tokens.length && tokens[token][1] <= start) token++;
    while (mark < marks.length && marks[mark][1] <= start) mark++;
    const className = token < tokens.length && tokens[token][0] <= start ? `t-${tokens[token][2]}` : null;
    const piece = text.slice(start, end);
    const node = className ? h("span", { class: className }, piece) : piece;
    nodes.push(mark < marks.length && marks[mark][0] <= start ? h("mark", null, node) : node);
  }
  return nodes;
}

/**
 * Renders one diff line. In the unified layout it is a full row with both
 * line numbers. In the split layout it is one half of a row, on the `left`
 * (old) or `right` (new) side, with that side's number.
 */
function renderLine(line, index, side) {
  const kind = line.kind === "+" ? "add" : line.kind === "-" ? "del" : "ctx";
  const text = h("span", { class: "text" }, renderText(line));
  if (line.no_eol) text.append(h("span", { class: "eol" }, "  no newline at end of file"));
  const numbers = side
    ? [h("span", { class: "no" }, (side === "left" ? line.old : line.new) ?? "")]
    : [h("span", { class: "no" }, line.old ?? ""), h("span", { class: "no" }, line.new ?? "")];
  return h(
    "div",
    { class: `line ${kind} ${side || ""}`.trim(), dataset: { index } },
    numbers,
    h("span", { class: "sign" }, line.kind.trim()),
    text,
  );
}

/**
 * Lays a hunk out in two columns. Unchanged lines appear on both sides; each
 * run of removed lines sits beside the added lines that replaced it.
 */
function renderSplit(lines) {
  const rows = [];
  const half = (index, side) => (index == null ? h("div", { class: "filler" }) : renderLine(lines[index], index, side));
  const pair = (left, right) => rows.push(h("div", { class: "pair" }, half(left, "left"), half(right, "right")));
  let at = 0;
  while (at < lines.length) {
    if (lines[at].kind === " ") {
      pair(at, at);
      at++;
      continue;
    }
    const removed = [];
    const added = [];
    while (at < lines.length && lines[at].kind === "-") removed.push(at++);
    while (at < lines.length && lines[at].kind === "+") added.push(at++);
    for (let i = 0; i < Math.max(removed.length, added.length); i++) pair(removed[i], added[i]);
  }
  return rows;
}

/**
 * Renders the hunks of one file. With `review`, hunks get the controls that
 * move them between "to review" and "reviewed".
 */
function renderHunks(file, review, layout = state.layout) {
  if (file.note) return [h("p", { class: "file-note" }, file.note)];
  if (file.binary) return [h("p", { class: "file-note" }, "Binary file. It can only be marked as a whole.")];
  if (!file.hunks.length) {
    const what = file.status === "added" ? "Empty file." : "No content changes; only the file mode differs.";
    return [h("p", { class: "file-note" }, what)];
  }
  return file.hunks.map((hunk) => {
    // A file that is all new or all gone has nothing to put beside itself.
    const split = layout === "split" && file.status === "modified";
    const rows = split ? renderSplit(hunk.lines) : hunk.lines.map((line, index) => renderLine(line, index));
    const lines = h("div", { class: split ? "lines split" : "lines" }, rows);
    const range = `@@ -${hunk.old_start},${hunk.old_count} +${hunk.new_start},${hunk.new_count} @@`;
    const head = h("div", { class: "hunk-head" }, h("span", null, range), h("span", { class: "section" }, hunk.section));
    const el = h("div", { class: "hunk" }, head, lines);
    el.hunk = hunk;
    if (review) attachReviewControls(el, head, lines, review);
    return el;
  });
}

function attachReviewControls(el, head, lines, review) {
  const staged = review.side === "staged";
  const selected = new Set();
  // Where the last plain click landed, and whether it selected or deselected.
  let anchor = null;
  let anchorOn = true;
  const clear = h("button", { type: "button", class: "quiet", hidden: true }, "Clear selection");
  const act = h("button", { type: "button", class: staged ? "unmark" : "mark" });
  const relabel = () => {
    const count = selected.size;
    const what = count ? `${count} line${count === 1 ? "" : "s"}` : "hunk";
    act.textContent = staged ? `Send ${what} back` : count ? `Mark ${what} reviewed` : "Mark above reviewed";
    clear.hidden = count === 0;
  };
  const toggle = (index, on) => {
    if (el.hunk.lines[index].kind === " ") return;
    if (on) selected.add(index);
    else selected.delete(index);
    for (const row of lines.querySelectorAll(`.line[data-index="${index}"]`)) row.classList.toggle("selected", on);
  };
  lines.classList.add("selectable");
  lines.addEventListener("click", (event) => {
    const row = event.target.closest(".line");
    if (!row || !event.target.classList.contains("no") || row.classList.contains("ctx")) return;
    const index = Number(row.dataset.index);
    if (event.shiftKey && anchor != null) {
      // Extend what the last click did, be it selecting or deselecting.
      const [low, high] = [Math.min(anchor, index), Math.max(anchor, index)];
      for (let i = low; i <= high; i++) toggle(i, anchorOn);
      // Shift-click also selects text; that is not what was asked for.
      getSelection().removeAllRanges();
    } else {
      anchorOn = !selected.has(index);
      toggle(index, anchorOn);
      anchor = index;
    }
    relabel();
    focusHunk(el, false);
  });
  clear.addEventListener("click", () => {
    for (const index of [...selected]) toggle(index, false);
    anchor = null;
    relabel();
  });
  act.addEventListener("click", () => el.apply());
  el.apply = () => applyHunk(review.section, el, selected.size ? [...selected].sort((a, b) => a - b) : null);
  el.addEventListener("click", () => focusHunk(el, false));
  relabel();
  // At the foot, where the reader is once they have read the hunk.
  el.append(h("div", { class: "hunk-foot" }, clear, act));
}

function fileShell(file, pathText, actions) {
  const body = h("div", { class: "file-body" });
  const path = h("span", { class: "path", title: "Collapse or expand" }, pathLabel(pathText));
  const counts = h("span", { class: "counts" });
  const head = h(
    "div",
    { class: "file-head" },
    h("span", { class: `status ${file.status || ""}` }, STATUS_LETTER[file.status] || "?"),
    path,
    counts,
    h("span", { class: "actions" }, actions),
  );
  const el = h("article", { class: "file" }, head, body);
  path.addEventListener("click", () => el.classList.toggle("collapsed"));
  return { el, body, counts, head };
}

// --------------------------------------------------------------- review

const sideOf = (entry, side) => (side === "staged" ? entry.staged : entry.unstaged || (entry.directory ? {} : null));

function reviewEntries(side = state.side) {
  return (state.status?.entries || []).filter((entry) => sideOf(entry, side));
}

const shortRepo = (id) => id.replace(/\.git$/, "").split("/").slice(-2).join("/");

/** Where a repository is kept, when that is somewhere other than a listed workspace. */
const repoNote = (repo) => (repo.detached ? `Repository kept in ${repo.path}` : "");

function groupLabel(workspace) {
  if (!workspace) return { name: "Root files", ws: "", title: "Files outside any jj workspace" };
  const found = allWorkspaces().find((ws) => ws.path === workspace);
  if (!found) return { name: workspace, ws: "", title: workspace };
  const { repo } = found;
  // A workspace that is its own repository needs no second label.
  const own = repo.path === found.path;
  return {
    name: found.path === "." ? state.root.name : shortRepo(repo.id),
    ws: own ? "" : found.name,
    title: [found.path, repoNote(repo)].filter(Boolean).join("\n"),
  };
}

function grouped(entries) {
  const groups = new Map();
  for (const entry of entries) {
    const key = entry.workspace || "";
    if (!groups.has(key)) groups.set(key, { key, ...groupLabel(entry.workspace), entries: [] });
    groups.get(key).entries.push(entry);
  }
  return [...groups.values()];
}

const relativeTo = (path, workspace) => (workspace && workspace !== "." ? path.slice(workspace.length + 1) : path);

/** What to call an entry inside its group; a nested repository is the workspace itself. */
const displayPath = (entry) => relativeTo(entry.path, entry.workspace) || splitPath(entry.path)[1];

function totals(entries, side) {
  let added = 0;
  let removed = 0;
  for (const entry of entries) {
    added += sideOf(entry, side)?.added || 0;
    removed += sideOf(entry, side)?.removed || 0;
  }
  return { added, removed };
}

function renderSidebar() {
  for (const side of ["unstaged", "staged"]) {
    $(`#count-${side}`).textContent = reviewEntries(side).length;
    $(`.sides [data-side="${side}"]`).setAttribute("aria-selected", side === state.side);
  }
  $("#side-note").textContent =
    state.side === "staged"
      ? "Staged in git. Commit from a terminal when the review is done."
      : "Not yet staged in git.";
  const tree = $("#tree");
  tree.replaceChildren();
  for (const group of grouped(reviewEntries())) {
    const sum = totals(group.entries, state.side);
    const folded = state.folded.has(group.key);
    tree.append(
      h(
        "button",
        {
          type: "button",
          class: "group-row",
          title: group.title,
          "aria-expanded": String(!folded),
          onclick: () => toggleGroup(group.key).catch(fail),
        },
        h("span", { class: "caret" }, folded ? "▸" : "▾"),
        group.name,
        h("span", { class: "ws" }, group.ws),
        folded && h("span", { class: "ws" }, fileCount(group.entries.length)),
        tally(sum.added, sum.removed),
      ),
    );
    if (folded) continue;
    for (const entry of group.entries) {
      const stat = sideOf(entry, state.side);
      tree.append(
        h(
          "button",
          {
            type: "button",
            class: "file-row",
            title: entry.path,
            dataset: { path: entry.path },
            onclick: () => scrollToKey(entry.path),
          },
          h("span", { class: "name" }, h("bdi", null, pathLabel(displayPath(entry)))),
          entry.directory ? h("span", { class: "tally" }, "directory") : tally(stat.added, stat.removed),
        ),
      );
    }
  }
  markCurrentFile();
}

const fileCount = (count) => `${count} file${count === 1 ? "" : "s"}`;

/**
 * Folds a workspace away, or brings it back: its files in the sidebar and
 * its diffs in the stream. Whatever is being read stays where it is (see
 * keepPlace), unless it is one of the diffs being folded, in which case the
 * workspace's heading takes its place.
 */
async function toggleGroup(key) {
  const reading = firstInView();
  if (!state.folded.delete(key)) state.folded.add(key);
  renderSidebar();
  await syncStream();
  if (reading && !reading.isConnected) scrollToKey(`group:${key}`);
  markCurrentFile();
}

// ---- keeping the diff stream still
//
// Diffs load as they come near the screen, so the stream changes height above
// and below what is being read. Browsers that anchor scrolling do not all do
// it, nor in the same way, so the stream does its own: see keepPlace.

/** How far below the top of the stream a file asked for comes to rest. */
const PIN_GAP = 8;

/**
 * A file or heading the person asked to see. It is held at the top while the
 * diffs around it load and change the stream's height, until they scroll by
 * some other means; `top` is where the stream was last put to hold it.
 */
const pin = { key: null, top: 0 };

function firstInView() {
  const stream = $("#stream");
  const top = stream.getBoundingClientRect().top;
  return [...stream.children].find((el) => el.getBoundingClientRect().bottom > top + 1) || null;
}

function scrollStreamTo(top) {
  const stream = $("#stream");
  top = Math.max(0, Math.min(top, stream.scrollHeight - stream.clientHeight));
  // Instantly: a smooth scroll across diffs that load on the way would end
  // short of where it was aimed.
  if (Math.abs(top - stream.scrollTop) >= 0.5) stream.scrollTo({ top, behavior: "instant" });
  return stream.scrollTop;
}

/** Puts the pinned element back at the top of the stream. */
function holdPin() {
  const stream = $("#stream");
  const target = pin.key && [...stream.children].find((el) => el.dataset.key === pin.key);
  if (!target) {
    pin.key = null;
    return;
  }
  const offset = target.getBoundingClientRect().top - stream.getBoundingClientRect().top - PIN_GAP;
  pin.top = scrollStreamTo(stream.scrollTop + offset);
}

function releasePin() {
  pin.key = null;
}

/** The stream's elements by key: file paths, and `group:` for headings. */
const streamByKey = () => new Map([...$("#stream").children].map((el) => [el.dataset.key, el]));

/**
 * Runs `change`, which may resize, add or remove anything in the stream,
 * without moving what is being read: the pinned element if there is one,
 * otherwise the first element in view, keeps its place on screen. If the
 * change takes that element away (its file is all reviewed, say), whatever
 * followed it takes its place, at the top if the reader was partway into it.
 */
function keepPlace(change) {
  const stream = $("#stream");
  const children = [...stream.children];
  const anchor = (pin.key && children.find((el) => el.dataset.key === pin.key)) || firstInView();
  const before = anchor?.getBoundingClientRect().top;
  const following = anchor ? children.slice(children.indexOf(anchor) + 1).map((el) => el.dataset.key) : [];
  // A focused button that is about to go (Mark file reviewed, say) must not
  // take the focus with it: Chrome then lays out the half-changed stream and
  // clamps its scroll position, which throws the reader far up the list.
  if (stream.contains(document.activeElement) && document.activeElement !== stream) stream.focus({ preventScroll: true });
  change();
  if (!anchor) return;
  const now = streamByKey();
  if (pin.key && now.has(pin.key)) return holdPin();
  releasePin();
  const same = now.get(anchor.dataset.key);
  if (same) {
    scrollStreamTo(stream.scrollTop + same.getBoundingClientRect().top - before);
    return;
  }
  const next = following.map((key) => now.get(key)).find(Boolean);
  if (!next) return;
  const top = Math.max(before, stream.getBoundingClientRect().top + PIN_GAP);
  scrollStreamTo(stream.scrollTop + next.getBoundingClientRect().top - top);
}

function scrollToKey(key) {
  const target = [...$("#stream").children].find((el) => el.dataset.key === key);
  if (!target) return;
  target.classList.remove("collapsed");
  pin.key = key;
  holdPin();
}

const lazy = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      const section = state.sections.get(entry.target.dataset.key);
      if (section && !section.loaded && !section.loading) loadSection(section).catch(fail);
    }
  },
  { root: $("#stream"), rootMargin: "800px 0px" },
);

/** Whether the entry is a file that exists in the worktree, and so can be edited. */
const onDisk = (entry) => !entry.directory && entry.worktree !== "D" && !(entry.index === "D" && entry.worktree === ".");

function createSection(entry) {
  const staged = state.side === "staged";
  const section = { path: entry.path, entry, signature: null, loaded: false, loading: false };
  const actions = [];
  if (entry.workspace) {
    actions.push(
      h(
        "button",
        {
          type: "button",
          class: "quiet",
          title: "See the jj changes that touched this file",
          onclick: () => showFileHistory(entry.workspace, relativeTo(entry.path, entry.workspace)),
        },
        "History",
      ),
    );
  }
  if (onDisk(entry)) {
    actions.push(
      h(
        "button",
        { type: "button", class: "quiet", title: "Edit this file on disk", onclick: () => editSection(section) },
        "Edit",
      ),
    );
  }
  if (!entry.directory) {
    actions.push(
      h(
        "button",
        { type: "button", onclick: () => applyFiles(staged ? "unstage" : "stage", [entry.path]) },
        staged ? "Send file back" : "Mark file reviewed",
      ),
    );
  }
  const shell = fileShell({}, displayPath(entry), actions);
  Object.assign(section, shell);
  section.el.dataset.key = entry.path;
  section.body.classList.add("pending");
  lazy.observe(section.el);
  return section;
}

async function loadSection(section) {
  section.loading = true;
  const { entry } = section;
  try {
    let content;
    let file = null;
    if (entry.directory) {
      content = [h("p", { class: "file-note" }, "An untracked directory holding its own repository. Git lists it but does not look inside.")];
    } else {
      file = await api("/api/review/diff", { path: entry.path, side: state.side, context: state.context });
      content = file
        ? renderHunks(file, { side: state.side, section })
        : [h("p", { class: "file-note" }, "Nothing left here. Reload to refresh the list.")];
    }
    const status = $(".status", section.head);
    status.className = `status ${file?.status || ""}`;
    status.textContent = entry.untracked ? "?" : STATUS_LETTER[file?.status] || "M";
    const stat = sideOf(entry, state.side);
    keepPlace(() => {
      section.counts.replaceChildren(entry.directory ? "" : tally(stat.added, stat.removed));
      section.body.classList.remove("pending");
      section.body.replaceChildren(...content);
      markSection(section);
    });
    section.loaded = true;
  } finally {
    section.loading = false;
  }
}

/** Brings the diff stream in line with the latest status, reusing sections. */
async function syncStream() {
  const stream = $("#stream");
  stream.dataset.side = state.side;
  const entries = reviewEntries();
  const staged = state.side === "staged";
  if (!entries.length) {
    for (const section of state.sections.values()) lazy.unobserve(section.el);
    state.sections.clear();
    // A root just started has nothing anywhere yet: say how one fills up.
    const fresh = !state.status?.head && !state.status?.entries.length && !state.root?.repos.length;
    const message = fresh
      ? ["A new root", "Nothing is in it yet. jj repositories go under track/, one folder per upstream, such as track/github.com/tenant/repo.git/, with the repository in .repo/ and workspaces like sts-0/ beside it. Whatever changes in here shows up to review."]
      : staged
        ? ["Nothing reviewed yet", "Changes you mark as reviewed are staged in git and collect here until you commit them."]
        : ["Nothing left to review", "Every change is staged in git. Commit from a terminal to record the review."];
    stream.replaceChildren(
      h(
        "div",
        { class: "empty" },
        h("h2", null, message[0]),
        h("p", null, message[1]),
        fresh &&
          h("p", null, h("button", { type: "button", class: "mark", onclick: () => (showEditor(), startCreate("file", "")) }, "Add a file")),
      ),
    );
    return;
  }
  const children = [];
  const kept = new Map();
  const reloads = [];
  for (const group of grouped(entries)) {
    const paths = group.entries.filter((entry) => !entry.directory).map((entry) => entry.path);
    const folded = state.folded.has(group.key);
    const head = h(
      "h2",
      { class: "group-head", title: group.title, dataset: { key: `group:${group.key}` } },
      h(
        "button",
        { type: "button", class: "group-toggle", "aria-expanded": String(!folded), onclick: () => toggleGroup(group.key).catch(fail) },
        h("span", { class: "caret" }, folded ? "▸" : "▾"),
        group.name,
        h("span", { class: "ws" }, group.ws),
        folded && h("span", { class: "ws" }, `${fileCount(group.entries.length)} folded away`),
      ),
      paths.length > 1 &&
        h(
          "button",
          {
            type: "button",
            onclick: () => {
              const question = staged
                ? `Send all ${paths.length} files in ${group.name} back to review?`
                : `Mark all ${paths.length} files in ${group.name} as reviewed without reading them here?`;
              if (confirm(question)) applyFiles(staged ? "unstage" : "stage", paths);
            },
          },
          staged ? `Send all ${paths.length} files back` : `Mark all ${paths.length} files reviewed`,
        ),
    );
    children.push(head);
    if (folded) continue;
    for (const entry of group.entries) {
      const signature = JSON.stringify(entry);
      let section = state.sections.get(entry.path);
      if (!section) {
        section = createSection(entry);
      } else if (section.signature !== signature) {
        section.entry = entry;
        if (section.loaded) reloads.push(loadSection(section));
      }
      section.signature = signature;
      kept.set(entry.path, section);
      children.push(section.el);
    }
  }
  for (const [path, section] of state.sections) {
    if (!kept.has(path)) lazy.unobserve(section.el);
  }
  state.sections = kept;
  // Elements that stay are left in place rather than taken out and put back,
  // so that the stream is never even briefly emptied.
  keepPlace(() => {
    const wanted = new Set(children);
    for (const el of [...stream.children]) if (!wanted.has(el)) el.remove();
    children.forEach((el, i) => stream.children[i] !== el && stream.insertBefore(el, stream.children[i] || null));
  });
  await Promise.all(reloads);
}

async function loadStatus() {
  const status = await api("/api/review/status");
  const text = JSON.stringify(status);
  if (text === state.statusText) return;
  const known = new Set(allWorkspaces().map((ws) => ws.path));
  if (status.entries.some((entry) => entry.workspace && !known.has(entry.workspace))) await loadRoot();
  state.status = status;
  state.statusText = text;
  $("#root-head").textContent = `${status.branch || "detached"} @ ${status.head ? status.head.slice(0, 8) : "no commits"}`;
  renderSidebar();
  // The explorer shows which files changed, and open files may have too.
  if (editorOpen()) refreshFiles().catch(() => {});
  syncEditorWithDisk();
  await syncStream();
}

function switchSide(side) {
  if (side === state.side) return Promise.resolve();
  state.side = side;
  for (const section of state.sections.values()) lazy.unobserve(section.el);
  state.sections = new Map();
  state.focused = null;
  renderSidebar();
  releasePin();
  const synced = syncStream().catch(fail);
  $("#stream").scrollTop = 0;
  return synced;
}

function hunkElements() {
  return [...document.querySelectorAll("#stream .hunk")];
}

function focusHunk(el, scroll = true) {
  state.focused?.classList.remove("focused");
  state.focused = el;
  if (!el) return;
  el.classList.add("focused");
  el.closest(".file").classList.remove("collapsed");
  if (!scroll) return;
  releasePin();
  el.scrollIntoView({ block: "nearest" });
}

function moveFocus(step) {
  const hunks = hunkElements();
  if (!hunks.length) return;
  const at = hunks.indexOf(state.focused);
  const next = at < 0 ? (step > 0 ? firstVisible(hunks) : hunks.length - 1) : at + step;
  focusHunk(hunks[Math.max(0, Math.min(hunks.length - 1, next))]);
}

function firstVisible(elements) {
  const top = $("#stream").getBoundingClientRect().top;
  const index = elements.findIndex((el) => el.getBoundingClientRect().bottom > top + 40);
  return Math.max(0, index);
}

function moveFile(step) {
  const files = [...document.querySelectorAll("#stream .file")];
  if (!files.length) return;
  const top = $("#stream").getBoundingClientRect().top;
  const at = files.findIndex((el) => el.getBoundingClientRect().bottom > top + 44);
  scrollToKey(files[Math.max(0, Math.min(files.length - 1, at + step))].dataset.key);
}

/** Remembers where the focused hunk sits so focus survives a re-render. */
function focusAnchor(section, el) {
  const paths = [...state.sections.keys()];
  return {
    path: section.path,
    index: [...section.body.querySelectorAll(".hunk")].indexOf(el),
    nextPath: paths[paths.indexOf(section.path) + 1],
    top: el.getBoundingClientRect().top,
  };
}

function restoreFocus(anchor) {
  const hunksOf = (path) => [...(state.sections.get(path)?.body.querySelectorAll(".hunk") || [])];
  const hunks = hunksOf(anchor.path);
  // When the file is done, carry on with the one that followed it.
  const target = hunks.length ? hunks[Math.min(anchor.index, hunks.length - 1)] : hunksOf(anchor.nextPath)[0];
  focusHunk(target || null, false);
  // While the file has hunks left, the one that now comes next takes the
  // place of the one just marked, so reading carries on from there; but not
  // under the file's header, which sticks to the top. (A file with nothing
  // left makes way for the next one; see keepPlace.)
  if (!hunks.length || !target || anchor.top == null) return;
  const stream = $("#stream");
  const header = state.sections.get(anchor.path)?.head.offsetHeight || 0;
  const place = Math.max(anchor.top, stream.getBoundingClientRect().top + header);
  scrollStreamTo(stream.scrollTop + target.getBoundingClientRect().top - place);
}

async function refreshAfterChange(anchor) {
  try {
    await loadStatus();
    if (anchor) restoreFocus(anchor);
  } catch (err) {
    fail(err);
  }
}

async function applyHunk(section, el, lines) {
  if (el.busy) return;
  el.busy = true;
  const anchor = focusAnchor(section, el);
  try {
    await post("/api/review/apply", {
      action: state.side === "staged" ? "unstage" : "stage",
      path: section.path,
      hunk: el.hunk.id,
      lines,
      context: state.context,
    });
  } catch (err) {
    fail(err);
    // The file moved under us; show what is really there now.
    section.signature = null;
    state.statusText = "";
  } finally {
    el.busy = false;
  }
  await refreshAfterChange(anchor);
}

async function applyFiles(action, paths) {
  try {
    await post("/api/review/apply-files", { action, paths });
  } catch (err) {
    fail(err);
  }
  await refreshAfterChange(null);
}

function markCurrentFile() {
  if (state.view !== "review") return;
  const stream = $("#stream");
  const top = stream.getBoundingClientRect().top;
  const current = [...stream.querySelectorAll(".file")].find((el) => el.getBoundingClientRect().bottom > top + 44);
  for (const row of document.querySelectorAll("#tree .file-row")) {
    const on = current != null && row.dataset.path === current.dataset.key;
    if (on && !row.classList.contains("current")) row.scrollIntoView({ block: "nearest" });
    row.classList.toggle("current", on);
  }
}

// --------------------------------------------------------------- editing

/** The diff rows of one file card that a text selection touches, in order. */
function selectedRows() {
  const selection = getSelection();
  if (!selection.rangeCount || selection.isCollapsed) return null;
  const range = selection.getRangeAt(0);
  const start = range.startContainer.nodeType === Node.ELEMENT_NODE ? range.startContainer : range.startContainer.parentElement;
  const first = start?.closest("#stream .line") || [...document.querySelectorAll("#stream .line")].find((row) => range.intersectsNode(row));
  const card = first?.closest(".file");
  const section = card && state.sections.get(card.dataset.key);
  if (!section || !onDisk(section.entry)) return null;
  // Side by side, a selection runs down one column, but the range between its
  // ends also crosses the other column's rows.
  const side = ["left", "right"].find((name) => first.classList.contains(name));
  const rows = [...card.querySelectorAll(side ? `.line.${side}` : ".line")].filter((row) => range.intersectsNode(row));
  return rows.length ? { section, range, rows } : null;
}

/** Offset into a row's text of a range boundary, clamped to the row. */
function offsetInRow(row, container, offset) {
  const text = $(".text", row);
  const length = row.closest(".hunk").hunk.lines[row.dataset.index].text.length;
  const probe = document.createRange();
  probe.selectNodeContents(text);
  const where = probe.comparePoint(container, offset);
  if (where < 0) return 0;
  if (where > 0) return length;
  probe.setEnd(container, offset);
  return Math.min(probe.toString().length, length);
}

/**
 * Where a row sits in the file on disk, as far as the diff can tell. Removed
 * rows are not in the file, so they stand for the position they were cut
 * from. The editor refines this against the file's real content.
 */
function rowPosition(row, column, isEnd) {
  const lines = row.closest(".hunk").hunk.lines;
  const index = Number(row.dataset.index);
  const line = lines[index];
  if (line.new != null) return { line: line.new, column, text: line.text };
  for (let i = index + 1; i < lines.length; i++) {
    if (lines[i].new != null) return { line: lines[i].new, column: 0, text: lines[i].text };
  }
  for (let i = index - 1; i >= 0; i--) {
    if (lines[i].new != null) return { line: lines[i].new, column: isEnd ? lines[i].text.length : 0, text: lines[i].text };
  }
  return { line: 1, column: 0, text: null };
}

function positionPopover() {
  const popover = $("#selection-actions");
  const found = state.view === "review" ? selectedRows() : null;
  if (!found) {
    popover.hidden = true;
    return;
  }
  const rects = found.range.getClientRects();
  const rect = rects[rects.length - 1] || found.range.getBoundingClientRect();
  // Follow the selection while it is on screen, and step aside once it is not.
  const pane = $("#stream").getBoundingClientRect();
  if (rect.bottom < pane.top || rect.top > pane.bottom) {
    popover.hidden = true;
    return;
  }
  popover.hidden = false;
  const { width, height } = popover.getBoundingClientRect();
  const left = Math.max(8, Math.min(rect.right - width / 2, innerWidth - width - 8));
  const below = rect.bottom + 8;
  popover.style.left = `${left}px`;
  popover.style.top = `${below + height > innerHeight ? rect.top - height - 8 : below}px`;
}

function editSelection() {
  const found = selectedRows();
  if (!found) return;
  const { section, range, rows } = found;
  const [first, last] = [rows[0], rows[rows.length - 1]];
  const start = rowPosition(first, offsetInRow(first, range.startContainer, range.startOffset), false);
  const end = rowPosition(last, offsetInRow(last, range.endContainer, range.endOffset), true);
  $("#selection-actions").hidden = true;
  openEditor(section.path, start, end)
    // The editor takes room from the diff; keep what was selected in sight.
    .then(() => first.isConnected && first.scrollIntoView({ block: "nearest" }))
    .catch(fail);
}

/** Opens the editor at the first change shown for the file. */
function editSection(section) {
  const lines = [...section.body.querySelectorAll(".hunk")].flatMap((el) => el.hunk.lines);
  const line = lines.find((candidate) => candidate.kind === "+") || lines.find((candidate) => candidate.new != null);
  const at = line ? { line: line.new, column: 0, text: line.text } : { line: 1, column: 0, text: null };
  openEditor(section.path, at, at).catch(fail);
}

/**
 * Finds the line a position refers to in the file as it is now. Line numbers
 * from the diff are exact for unreviewed changes, but only a hint once the
 * index and the worktree differ or the file has been edited since.
 */
function locate(lines, position) {
  const wanted = Math.max(1, Math.min(position.line, lines.length));
  if (position.text == null || lines[wanted - 1] === position.text) return wanted;
  for (let distance = 1; distance < lines.length; distance++) {
    for (const candidate of [wanted - distance, wanted + distance]) {
      if (lines[candidate - 1] === position.text) return candidate;
    }
  }
  return wanted;
}

/**
 * The editor keeps one buffer per open file, each with its own text area so
 * that undo history, caret and scroll survive switching between them.
 */
const editor = {
  buffers: new Map(),
  active: null,
  // A one-off line for the header when there is no buffer to hang it on.
  message: "",
  // The explorer: every file git does not ignore, and what is unfolded.
  files: [],
  // Null until listed, so that a root with no files at all is listed too.
  filesText: null,
  tree: null,
  expanded: new Set(),
  filter: "",
  // The folder new files and folders go in: the one last clicked, or the
  // folder of the file last opened.
  here: "",
  // A name being typed for something new: `{ kind, dir, value }`.
  creating: null,
  // A file or folder being renamed: `{ path, kind, value }`.
  renaming: null,
  // Folders made here that git cannot list yet, having nothing in them.
  madeFolders: new Set(),
  // What waits for a second press of its delete button.
  deleting: null,
  // The dirty file whose close button is waiting for a second press.
  armed: null,
  checking: false,
  recheck: false,
};

const editorOpen = () => !$("#editor").hidden;
const activeBuffer = () => editor.buffers.get(editor.active) || null;
const isDirty = (buffer) => buffer.text.value !== buffer.original;
const dirtyBuffers = () => [...editor.buffers.values()].filter(isDirty);
const canSave = (buffer) => isDirty(buffer) && !buffer.pending && !buffer.missing;

/** When git last saw the file change, or null if it has no changes. */
const stampOf = (path) => state.status?.entries.find((entry) => entry.path === path)?.modified ?? null;

function detectIndent(content) {
  const leads = content.split("\n").map((line) => line.match(/^(\t| +)\S/)?.[1]).filter(Boolean);
  if (leads.some((lead) => lead === "\t")) return "\t";
  return leads.reduce((least, lead) => (lead.length < least.length ? lead : least), leads[0] || "  ");
}

function editorOffset(lines, position) {
  const line = locate(lines, position);
  let offset = 0;
  for (let i = 0; i < line - 1; i++) offset += lines[i].length + 1;
  return { line, offset: offset + Math.min(position.column, lines[line - 1].length) };
}

/** Opens a buffer on `file` as read from disk, optionally with edits on top. */
function createBuffer(file, content = file.content) {
  const text = h("textarea", {
    class: "editor-text",
    wrap: "off",
    spellcheck: "false",
    autocapitalize: "off",
    autocomplete: "off",
    "aria-label": `Content of ${file.path}`,
  });
  text.value = content;
  // A colored copy of the text sits behind the text area, which then shows
  // only its caret and selection. See paintColors.
  const colors = h("pre", null);
  const marks = h("div", { class: "editor-marks", "aria-hidden": "true" });
  const layer = h("div", { class: "editor-layer" }, marks, h("div", { class: "editor-colors", "aria-hidden": "true" }, colors), text);
  const buffer = {
    path: file.path,
    eol: file.eol,
    // The version on disk these edits are based on, and its content.
    version: file.version,
    original: file.content,
    text,
    layer,
    colors,
    // The guide's highlights. See paintEditorMarks.
    marks,
    indent: detectIndent(file.content),
    // How the edits relate to a newer file on disk, until a person has
    // looked at it and chosen. See checkAgainstDisk.
    pending: null,
    missing: false,
    note: "",
    noteIsError: false,
    stamp: stampOf(file.path),
    wasDirty: content !== file.content,
    // When the text was last typed in, for telling old drafts from new.
    edited: null,
    // Syntax colors. `lines` and `tokens` hold the text and tokens of the
    // first `read` lines, and `states[i]` what is still open (a comment, a
    // string) where line i starts. See paintColors.
    grammar: grammarFor(file.path),
    lines: [],
    tokens: [],
    states: [""],
    read: 0,
    painted: null,
  };
  text.addEventListener("input", () => {
    buffer.edited = Date.now();
    setNote(buffer, "");
    paintColors(buffer, true);
    refreshBuffer(buffer);
  });
  text.addEventListener("scroll", () => {
    if (buffer === activeBuffer()) $("#editor-gutter").scrollTop = text.scrollTop;
    paintColors(buffer);
    paintEditorMarks(buffer);
  });
  text.addEventListener("keydown", (event) => {
    const plain = !event.metaKey && !event.ctrlKey && !event.altKey;
    if (event.key === "Tab" && plain && !event.shiftKey) {
      event.preventDefault();
      // Goes through the browser's editing path so that undo still works.
      if (!document.execCommand("insertText", false, buffer.indent)) {
        text.setRangeText(buffer.indent, text.selectionStart, text.selectionEnd, "end");
        text.dispatchEvent(new Event("input"));
      }
    } else if (event.key === "Escape" && plain) {
      // Back to the diff; the editor and its edits stay.
      $("#stream").focus();
    }
  });
  $("#editor-texts").append(layer);
  editor.buffers.set(file.path, buffer);
  return buffer;
}

// ---- syntax colors in the editor

/**
 * Draws the colored copy of the lines in view, plus a margin, and lines it up
 * with the text area's scroll position.
 *
 * The grammar reads a line at a time, so only what changed is read again:
 * from the first line that differs from last time, down to the last line
 * drawn. Lines further down are read when they are scrolled to.
 */
function paintColors(buffer, redraw = false) {
  const { text, colors, layer, grammar } = buffer;
  layer.classList.toggle("colored", Boolean(grammar));
  if (!grammar || !text.clientHeight) return;
  const lineHeight = parseFloat(getComputedStyle(text).lineHeight);
  const top = Math.floor(text.scrollTop / lineHeight);
  const bottom = Math.ceil((text.scrollTop + text.clientHeight) / lineHeight);
  const painted = buffer.painted;
  if (redraw || !painted || top < painted.first || bottom > painted.last) {
    const lines = text.value.split("\n");
    const first = Math.max(0, top - 40);
    const last = Math.min(lines.length, bottom + 40);
    // Scrolling leaves the text alone, so what was read still stands.
    let read = Math.min(buffer.read, lines.length);
    if (redraw) {
      read = 0;
      while (read < buffer.read && read < lines.length && buffer.lines[read] === lines[read]) read++;
    }
    for (; read < last; read++) {
      buffer.lines[read] = lines[read];
      [buffer.tokens[read], buffer.states[read + 1]] = grammar(lines[read], buffer.states[read]);
    }
    buffer.read = read;
    const rows = [];
    for (let i = first; i < last; i++) rows.push(h("div", null, renderText({ text: lines[i], syn: buffer.tokens[i] })));
    colors.replaceChildren(...rows);
    buffer.painted = { first, last };
  }
  const offset = buffer.painted.first * lineHeight - text.scrollTop;
  colors.style.transform = `translate(${-text.scrollLeft}px, ${offset}px)`;
}

function setNote(buffer, note, isError = false) {
  buffer.note = note;
  buffer.noteIsError = isError;
  editor.message = "";
}

/** Says something in the header that is not about the file being shown. */
function sayInEditor(message) {
  editor.message = message;
  updateChrome();
  setTimeout(() => {
    if (editor.message !== message) return;
    editor.message = "";
    updateChrome();
  }, 6000);
}

/** Brings everything that shows a buffer's state in line with it. */
function refreshBuffer(buffer) {
  if (buffer === activeBuffer()) updateGutter();
  const dirty = isDirty(buffer);
  if (dirty !== buffer.wasDirty) {
    buffer.wasDirty = dirty;
    renderExplorer();
  }
  updateChrome();
  storeDrafts();
}

function updateGutter() {
  const buffer = activeBuffer();
  const gutter = $("#editor-gutter");
  const count = buffer ? buffer.text.value.split("\n").length : 0;
  if (Number(gutter.dataset.count) !== count) {
    gutter.textContent = Array.from({ length: count }, (_, i) => i + 1).join("\n");
    gutter.dataset.count = count;
  }
  gutter.scrollTop = buffer ? buffer.text.scrollTop : 0;
}

/** The header, the notice bar and the top bar's summary of unsaved work. */
function updateChrome() {
  const buffer = activeBuffer();
  const unsaved = dirtyBuffers();
  $("#editor-path").textContent = buffer ? buffer.path : "No file open";
  const status = $("#editor-state");
  const own = buffer ? buffer.note || (isDirty(buffer) ? "Unsaved changes" : "") : "";
  status.textContent = editor.message || own;
  status.classList.toggle("error", Boolean(!editor.message && buffer?.note && buffer.noteIsError));
  $("#editor-save").disabled = !buffer || !canSave(buffer);
  const savable = unsaved.filter(canSave).length;
  $("#editor-save-all").hidden = savable < 2;
  $("#editor-save-all").textContent = `Save all ${savable}`;
  $("#editor-drafts").hidden = !unsaved.length;
  $("#editor-drafts").textContent = `Unsaved edits (${unsaved.length})`;
  $("#editor-empty").hidden = Boolean(buffer);
  $("#editor-gutter").hidden = !buffer;

  const notice = $("#editor-notice");
  const say = (message, ...actions) => {
    notice.replaceChildren(h("span", null, message), ...actions);
    notice.hidden = false;
  };
  const action = (label, run) => h("button", { type: "button", onclick: run }, label);
  if (buffer?.missing) {
    say(
      "This file is no longer on disk. What you wrote is still here, so you can copy it out.",
      action("Close it", () => closeBuffer(buffer.path, true)),
    );
  } else if (buffer?.pending) {
    say(
      "This file changed on disk while you had unsaved edits. Saving waits until you have looked at both.",
      action("Review the changes", () => openMerge(buffer).catch(fail)),
    );
  } else {
    notice.hidden = true;
  }

  const toggle = $("#editor-toggle");
  toggle.textContent = unsaved.length ? `Editor (${unsaved.length} unsaved)` : "Editor";
  toggle.setAttribute("aria-pressed", editorOpen());
}

function activate(path) {
  editor.active = editor.buffers.has(path) ? path : null;
  editor.message = "";
  for (const buffer of editor.buffers.values()) {
    buffer.layer.classList.toggle("active", buffer.path === editor.active);
  }
  if (activeBuffer()) paintColors(activeBuffer(), true);
  if (editor.active) reveal(editor.active);
  updateGutter();
  updateChrome();
  renderExplorer();
}

function setEditorHeight(pixels) {
  const height = Math.round(Math.max(160, Math.min(pixels, innerHeight - 180)));
  $("#editor").style.height = `${height}px`;
  if (height !== setting("editor-height")) saveSetting("editor-height", height);
}

/**
 * Gives `panel` a handle on its right edge (or its left, with `fromLeft`)
 * that drags, or moves with the arrow keys, to change its width, kept as the
 * setting `key`. Double-click puts it back to the stock width. style.css
 * reads the width from the CSS variable `--{key}` and keeps room beside the
 * panel however it is set.
 */
function widthHandle(panel, key, label, around = (change) => change(), fromLeft = false) {
  const outward = fromLeft ? -1 : 1;
  const root = document.documentElement;
  const set = (pixels) =>
    around(() => {
      if (pixels == null) root.style.removeProperty(`--${key}`);
      else root.style.setProperty(`--${key}`, `${Math.round(pixels)}px`);
    });
  // The width the panel ended up with, within what the CSS allows.
  const save = () => {
    const width = Math.round(panel.getBoundingClientRect().width);
    set(width);
    if (width !== setting(key)) saveSetting(key, width);
  };
  const handle = h("div", { class: fromLeft ? "width-handle left" : "width-handle", role: "separator", "aria-orientation": "vertical", "aria-label": label, tabindex: "0", title: "Drag to resize; double-click for the usual width" });
  handle.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    handle.setPointerCapture(event.pointerId);
    const from = { x: event.clientX, width: panel.getBoundingClientRect().width };
    const drag = (move) => set(from.width + outward * (move.clientX - from.x));
    handle.addEventListener("pointermove", drag);
    handle.addEventListener(
      "lostpointercapture",
      () => {
        handle.removeEventListener("pointermove", drag);
        save();
      },
      { once: true },
    );
  });
  handle.addEventListener("keydown", (event) => {
    const step = { ArrowLeft: -16, ArrowRight: 16 }[event.key];
    if (!step) return;
    event.preventDefault();
    set(panel.getBoundingClientRect().width + outward * step);
    save();
  });
  handle.addEventListener("dblclick", () => {
    set(null);
    saveSetting(key, null);
  });
  panel.append(handle);
  const width = Number(setting(key));
  if (width > 0) set(width);
}

function showEditor() {
  const drawer = $("#editor");
  if (!drawer.hidden) return;
  drawer.hidden = false;
  setEditorHeight(Number(setting("editor-height")) || innerHeight * 0.45);
  refreshFiles().catch(fail);
  updateChrome();
}

/** Hides the drawer. Open files and unsaved edits stay as they are. */
function hideEditor() {
  $("#editor").hidden = true;
  updateChrome();
}

/**
 * Shows a file in the editor, selecting from `start` to `end` if given. A
 * file that is already open keeps its edits and simply comes to the front.
 */
async function openEditor(path, start, end) {
  showEditor();
  if (!editor.buffers.has(path)) {
    let file;
    try {
      file = await api("/api/file", { path });
    } catch (err) {
      // Not a failure worth an alert: some files are just not for editing here.
      sayInEditor(`Not opened. ${err.message}`);
      return;
    }
    if (!editor.buffers.has(path)) createBuffer(file);
  }
  activate(path);
  const { text } = editor.buffers.get(path);
  // Focusing must not move a file away from where it was left.
  const { scrollTop, scrollLeft } = text;
  text.focus({ preventScroll: true });
  text.scrollTop = scrollTop;
  text.scrollLeft = scrollLeft;
  if (!start) return;
  const lines = text.value.split("\n");
  const from = editorOffset(lines, start);
  const to = editorOffset(lines, end || start);
  text.setSelectionRange(Math.min(from.offset, to.offset), Math.max(from.offset, to.offset));
  // Browsers do not scroll a text area to a selection made from script.
  const lineHeight = parseFloat(getComputedStyle(text).lineHeight);
  text.scrollTop = Math.max(0, (from.line - 1) * lineHeight - text.clientHeight / 3);
  text.scrollLeft = 0;
}

/**
 * Closes a file. One with unsaved edits needs the button pressed twice, the
 * second time on a label that says what will be lost.
 */
function closeBuffer(path, sure = false) {
  const buffer = editor.buffers.get(path);
  if (!buffer) return;
  if (!sure && isDirty(buffer) && editor.armed !== path) {
    editor.armed = path;
    renderExplorer();
    setTimeout(() => {
      if (editor.armed !== path) return;
      editor.armed = null;
      renderExplorer();
    }, 5000);
    return;
  }
  editor.armed = null;
  buffer.layer.remove();
  editor.buffers.delete(path);
  activate(editor.active === path ? [...editor.buffers.keys()].pop() : editor.active);
  storeDrafts();
}

/** Replaces a buffer's text, keeping the caret on the line it was on. */
function replaceText(buffer, content) {
  const { text } = buffer;
  if (text.value === content) return;
  const { scrollTop, scrollLeft, selectionStart } = text;
  const before = text.value.slice(0, selectionStart).split("\n");
  const caret = { line: before.length, column: before[before.length - 1].length };
  caret.text = text.value.split("\n")[caret.line - 1];
  text.value = content;
  const { offset } = editorOffset(content.split("\n"), caret);
  text.setSelectionRange(offset, offset);
  text.scrollTop = scrollTop;
  text.scrollLeft = scrollLeft;
  paintColors(buffer, true);
}

/** Makes `file`, as read from disk, the version a buffer's edits build on. */
function rebaseOnto(buffer, file) {
  Object.assign(buffer, { version: file.version, eol: file.eol, original: file.content, pending: null, missing: false });
}

/** Asks the server how a buffer's edits relate to the file as it is now. */
const compareWithDisk = (buffer) => post("/api/file/rebase", { path: buffer.path, base: buffer.original, content: buffer.text.value });

/** Records what `compareWithDisk` found. Returns whether a person must look. */
function setPending(buffer, result) {
  buffer.missing = false;
  if (result.status === "unchanged") {
    rebaseOnto(buffer, result.file);
    return false;
  }
  // Nothing is combined behind anyone's back: the edits wait as they are.
  buffer.pending = { ...result, seen: buffer.pending?.seen || false };
  return true;
}

/**
 * Looks at a buffer with unsaved edits after its file may have changed on
 * disk. If it did, the buffer is held back from saving and the three-way
 * merge is put in front of the person: at once when they are in that file,
 * or as a notice on it otherwise.
 */
async function checkAgainstDisk(buffer, show = false) {
  let result;
  const shown = buffer.pending?.file.version;
  try {
    result = await compareWithDisk(buffer);
  } catch (err) {
    if (err.status === 404) buffer.missing = true;
    else setNote(buffer, err.message, true);
    return;
  }
  if (!setPending(buffer, result)) {
    if (merge.path === buffer.path) closeMerge();
    return;
  }
  const dialog = $("#merge");
  if (dialog.open && merge.path === buffer.path) {
    // Only a file that really moved again is worth redrawing for.
    if (result.file.version !== shown) renderMerge(buffer, buffer.pending, true);
  } else if (!dialog.open && !$("#drafts").open && (show || (buffer === activeBuffer() && editorOpen() && !buffer.pending.seen))) {
    openMerge(buffer).catch(fail);
  }
}

/** Shows what is on disk in a buffer that has no edits of its own. */
async function followDisk(buffer) {
  let file;
  try {
    file = await api("/api/file", { path: buffer.path });
  } catch (err) {
    // Gone, or no longer text; nothing here is worth keeping open.
    if (err.status === 404 || err.status === 400) closeBuffer(buffer.path, true);
    return;
  }
  if (file.version === buffer.version) return;
  rebaseOnto(buffer, file);
  replaceText(buffer, file.content);
  setNote(buffer, "Updated from disk");
}

/**
 * Ends a pending merge one of three ways: with both sets of changes, with
 * only the edits made here, or with only the file on disk.
 */
function resolvePending(buffer, choice) {
  const { file, content } = buffer.pending;
  rebaseOnto(buffer, file);
  if (choice === "merged") {
    replaceText(buffer, content);
    setNote(buffer, isDirty(buffer) ? "Combined with the change on disk. Save to write it." : "Now the same as the file on disk");
  } else if (choice === "mine") {
    setNote(buffer, "Keeping only your version. Save to write it over the one on disk.");
  } else {
    replaceText(buffer, file.content);
    setNote(buffer, "Now showing the version on disk");
  }
  refreshBuffer(buffer);
  renderExplorer();
}

/**
 * Called when the worktree may have changed. Only buffers whose file git saw
 * change are looked at again.
 */
async function syncEditorWithDisk() {
  if (editor.checking) {
    editor.recheck = true;
    return;
  }
  editor.checking = true;
  try {
    for (const buffer of [...editor.buffers.values()]) {
      const stamp = stampOf(buffer.path);
      if (stamp === buffer.stamp && !buffer.unchecked) continue;
      buffer.stamp = stamp;
      buffer.unchecked = false;
      if (isDirty(buffer) || buffer.pending) await checkAgainstDisk(buffer);
      else await followDisk(buffer);
      if (editor.buffers.has(buffer.path)) refreshBuffer(buffer);
    }
  } finally {
    editor.checking = false;
  }
  renderExplorer();
  if (editor.recheck) {
    editor.recheck = false;
    syncEditorWithDisk();
  }
}

/** Writes one buffer to disk. Returns whether the file now holds its text. */
async function saveBuffer(buffer) {
  if (!canSave(buffer)) return false;
  const content = buffer.text.value;
  try {
    const saved = await post("/api/file", { path: buffer.path, content, eol: buffer.eol, base: buffer.version });
    buffer.version = saved.version;
    buffer.original = content;
    setNote(buffer, "Saved");
  } catch (err) {
    if (err.status === 409) {
      // The file moved since this buffer last looked. Show what happened
      // rather than deciding for the person.
      await checkAgainstDisk(buffer, true);
      refreshBuffer(buffer);
      renderExplorer();
      return false;
    }
    if (err.status === 404) buffer.missing = true;
    else setNote(buffer, err.message, true);
    refreshBuffer(buffer);
    return false;
  }
  refreshBuffer(buffer);
  // Reload the file's diff even if its line counts came out the same.
  const section = state.sections.get(buffer.path);
  if (section) section.signature = null;
  state.statusText = "";
  return true;
}

async function saveBuffers(buffers) {
  for (const buffer of buffers) await saveBuffer(buffer);
  await loadStatus().catch(fail);
}

// ---- unsaved edits outlive the page

/**
 * Unsaved edits are kept by the server, in a store shared by every projj
 * server on the device and filed by root. They come back whichever port or
 * browser the root is next opened in.
 */
const drafts = {
  // What the store holds for this root as far as this page knows:
  // path -> { version, content }.
  stored: new Map(),
  // Where the store is, for telling the person.
  file: "",
  timer: null,
  running: false,
  again: false,
  // Why the last attempt to store edits failed, if it did.
  problem: "",
};

/** The requests that would bring the store in line with the editor. */
function draftChanges() {
  const changes = [];
  const unsaved = new Map(dirtyBuffers().map((buffer) => [buffer.path, buffer]));
  for (const [path, buffer] of unsaved) {
    const stored = drafts.stored.get(path);
    const content = buffer.text.value;
    if (stored?.version === buffer.version && stored.content === content) continue;
    const body = { path, eol: buffer.eol, version: buffer.version, content, edited: buffer.edited };
    // The content the edits started from is sent once, not with every pause.
    if (stored?.version !== buffer.version) body.original = buffer.original;
    changes.push({ path, body });
  }
  for (const path of drafts.stored.keys()) {
    if (!unsaved.has(path)) changes.push({ path, body: null });
  }
  return changes;
}

async function sendDraft({ path, body }) {
  if (!body) {
    await request("DELETE", "/api/drafts", { path });
    drafts.stored.delete(path);
    return;
  }
  try {
    await post("/api/drafts", body);
  } catch (err) {
    if (err.status !== 409) throw err;
    // The store no longer has what these edits started from (they were
    // discarded from elsewhere, say), so the next round sends it again.
    drafts.stored.delete(path);
    drafts.again = true;
    return;
  }
  drafts.stored.set(path, { version: body.version, content: body.content });
}

function storeDrafts() {
  clearTimeout(drafts.timer);
  drafts.timer = setTimeout(syncDrafts, 300);
}

/** Brings the store in line with the editor, one round at a time. */
async function syncDrafts() {
  clearTimeout(drafts.timer);
  drafts.timer = null;
  if (drafts.running) {
    drafts.again = true;
    return;
  }
  drafts.running = true;
  let problem = "";
  try {
    for (const change of draftChanges()) await sendDraft(change);
  } catch (err) {
    problem = err.message || String(err);
  }
  drafts.running = false;
  if (problem && !drafts.problem) {
    toast(`Unsaved edits are not being kept: ${problem}. They will be lost if this page closes before you save.`, true);
  }
  drafts.problem = problem;
  if (problem) {
    // The store may only be busy; try again without being asked.
    drafts.again = false;
    drafts.timer = setTimeout(syncDrafts, 5000);
  } else if (drafts.again) {
    drafts.again = false;
    await syncDrafts();
  }
}

/**
 * Called as the page closes: sends whatever the store lacks, all at once.
 * Returns whether those requests are small enough to outlive the page, which
 * a browser only promises up to a limit shared between them.
 */
function flushDrafts() {
  clearTimeout(drafts.timer);
  drafts.timer = null;
  const changes = draftChanges();
  let size = 0;
  for (const change of changes) {
    size += change.body ? JSON.stringify(change.body).length : 0;
    // One may already be on its way; sending it twice does no harm.
    sendDraft(change).catch(() => {});
  }
  return !drafts.problem && (desktop || size < KEEPALIVE_LIMIT / 2);
}

async function restoreDrafts() {
  let kept = [];
  try {
    const answer = await api("/api/drafts");
    drafts.file = answer.store;
    kept = answer.drafts;
  } catch (err) {
    drafts.problem = err.message || String(err);
    toast(`Unsaved edits from earlier could not be read: ${drafts.problem}`, true);
  }
  for (const draft of kept) {
    if (editor.buffers.has(draft.path)) continue;
    drafts.stored.set(draft.path, { version: draft.version, content: draft.content });
    const file = { path: draft.path, eol: draft.eol, version: draft.version, content: draft.original };
    // The file may have moved on while the page was closed.
    const buffer = createBuffer(file, draft.content);
    buffer.unchecked = true;
    buffer.edited = draft.edited || null;
  }
  updateChrome();
  if (kept.length) toast(`Brought back unsaved edits to ${kept.length} file${kept.length === 1 ? "" : "s"}. See Editor.`, false, true);
}

// ---- reviewing unsaved edits against the file on disk

/** The file whose edits the merge dialog is showing, and what to do after. */
const merge = { path: null, backToList: false };

function closeMerge() {
  const dialog = $("#merge");
  if (dialog.open) dialog.close();
  const { backToList } = merge;
  Object.assign(merge, { path: null, backToList: false });
  updateChrome();
  renderExplorer();
  if (backToList && dirtyBuffers().length) openDrafts();
}

/**
 * Shows a buffer's unsaved edits beside what happened on disk. `result` is
 * what `compareWithDisk` returned; every diff in it is against the content
 * the edits started from, which is what makes the three comparable.
 */
function renderMerge(buffer, result, refreshed = false) {
  const [dir, name] = splitPath(buffer.path);
  const pane = (title, diff, hint) =>
    h(
      "section",
      { class: "merge-pane" },
      h("h3", null, title, " ", tally(diff.added, diff.removed)),
      h("p", { class: "merge-hint" }, hint),
      h("div", { class: "merge-diff" }, diff.hunks.length ? renderHunks(diff, null, "unified") : h("p", { class: "file-note" }, "No difference.")),
    );
  const button = (label, run, className) => h("button", { type: "button", class: className, onclick: run }, label);
  const settle = (choice) => () => {
    resolvePending(buffer, choice);
    closeMerge();
  };
  const later = button("Decide later", closeMerge);
  let lede;
  let panes;
  let actions;
  if (result.status === "merged") {
    lede = "This file changed on disk while you had unsaved edits. The two do not touch the same lines, so they can be combined. Nothing changes until you choose.";
    panes = [
      pane("Your unsaved edits", result.mine, "What you changed, here in the editor."),
      pane("Combined", result.merged, "Both sets of changes together. Accepting puts this in the editor, still unsaved."),
      pane("Changed on disk", result.theirs, "What something else wrote to the file meanwhile."),
    ];
    actions = [
      button("Keep only mine", settle("mine")),
      button("Use the one on disk", settle("disk")),
      later,
      button("Accept the combined version", () => acceptMerge(buffer).catch(fail), "mark"),
    ];
  } else if (result.status === "conflict") {
    lede = "This file changed on disk while you had unsaved edits, and both touch the same lines, so they cannot be combined for you. Nothing changes until you choose.";
    panes = [
      pane("Your unsaved edits", result.mine, "What you changed, here in the editor."),
      pane("Changed on disk", result.theirs, "What something else wrote to the file meanwhile."),
    ];
    actions = [button("Use the one on disk", settle("disk")), later, button("Keep mine", settle("mine"), "mark")];
  } else {
    lede = "Edits to this file that you have not saved. The file on disk is as it was when you started.";
    panes = [pane("Your unsaved edits", result.mine, "What saving would change.")];
    const discard = button("Discard these edits", () => {
      // Throwing work away takes a second press.
      if (discard.classList.contains("armed")) {
        closeBuffer(buffer.path, true);
        closeMerge();
      } else {
        discard.classList.add("armed");
        discard.textContent = "Press again to discard";
      }
    });
    actions = [
      discard,
      button("Close", closeMerge),
      button("Open in the editor", () => {
        merge.backToList = false;
        closeMerge();
        openEditor(buffer.path).catch(fail);
      }, "mark"),
    ];
  }
  $("#merge-title").replaceChildren(name, h("span", { class: "dir" }, ` ${dir}`));
  $("#merge-lede").textContent = lede;
  $("#merge-refreshed").hidden = !refreshed;
  const panesEl = $("#merge-panes");
  panesEl.dataset.count = panes.length;
  panesEl.replaceChildren(...panes);
  $("#merge-actions").replaceChildren(...actions);
  // Enter or Space on arrival must never pick a version by accident.
  requestAnimationFrame(() => (later.isConnected ? later : $("#merge-actions button:nth-child(2)"))?.focus());
}

/** Opens the merge dialog on a buffer, comparing afresh with the disk. */
async function openMerge(buffer, backToList = false) {
  let result;
  try {
    result = await compareWithDisk(buffer);
  } catch (err) {
    if (err.status === 404) buffer.missing = true;
    else setNote(buffer, err.message, true);
    refreshBuffer(buffer);
    // What is left to do with a file that is gone is in the editor itself.
    if (buffer.missing) openEditor(buffer.path).catch(fail);
    return;
  }
  setPending(buffer, result);
  if (buffer.pending) buffer.pending.seen = true;
  Object.assign(merge, { path: buffer.path, backToList });
  renderMerge(buffer, buffer.pending || result);
  if (!$("#merge").open) $("#merge").showModal();
  updateChrome();
}

/**
 * Takes the combined version, provided the file is still the one the dialog
 * shows. If it changed again in the meantime, the dialog is brought up to
 * date instead, and the person decides again with the new picture.
 */
async function acceptMerge(buffer) {
  const shown = buffer.pending;
  const fresh = await compareWithDisk(buffer);
  if (fresh.file.version !== shown.file.version) {
    if (setPending(buffer, fresh)) {
      buffer.pending.seen = true;
      renderMerge(buffer, buffer.pending, true);
    } else {
      closeMerge();
    }
    return;
  }
  resolvePending(buffer, "merged");
  closeMerge();
}

// ---- everything that is unsaved, in one place

async function openDrafts() {
  const dialog = $("#drafts");
  const list = $("#drafts-list");
  const render = (rows) => {
    list.replaceChildren(...rows);
    $("#drafts-empty").hidden = rows.length > 0;
    $("#drafts-discard-all").hidden = rows.length < 2;
  };
  const arm = (button, run) => () => {
    if (button.classList.contains("armed")) return run();
    button.classList.add("armed");
    button.textContent = "Press again to discard";
  };
  const row = (buffer, result) => {
    const [dir, name] = splitPath(buffer.path);
    const discard = h("button", { type: "button" }, "Discard");
    discard.addEventListener("click", arm(discard, () => {
      closeBuffer(buffer.path, true);
      openDrafts();
    }));
    const disk = buffer.missing
      ? "The file is no longer on disk."
      : result && result.status !== "unchanged"
        ? "The file changed on disk since."
        : "";
    return h(
      "li",
      null,
      h(
        "div",
        { class: "draft-what" },
        h("span", { class: "draft-name" }, name, h("span", { class: "dir" }, ` ${dir}`)),
        h(
          "span",
          { class: "draft-meta" },
          result ? tally(result.mine.added, result.mine.removed) : "",
          buffer.edited ? ` last edited ${relativeTime(buffer.edited)}` : " edited before this page was opened",
          disk && h("span", { class: "draft-disk" }, ` ${disk}`),
        ),
      ),
      h("button", { type: "button", onclick: () => { dialog.close(); openMerge(buffer, true).catch(fail); } }, "Review"),
      discard,
    );
  };
  const buffers = dirtyBuffers();
  render(buffers.map((buffer) => row(buffer, null)));
  const all = $("#drafts-discard-all");
  all.classList.remove("armed");
  all.textContent = "Discard all";
  all.onclick = arm(all, () => {
    for (const buffer of dirtyBuffers()) closeBuffer(buffer.path, true);
    openDrafts();
  });
  const where = $("#drafts-where");
  where.hidden = !drafts.file && !drafts.problem;
  where.classList.toggle("error", Boolean(drafts.problem));
  where.replaceChildren(
    ...(drafts.problem
      ? [`They are not being kept right now: ${drafts.problem}`]
      : ["Kept in ", h("code", null, drafts.file), ". ", h("code", null, "projj drafts"), " in a terminal lists them for every root."]),
  );
  if (!dialog.open) dialog.showModal();
  // Fill in what each draft changes and whether its file has moved on.
  const results = await Promise.all(
    buffers.map(async (buffer) => {
      try {
        const result = await compareWithDisk(buffer);
        setPending(buffer, result);
        return result;
      } catch (err) {
        if (err.status === 404) buffer.missing = true;
        return null;
      }
    }),
  );
  if (dialog.open) render(buffers.filter((buffer) => editor.buffers.has(buffer.path)).map((buffer, i) => row(buffer, results[i])));
  updateChrome();
  renderExplorer();
}

// ---- the explorer

async function refreshFiles() {
  const { files } = await api("/api/files");
  const text = [...files, "", ...editor.madeFolders].join("\n");
  if (text === editor.filesText) return;
  editor.files = files;
  editor.filesText = text;
  const root = { path: "", dirs: new Map(), files: [] };
  const folder = (parts) => {
    let node = root;
    parts.forEach((part, depth) => {
      if (!node.dirs.has(part)) {
        node.dirs.set(part, { name: part, path: parts.slice(0, depth + 1).join("/"), dirs: new Map(), files: [] });
      }
      node = node.dirs.get(part);
    });
    return node;
  };
  for (const listed of files) {
    // Git names a nested repository as a directory and does not look inside.
    const nested = listed.endsWith("/");
    const parts = (nested ? listed.slice(0, -1) : listed).split("/");
    const name = parts.pop();
    folder(parts).files.push({ name, path: listed, nested });
  }
  // Empty folders made here are shown too, though git has nothing in them to list.
  for (const made of editor.madeFolders) folder(made.split("/"));
  editor.tree = root;
  if (!editor.expanded.size) {
    // Start with the workspaces showing rather than a single folded line.
    for (const workspace of allWorkspaces()) reveal(`${workspace.path}/x`);
  }
  renderExplorer();
}

/** Unfolds every directory above a file. */
function reveal(path) {
  const parts = path.split("/");
  for (let depth = 1; depth < parts.length; depth++) editor.expanded.add(parts.slice(0, depth).join("/"));
}

/** Every directory above each of the given paths. */
function ancestors(paths) {
  const dirs = new Set();
  for (const path of paths) {
    const parts = path.split("/");
    for (let depth = 1; depth < parts.length; depth++) dirs.add(parts.slice(0, depth).join("/"));
  }
  return dirs;
}

function renderExplorer() {
  if (!editorOpen()) return;
  const changes = new Map(
    (state.status?.entries || []).map((entry) => [entry.path, entry.untracked ? "?" : entry.worktree !== "." ? entry.worktree : entry.index]),
  );
  const unsaved = new Set(dirtyBuffers().map((buffer) => buffer.path));
  const marks = { changes, unsaved, changedDirs: ancestors(changes.keys()), unsavedDirs: ancestors(unsaved) };

  const fileRow = (file, depth, withDir) => {
    const [dir, name] = splitPath(file.path);
    const row = h(
      "button",
      {
        type: "button",
        class: `tree-row leaf ${file.path === editor.active ? "active" : ""}`,
        // Past the caret column, so names line up with their folder's.
        style: `padding-left: ${26 + depth * 14}px`,
        title: file.nested ? "A repository of its own; git does not list what is inside" : file.path,
        disabled: file.nested,
        onclick: () => {
          editor.here = dir.replace(/\/$/, "");
          openEditor(file.path).catch(fail);
        },
      },
      h("span", { class: "name" }, file.nested ? `${file.name}/` : withDir ? [name, h("span", { class: "dir" }, ` ${dir}`)] : file.name),
      unsaved.has(file.path) && h("span", { class: "unsaved", title: "Unsaved edits" }),
      changes.has(file.path) && h("span", { class: "change", title: "Has changes to review" }, changes.get(file.path)),
    );
    if (editor.renaming?.path === file.path) return renameRow(26 + Math.max(depth, 0) * 14);
    return treeItem(
      row,
      file.nested ? [] : [renameButton(file.path, name, "file"), deleteButton(file.path, name, changes.get(file.path) === "?")],
    );
  };

  const rows = [];
  const walk = (node, depth) => {
    for (const first of [...node.dirs.values()].sort((a, b) => a.name.localeCompare(b.name))) {
      // A chain of directories holding nothing else reads as one row.
      let dir = first;
      let label = first.name;
      // Not past a folder something new is being named in.
      while (!dir.files.length && dir.dirs.size === 1 && dir.path !== editor.creating?.dir) {
        dir = dir.dirs.values().next().value;
        label += `/${dir.name}`;
      }
      const open = editor.expanded.has(dir.path);
      const row = h(
        "button",
        {
          type: "button",
          class: `tree-row folder ${dir.path === editor.here ? "here" : ""}`,
          style: `padding-left: ${10 + depth * 14}px`,
          "aria-expanded": String(open),
          title: dir.path,
          onclick: () => {
            if (open) editor.expanded.delete(dir.path);
            else editor.expanded.add(dir.path);
            editor.here = dir.path;
            renderExplorer();
          },
        },
        h("span", { class: "caret" }, open ? "▾" : "▸"),
        h("span", { class: "name" }, h("bdi", null, label)),
        marks.unsavedDirs.has(dir.path) && !open && h("span", { class: "unsaved", title: "Unsaved edits inside" }),
        marks.changedDirs.has(dir.path) && !open && h("span", { class: "change", title: "Changes to review inside" }, "•"),
      );
      rows.push(
        editor.renaming?.path === dir.path
          ? renameRow(10 + depth * 14)
          : treeItem(row, [
              actionButton("file", `New file in ${dir.path}`, () => startCreate("file", dir.path)),
              actionButton("folder", `New folder in ${dir.path}`, () => startCreate("folder", dir.path)),
              renameButton(dir.path, label, "folder"),
              deleteButton(dir.path, label, false, true),
            ]),
      );
      if (open) walk(dir, depth + 1);
    }
    if (editor.creating?.dir === node.path) rows.push(nameRow(depth));
    for (const file of [...node.files].sort((a, b) => a.name.localeCompare(b.name))) rows.push(fileRow(file, depth, false));
  };

  const terms = editor.filter.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length) {
    const matches = editor.files
      .filter((path) => !path.endsWith("/") && terms.every((term) => path.toLowerCase().includes(term)))
      // Prefer a match in the file's own name, then the shorter path.
      .map((path) => [terms.every((term) => splitPath(path)[1].toLowerCase().includes(term)) ? 0 : 1, path])
      .sort((a, b) => a[0] - b[0] || a[1].length - b[1].length)
      .slice(0, 80);
    rows.push(...matches.map(([, path]) => fileRow({ path }, -1, true)));
    if (!matches.length) rows.push(h("p", { class: "tree-note" }, "No file matches."));
  } else if (editor.tree) {
    walk(editor.tree, 0);
  }

  const open = [...editor.buffers.values()].map((buffer) => {
    const [dir, name] = splitPath(buffer.path);
    const armed = editor.armed === buffer.path;
    return h(
      "div",
      { class: `open-row ${buffer.path === editor.active ? "active" : ""}` },
      h(
        "button",
        { type: "button", class: "tree-row leaf", title: buffer.path, onclick: () => openEditor(buffer.path).catch(fail) },
        h("span", { class: "name" }, name, h("span", { class: "dir" }, ` ${dir.split("/").slice(-3).join("/")}`)),
        unsaved.has(buffer.path) && h("span", { class: "unsaved", title: "Unsaved edits" }),
        (buffer.pending || buffer.missing) && h("span", { class: "change", title: "Needs a decision" }, "!"),
      ),
      h(
        "button",
        {
          type: "button",
          class: armed ? "close armed" : "close",
          title: armed ? "Press again to throw these edits away" : "Close this file",
          "aria-label": `Close ${name}`,
          onclick: () => closeBuffer(buffer.path),
        },
        armed ? "Discard edits" : "×",
      ),
    );
  });

  const list = $("#explorer-open");
  list.hidden = !open.length;
  list.replaceChildren(h("h3", null, "Open"), ...open);
  const tree = $("#explorer-tree");
  const { scrollTop } = tree;
  const typing = document.activeElement?.classList.contains("tree-input") && document.activeElement;
  const [selStart, selEnd] = typing ? [typing.selectionStart, typing.selectionEnd] : [];
  tree.replaceChildren(...rows);
  tree.scrollTop = scrollTop;
  // Rows are redrawn as files change; whoever was typing a name carries on.
  const input = $("#explorer-tree .tree-input");
  if (!input) return;
  if (editor.renaming?.focus) {
    // The name is what is usually changed: select it, without its extension.
    const { value } = input;
    const start = value.lastIndexOf("/") + 1;
    const dot = value.lastIndexOf(".");
    const end = editor.renaming.kind === "file" && dot > start ? dot : value.length;
    input.focus();
    input.setSelectionRange(start, end);
    editor.renaming.focus = false;
  } else if (editor.creating?.focus) {
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
    editor.creating.focus = false;
  } else if (typing) {
    input.focus();
    input.setSelectionRange(selStart, selEnd);
  }
}

/** The outlines of the explorer's small buttons, on a 16-unit grid. */
const ICONS = {
  file: ["M4 1.5h5l3.5 3.5v9.5h-8.5z M9 1.5v3.5h3.5", "M8 8v5 M5.5 10.5h5"],
  folder: ["M1.5 3.5h4.5l1.5 1.5h7v8.5h-13z", "M8 7v5 M5.5 9.5h5"],
  trash: ["M2.5 4h11 M6 4v-2h4v2 M4 4l0.8 10h6.4l0.8-10", "M6.5 6.5v5 M9.5 6.5v5"],
  rename: ["M10.5 2.5l3 3-8 8h-3v-3z", "M9 4l3 3"],
};

function icon(name) {
  const el = svg("svg", { viewBox: "0 0 16 16", "aria-hidden": "true" });
  for (const d of ICONS[name]) el.append(svg("path", { d }));
  return el;
}

/** A row of the tree with buttons that show when it is pointed at or focused. */
function treeItem(row, actions) {
  return h("div", { class: "tree-item" }, row, actions.length && h("span", { class: "tree-actions" }, actions));
}

function actionButton(name, label, onclick) {
  return h("button", { type: "button", class: "icon tree-act", title: label, "aria-label": label, onclick }, icon(name));
}

function renameButton(path, name, kind) {
  return actionButton("rename", `Rename or move ${name} (F2)`, () => startRename(path, kind));
}

/** Deletes on the second press, so that one slip of the mouse loses nothing. */
function deleteButton(path, name, untracked, folder = false) {
  const armed = editor.deleting === path;
  const warning = folder ? "everything in it goes too; whatever git has not seen cannot be brought back" : untracked ? "git has not seen it, so it cannot be brought back" : "git can bring it back";
  return h(
    "button",
    {
      type: "button",
      class: `icon tree-act ${armed ? "armed" : ""}`,
      title: armed ? `Press again to delete ${path}: ${warning}` : `Delete ${path}`,
      "aria-label": armed ? `Press again to delete ${name}` : `Delete ${name}`,
      onclick: () => deleteEntry(path),
    },
    armed ? "Delete?" : icon("trash"),
  );
}

/** The input for naming something new, in the folder it will go in. */
function nameRow(depth) {
  const creating = editor.creating;
  const label = creating.kind === "file" ? "New file name" : "New folder name";
  const input = h("input", {
    type: "text",
    class: "tree-input",
    spellcheck: "false",
    autocomplete: "off",
    "aria-label": creating.dir ? `${label}, in ${creating.dir}` : label,
    placeholder: creating.kind === "file" ? "name.ext, or folder/name.ext" : "folder, or folder/inner",
    oninput: (event) => (creating.value = event.target.value),
    onkeydown: (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        createEntry().catch(fail);
      } else if (event.key === "Escape") {
        event.stopPropagation();
        cancelCreate();
      }
    },
    // Leaving an empty name means changing one's mind; a typed one is kept.
    onblur: () => setTimeout(() => editor.creating === creating && !creating.value.trim() && cancelCreate(), 150),
  });
  input.value = creating.value;
  const indent = (creating.kind === "file" ? 26 : 10) + depth * 14;
  return h("div", { class: `tree-item naming kind-${creating.kind}`, style: `padding-left: ${indent - 18}px` }, icon(creating.kind), input);
}

/** The input for a new name, or a new place, in the row of what is renamed. */
function renameRow(indent) {
  const renaming = editor.renaming;
  const input = h("input", {
    type: "text",
    class: "tree-input",
    spellcheck: "false",
    autocomplete: "off",
    "aria-label": `New name or place for ${renaming.path}`,
    oninput: (event) => (renaming.value = event.target.value),
    onkeydown: (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        renameEntry().catch(fail);
      } else if (event.key === "Escape") {
        event.stopPropagation();
        cancelRename();
      }
    },
    // Leaving the name as it was means changing one's mind; a changed one is kept.
    onblur: () => setTimeout(() => editor.renaming === renaming && renaming.value.trim() === renaming.path && cancelRename(), 150),
  });
  input.value = renaming.value;
  return h("div", { class: "tree-item naming", style: `padding-left: ${indent - 18}px` }, icon("rename"), input);
}

const within = (path, folder) => path === folder || path.startsWith(`${folder}/`);

/** Starts renaming, unless open files with unsaved edits would have to move. */
function startRename(path, kind) {
  const unsaved = dirtyBuffers().filter((buffer) => within(buffer.path, path));
  if (unsaved.length) {
    return toast(`Save or discard the unsaved edits to ${unsaved.map((buffer) => buffer.path).join(", ")} first.`, true);
  }
  editor.creating = null;
  editor.renaming = { path, kind, value: path, focus: true };
  renderExplorer();
}

function cancelRename() {
  editor.renaming = null;
  renderExplorer();
}

/** Renames or moves; open files, unfolded folders and the folder new things go in follow. */
async function renameEntry() {
  const { path: from, value } = editor.renaming;
  const to = value.trim().replace(/^\/+|\/+$/g, "");
  if (!to || to === from) return cancelRename();
  const { kind } = await post("/api/file/rename", { from, to });
  editor.renaming = null;
  const moved = (path) => (within(path, from) ? to + path.slice(from.length) : path);
  const reopen = [];
  for (const open of [...editor.buffers.keys()]) {
    if (!within(open, from)) continue;
    // The file in front comes back in front.
    if (open === editor.active) reopen.push(moved(open));
    else reopen.unshift(moved(open));
    closeBuffer(open, true);
  }
  editor.madeFolders = new Set([...editor.madeFolders].map(moved));
  editor.expanded = new Set([...editor.expanded].map(moved));
  if (editor.here) editor.here = moved(editor.here);
  reveal(kind === "folder" ? `${to}/x` : to);
  editor.filesText = null;
  await refreshFiles();
  for (const path of reopen) await openEditor(path);
  toast(`Renamed ${from} to ${to}.`);
  loadStatus().catch(() => {});
}

function startCreate(kind, dir = editor.here) {
  if (editor.filter) {
    editor.filter = "";
    $("#explorer-filter").value = "";
  }
  if (dir) {
    reveal(`${dir}/x`);
    editor.expanded.add(dir);
  }
  editor.renaming = null;
  editor.creating = { kind, dir, value: "", focus: true };
  renderExplorer();
}

function cancelCreate() {
  editor.creating = null;
  renderExplorer();
}

/** Makes what was named: a file opens in the editor, a folder becomes where the next things go. */
async function createEntry() {
  const { kind, dir, value } = editor.creating;
  const name = value.trim().replace(/^\/+|\/+$/g, "");
  if (!name) return cancelCreate();
  const path = dir ? `${dir}/${name}` : name;
  await post("/api/file/create", { path, kind });
  editor.creating = null;
  if (kind === "folder") {
    editor.madeFolders.add(path);
    reveal(`${path}/x`);
    editor.here = path;
  } else {
    reveal(path);
    editor.here = splitPath(path)[0].replace(/\/$/, "");
  }
  editor.filesText = null;
  await refreshFiles();
  if (kind === "file") await openEditor(path);
  loadStatus().catch(() => {});
}

async function deleteEntry(path) {
  if (editor.deleting !== path) {
    editor.deleting = path;
    renderExplorer();
    setTimeout(() => {
      if (editor.deleting !== path) return;
      editor.deleting = null;
      renderExplorer();
    }, 5000);
    return;
  }
  editor.deleting = null;
  try {
    await request("DELETE", "/api/file", { path });
  } catch (err) {
    renderExplorer();
    return fail(err);
  }
  const under = (each) => each === path || each.startsWith(`${path}/`);
  for (const open of [...editor.buffers.keys()]) if (under(open)) closeBuffer(open, true);
  for (const made of [...editor.madeFolders]) if (under(made)) editor.madeFolders.delete(made);
  if (under(editor.here)) editor.here = splitPath(path)[0].replace(/\/$/, "");
  toast(`Deleted ${path}.`);
  editor.filesText = null;
  await refreshFiles();
  loadStatus().catch(() => {});
}

function bindEditor() {
  const drawer = $("#editor");
  const popover = $("#selection-actions");
  // Pressing the popover must not drop the selection it acts on.
  popover.addEventListener("mousedown", (event) => event.preventDefault());
  $("#selection-edit").addEventListener("click", editSelection);
  document.addEventListener("mouseup", () => setTimeout(positionPopover));
  document.addEventListener("keyup", () => !getSelection().isCollapsed && positionPopover());
  document.addEventListener("selectionchange", () => {
    if (getSelection().isCollapsed) popover.hidden = true;
  });
  // Side by side, keep a text selection within the column it started in.
  document.addEventListener("mousedown", (event) => {
    const half = event.target.closest?.(".split .line");
    if (half) document.body.dataset.selectSide = half.classList.contains("left") ? "left" : "right";
  });
  let moving = false;
  const follow = () => {
    if (moving || getSelection().isCollapsed) return;
    moving = true;
    requestAnimationFrame(() => {
      moving = false;
      positionPopover();
    });
  };
  $("#stream").addEventListener("scroll", follow, { passive: true });
  addEventListener("resize", follow);

  drawer.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "s") {
      event.preventDefault();
      const buffer = activeBuffer();
      if (buffer) saveBuffers([buffer]);
    }
  });
  $("#editor-toggle").addEventListener("click", () => (editorOpen() ? hideEditor() : showEditor()));
  $("#editor-close").addEventListener("click", hideEditor);
  $("#editor-save").addEventListener("click", () => saveBuffers([activeBuffer()]));
  $("#editor-save-all").addEventListener("click", () => saveBuffers(dirtyBuffers()));
  $("#editor-drafts").addEventListener("click", () => openDrafts().catch(fail));
  $("#drafts-close").addEventListener("click", () => $("#drafts").close());
  // Escape means "decide later", like the button.
  $("#merge").addEventListener("cancel", (event) => {
    event.preventDefault();
    closeMerge();
  });
  // The colored copy is sized by the text area, which the drawer resizes.
  new ResizeObserver(() => activeBuffer() && paintColors(activeBuffer(), true)).observe($("#editor-texts"));

  const explorer = $("#explorer");
  const showFiles = (shown) => {
    explorer.hidden = !shown;
    $("#explorer-toggle").setAttribute("aria-pressed", shown);
    if (shown !== setting("explorer", true)) saveSetting("explorer", shown);
  };
  showFiles(setting("explorer", true));
  widthHandle(explorer, "explorer-width", "Resize the file list");
  $("#explorer-toggle").addEventListener("click", () => showFiles(explorer.hidden));
  $("#explorer-new-file").addEventListener("click", () => startCreate("file"));
  // F2 renames the focused row, as in most file managers.
  $("#explorer-tree").addEventListener("keydown", (event) => {
    const row = event.target.closest?.(".tree-row");
    if (event.key !== "F2" || !row || row.disabled) return;
    event.preventDefault();
    startRename(row.title, row.classList.contains("folder") ? "folder" : "file");
  });
  $("#explorer-new-folder").addEventListener("click", () => startCreate("folder"));
  const filter = $("#explorer-filter");
  filter.addEventListener("input", () => {
    editor.filter = filter.value;
    renderExplorer();
  });
  filter.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      // The best match is the first row.
      $("#explorer-tree .tree-row.leaf:not(:disabled)")?.click();
    } else if (event.key === "Escape" && filter.value) {
      event.stopPropagation();
      filter.value = "";
      editor.filter = "";
      renderExplorer();
    }
  });

  // Drag the top edge of the drawer to share the screen differently.
  const handle = $("#editor-resize");
  handle.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    handle.setPointerCapture(event.pointerId);
    const from = { y: event.clientY, height: drawer.offsetHeight };
    const drag = (move) => setEditorHeight(from.height + from.y - move.clientY);
    handle.addEventListener("pointermove", drag);
    handle.addEventListener("pointerup", () => handle.removeEventListener("pointermove", drag), { once: true });
  });
  handle.addEventListener("keydown", (event) => {
    const step = { ArrowUp: 32, ArrowDown: -32 }[event.key];
    if (!step) return;
    event.preventDefault();
    setEditorHeight(drawer.offsetHeight + step);
  });
  // Unsaved edits are kept by the server, so leaving needs no warning unless
  // the latest of them cannot be handed over in time.
  addEventListener("beforeunload", (event) => {
    if (!flushDrafts() && dirtyBuffers().length) event.preventDefault();
  });
  document.addEventListener("visibilitychange", () => document.hidden && drafts.timer && syncDrafts());
}

// -------------------------------------------------------------- history

const LANE = 14;

/**
 * Lays the commit graph out row by row. Each lane remembers the commit it is
 * waiting for; a row draws the lanes passing through it, the lanes arriving
 * at its node, and the edges leaving the node toward its parents.
 */
function layoutGraph(nodes) {
  let lanes = [];
  const rows = [];
  let width = 1;
  for (const node of nodes) {
    const id = node.commit.commit_id;
    const before = lanes.map((lane) => (lane ? { ...lane } : null));
    let column = lanes.findIndex((lane) => lane?.id === id);
    if (column < 0) {
      column = lanes.indexOf(null);
      if (column < 0) column = lanes.length;
    }
    lanes = lanes.map((lane) => (lane?.id === id ? null : lane));
    lanes[column] = null;
    const out = [];
    let stub = false;
    for (const edge of node.edges) {
      if (edge.kind === "missing") {
        stub = true;
        continue;
      }
      let target = lanes.findIndex((lane) => lane?.id === edge.target);
      if (target < 0) {
        target = lanes[column] == null ? column : lanes.indexOf(null);
        if (target < 0) target = lanes.length;
        lanes[target] = { id: edge.target, kind: edge.kind };
      }
      out.push({ column: target, kind: edge.kind });
    }
    while (lanes.length && lanes[lanes.length - 1] == null) lanes.pop();
    rows.push({ column, before, after: lanes.map((lane) => (lane ? { ...lane } : null)), out, stub, id });
    width = Math.max(width, before.length, lanes.length, column + 1);
  }
  return { rows, width };
}

function svg(tag, attrs) {
  const el = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [key, value] of Object.entries(attrs)) el.setAttribute(key, value);
  return el;
}

function renderGraphRow(row, width, commit) {
  const height = 30;
  const mid = height / 2;
  const x = (column) => column * LANE + LANE / 2;
  const el = svg("svg", { width: width * LANE, height, "aria-hidden": "true" });
  const link = (x1, y1, x2, y2, kind) => {
    const d = x1 === x2 ? `M${x1} ${y1}V${y2}` : `M${x1} ${y1}C${x1} ${(y1 + y2) / 2} ${x2} ${(y1 + y2) / 2} ${x2} ${y2}`;
    el.append(svg("path", { d, class: `graph-line ${kind === "indirect" ? "indirect" : ""}` }));
  };
  row.before.forEach((lane, column) => {
    if (!lane) return;
    if (lane.id === row.id) link(x(column), 0, x(row.column), mid, lane.kind);
    else link(x(column), 0, x(column), mid, lane.kind);
  });
  row.after.forEach((lane, column) => {
    if (!lane) return;
    const passing = row.before[column]?.id === lane.id && lane.id !== row.id;
    if (passing) link(x(column), mid, x(column), height, lane.kind);
  });
  for (const edge of row.out) link(x(row.column), mid, x(edge.column), height, edge.kind);
  if (row.stub) link(x(row.column), mid, x(row.column), mid + 9, "indirect");
  const kind = commit.conflict ? "conflict" : commit.current ? "current" : commit.immutable ? "immutable" : "";
  const cx = x(row.column);
  if (commit.immutable && !commit.current) {
    el.append(svg("path", { d: `M${cx} ${mid - 5}L${cx + 5} ${mid}L${cx} ${mid + 5}L${cx - 5} ${mid}Z`, class: `graph-node ${kind}` }));
  } else {
    el.append(svg("circle", { cx, cy: mid, r: commit.current ? 5 : 4, class: `graph-node ${kind}` }));
  }
  return el;
}

function idLabel(id, prefix, length = 8) {
  const shown = Math.max(length, Math.min(prefix, 12));
  return [h("b", null, id.slice(0, prefix)), id.slice(prefix, shown)];
}

function commitTags(commit) {
  return [
    ...commit.working_copies.map((name) => h("span", { class: "tag wc", title: `Working copy of ${name}` }, `${name}@`)),
    ...commit.bookmarks.map((name) => h("span", { class: "tag bookmark" }, name)),
    ...commit.remote_bookmarks.map((name) => h("span", { class: "tag" }, name)),
    ...commit.tags.map((name) => h("span", { class: "tag" }, name)),
    commit.conflict && h("span", { class: "tag warn" }, "conflict"),
    commit.hidden && h("span", { class: "tag" }, "hidden"),
    commit.empty && !commit.root && h("span", { class: "tag" }, "empty"),
  ];
}

const firstLine = (description) => description.split("\n")[0];

function describe(commit) {
  if (commit.root) return h("span", { class: "desc none" }, "root");
  const title = firstLine(commit.description);
  return h("span", { class: `desc ${title ? "" : "none"}` }, commitTags(commit), title || "no description");
}

function allWorkspaces() {
  return (state.root?.repos || []).flatMap((repo) => repo.workspaces.map((ws) => ({ ...ws, repo })));
}

function renderWorkspacePicker() {
  const select = $("#workspace");
  select.replaceChildren(
    ...(state.root?.repos || []).map((repo) =>
      h(
        "optgroup",
        { label: repo.id.replace(/\.git$/, "") },
        repo.workspaces.map((ws) =>
          h("option", { value: ws.path, selected: ws.path === state.ws }, ws.problem ? `${ws.name} (cannot be opened)` : ws.name),
        ),
      ),
    ),
  );
  const current = allWorkspaces().find((ws) => ws.path === state.ws);
  const note = $("#repo-note");
  note.textContent = current ? current.problem || repoNote(current.repo) : "";
  note.title = note.textContent;
  note.hidden = !note.textContent;
}

async function loadRoot() {
  state.root = await api("/api/root");
  $("#root-name").textContent = state.root.name;
  $("#root-name").title = state.root.label;
  document.title = `${state.root.name} \u2013 projj`;
  desktop?.showing(state.root.path, document.title);
  renderMenu();
  if (!allWorkspaces().some((ws) => ws.path === state.ws)) state.ws = allWorkspaces()[0]?.path || null;
  renderWorkspacePicker();
}

async function loadHistory() {
  writeHash();
  const log = $("#log");
  const detail = $("#detail");
  for (const tab of document.querySelectorAll(".seg [data-mode]")) {
    tab.setAttribute("aria-selected", tab.dataset.mode === state.mode);
  }
  if (!state.ws) {
    log.replaceChildren(h("div", { class: "empty" }, h("h2", null, "No jj workspaces"), h("p", null, "This root has no workspace under track/ yet.")));
    detail.replaceChildren();
    return;
  }
  log.classList.toggle("wide", state.mode === "operations");
  detail.hidden = state.mode === "operations";
  try {
    if (state.mode === "operations") {
      const operations = await api("/api/jj/operations", { ws: state.ws, limit: 300 });
      renderOperations(operations);
      return;
    }
    const info = await api("/api/jj/workspace", { ws: state.ws });
    $("#revset").placeholder = info.default_revset;
    state.log = state.filePath
      ? await api("/api/jj/file-log", { ws: state.ws, path: state.filePath })
      : await api("/api/jj/log", { ws: state.ws, revset: state.revset });
    const ids = state.log.nodes.map((node) => node.commit.commit_id);
    if (!ids.includes(state.selected)) {
      const current = state.log.nodes.find((node) => node.commit.current && !node.commit.empty);
      state.selected = (current || state.log.nodes.find((node) => !node.commit.root) || state.log.nodes[0])?.commit.commit_id || null;
    }
    writeHash();
    renderLog();
    await renderDetail();
  } catch (err) {
    log.replaceChildren(h("div", { class: "empty" }, h("h2", null, "That did not work"), h("p", null, err.message)));
    detail.replaceChildren();
  }
}

function renderLog() {
  const log = $("#log");
  const { rows, width } = layoutGraph(state.log.nodes);
  const children = [];
  if (state.filePath) {
    children.push(
      h(
        "div",
        { class: "log-note" },
        h("span", null, "Changes that touched ", h("code", null, state.filePath)),
        h("button", { type: "button", onclick: () => { state.filePath = null; state.allFiles = false; loadHistory(); } }, "Show all changes"),
      ),
    );
  }
  if (!state.log.nodes.length) {
    children.push(h("div", { class: "empty" }, h("h2", null, "No changes match"), h("p", null, "Try a wider revset, such as all().")));
  }
  state.log.nodes.forEach((node, index) => {
    const commit = node.commit;
    const classes = ["commit", commit.commit_id === state.selected && "selected", commit.commit_id === state.base?.commit_id && "base", commit.hidden && "hidden-commit"];
    children.push(
      h(
        "button",
        {
          type: "button",
          class: classes.filter(Boolean).join(" "),
          dataset: { id: commit.commit_id },
          title: "Click to open. Shift-click to compare from this change.",
          onclick: (event) => (event.shiftKey ? setBase(commit) : selectCommit(commit.commit_id)),
        },
        renderGraphRow(rows[index], width, commit),
        h("span", { class: "ids" }, idLabel(commit.change_id, commit.change_prefix)),
        describe(commit),
        !commit.root && when(commit.committer.time),
      ),
    );
  });
  if (state.log.has_more) {
    children.push(h("div", { class: "log-note" }, `Showing the first ${state.log.nodes.length} changes. Narrow the revset to reach the rest.`));
  }
  log.replaceChildren(...children);
}

function selectCommit(id) {
  state.selected = id;
  state.tab = "diff";
  for (const row of document.querySelectorAll("#log .commit")) {
    row.classList.toggle("selected", row.dataset.id === id);
  }
  writeHash();
  renderDetail().catch(fail);
}

function setBase(commit) {
  state.base = state.base?.commit_id === commit.commit_id ? null : commit;
  for (const row of document.querySelectorAll("#log .commit")) {
    row.classList.toggle("base", row.dataset.id === state.base?.commit_id);
  }
  renderDetail().catch(fail);
}

function moveCommit(step) {
  const ids = (state.log?.nodes || []).map((node) => node.commit.commit_id);
  if (!ids.length) return;
  const at = ids.indexOf(state.selected);
  const next = ids[Math.max(0, Math.min(ids.length - 1, at + step))];
  selectCommit(next);
  document.querySelector(`#log .commit[data-id="${next}"]`)?.scrollIntoView({ block: "nearest" });
}

function renderJjFiles(result, query) {
  const added = result.files.reduce((sum, file) => sum + file.added, 0);
  const removed = result.files.reduce((sum, file) => sum + file.removed, 0);
  const count = result.files.length;
  const summary = h(
    "p",
    { class: "summary" },
    count ? `${count}${result.truncated ? "+" : ""} file${count === 1 ? "" : "s"} changed, ` : "No file changes.",
    count ? tally(added, removed) : null,
  );
  const files = result.files.map((file) => {
    const shell = fileShell(file, file.path, []);
    shell.counts.replaceChildren(file.binary ? "binary" : tally(file.added, file.removed));
    if (file.deferred) {
      const load = h("button", { type: "button" }, "Load this diff");
      load.addEventListener("click", async () => {
        load.disabled = true;
        try {
          const single = await api("/api/jj/diff", { ...query, path: file.path });
          shell.body.replaceChildren(...renderHunks(single.files[0] || { hunks: [], status: file.status }));
        } catch (err) {
          load.disabled = false;
          fail(err);
        }
      });
      shell.body.replaceChildren(h("p", { class: "file-note" }, "Left out to keep this page quick. ", load));
    } else {
      shell.body.replaceChildren(...renderHunks(file));
    }
    return shell.el;
  });
  return [summary, ...files];
}

async function renderDetail() {
  const detail = $("#detail");
  const id = state.selected;
  if (!id) {
    detail.replaceChildren(h("div", { class: "empty" }, h("h2", null, "Pick a change"), h("p", null, "Choose a change on the left to read its diff.")));
    return;
  }
  const comparing = state.base && state.base.commit_id !== id ? state.base : null;
  const path = state.filePath && !state.allFiles ? state.filePath : null;
  const query = { ws: state.ws, to: id, from: comparing?.commit_id, path, context: state.context };
  const result = await api("/api/jj/diff", query);
  if (state.selected !== id) return;
  const commit = result.to;
  const title = firstLine(commit.description);
  const rest = commit.description.split("\n").slice(1).join("\n").trim();
  const head = h(
    "div",
    { class: "change-head" },
    h("h2", { class: title ? "" : "none" }, commit.root ? "Root" : title || "No description"),
    rest && h("pre", null, rest),
    h(
      "div",
      { class: "change-meta" },
      h("code", { title: "Change id" }, idLabel(commit.change_id, commit.change_prefix, 12)),
      h("code", { title: "Commit id" }, idLabel(commit.commit_id, commit.commit_prefix, 12)),
      !commit.root && h("span", null, commit.author.name || commit.author.email || "unknown author"),
      !commit.root && when(commit.committer.time),
      h("span", null, commitTags(commit)),
    ),
  );
  const tabs = h(
    "div",
    { class: "tabs", role: "tablist" },
    ...[["diff", "Diff"], ["evolution", "Evolution"]].map(([key, label]) =>
      h("button", { type: "button", role: "tab", "aria-selected": String(state.tab === key), onclick: () => { state.tab = key; renderDetail().catch(fail); } }, label),
    ),
    h("span", { class: "spacer" }),
    !comparing && h("button", { type: "button", class: "plain", title: "Then pick another change to see everything between the two", onclick: () => setBase(commit) }, "Compare from here"),
  );
  const parts = [head];
  if (comparing) {
    parts.push(
      h(
        "div",
        { class: "compare" },
        h("span", null, "Comparing from ", h("code", null, comparing.change_id.slice(0, 8)), " ", firstLine(comparing.description) || "no description"),
        h("button", { type: "button", onclick: () => setBase(comparing) }, "Stop comparing"),
      ),
    );
  }
  parts.push(tabs);
  if (state.tab === "evolution") {
    parts.push(await renderEvolution(id));
  } else {
    if (path) {
      parts.push(
        h(
          "div",
          { class: "log-note" },
          h("span", null, "Only ", h("code", null, path), " is shown."),
          h("button", { type: "button", onclick: () => { state.allFiles = true; renderDetail().catch(fail); } }, "Show every file"),
        ),
      );
    }
    parts.push(...renderJjFiles(result, query));
  }
  if (state.selected !== id) return;
  detail.replaceChildren(...parts);
  detail.scrollTop = 0;
}

function operationLine(operation) {
  const args = operation.attributes.find(([key]) => key === "args")?.[1];
  return [
    h("span", { class: "what" }, operation.description || "operation"),
    args && h("span", { class: "args" }, args),
  ];
}

async function renderEvolution(id) {
  const entries = await api("/api/jj/evolution", { ws: state.ws, rev: id });
  const list = h("ol", { class: "steps" });
  for (const entry of entries) {
    const files = h("div", { class: "files", hidden: true });
    const previous = entry.predecessors[0];
    const show = previous && h("button", { type: "button" }, "Show what this step changed");
    if (show) {
      show.addEventListener("click", async () => {
        if (!files.hidden) {
          files.hidden = true;
          show.textContent = "Show what this step changed";
          return;
        }
        try {
          const query = { ws: state.ws, from: previous, to: entry.commit.commit_id, context: state.context };
          files.replaceChildren(...renderJjFiles(await api("/api/jj/diff", query), query));
          files.hidden = false;
          show.textContent = "Hide";
        } catch (err) {
          fail(err);
        }
      });
    }
    list.append(
      h(
        "li",
        { class: "step" },
        h(
          "div",
          { class: "step-line" },
          h("code", null, entry.commit.commit_id.slice(0, 8)),
          entry.operation ? operationLine(entry.operation) : h("span", { class: "what" }, "Recorded before operations were tracked"),
          show,
          when(entry.operation ? entry.operation.end : entry.commit.committer.time),
        ),
        files,
      ),
    );
  }
  return list;
}

function renderOperations(operations) {
  const list = h("ol", { class: "steps ops" });
  for (const operation of operations) {
    list.append(
      h(
        "li",
        { class: "step" },
        h(
          "div",
          { class: "step-line" },
          h("code", null, operation.id.slice(0, 12)),
          operationLine(operation),
          operation.workspace && h("span", { class: "tag" }, operation.workspace),
          when(operation.end),
        ),
      ),
    );
  }
  $("#log").replaceChildren(list);
}

function showFileHistory(workspace, path) {
  state.ws = workspace;
  state.filePath = path;
  state.allFiles = false;
  state.mode = "changes";
  state.selected = null;
  state.base = null;
  renderWorkspacePicker();
  setView("history");
}

// ---------------------------------------------------------------- guide
//
// The guide is a conversation with Codex, which the server runs, in a drawer
// on the right. Codex reads the root through the server, and acts on this
// page through it too: when it asks to show or highlight something, the
// server passes that on to the page that sent the latest message, which does
// it and says what it did (answerCall). The page hears about that, and about
// replies as they are written, by asking the server what has happened since
// it last asked (guidePoll).

const guide = {
  // Names this page to the server, which points on the page that last spoke.
  page: [...crypto.getRandomValues(new Uint8Array(8))].map((byte) => byte.toString(16).padStart(2, "0")).join(""),
  loaded: false,
  problem: null,
  seq: 0,
  running: false,
  entries: [],
  polling: false,
  // What the guide is pointing at: `{ path, side, line, end, hunk, note }`.
  marks: [],
  // The tour being stepped through: `{ id, step }`.
  tour: null,
  // What each entry was last drawn as, so that only what changed is redrawn.
  nodes: new Map(),
  // Requests from Codex are carried out one at a time, in order.
  calls: Promise.resolve(),
  // What the person last selected in a diff, kept while they write about it.
  selection: null,
  frame: 0,
  drawnCount: 0,
};

const guideOpen = () => !$("#guide").hidden;

/** Ways in for someone who has not said anything yet. */
const GUIDE_STARTS = [
  ["Walk me through the changes", "Walk me through the unreviewed changes, in the order that makes them easiest to understand."],
  ["What deserves the most care?", "Which of the unreviewed changes deserve the most careful review, and why?"],
  ["Explain what is on my screen", "Explain what I am looking at."],
];

function showGuide(open, focus = open) {
  // The stream wraps differently once it is narrower; keep the reader's place.
  keepPlace(() => ($("#guide").hidden = !open));
  $("#guide-toggle").setAttribute("aria-pressed", open);
  if (open !== setting("guide-open", false)) saveSetting("guide-open", open);
  if (!open) return;
  if (!guide.loaded) loadGuide().catch(fail);
  else guidePoll();
  describeContext();
  if (focus) $("#guide-input").focus();
}

async function loadGuide() {
  const got = await api("/api/guide", { page: guide.page });
  guide.loaded = true;
  guide.problem = got.problem;
  takeSnapshot(got);
  guidePoll();
}

function takeSnapshot(got) {
  guide.seq = got.seq;
  guide.running = got.running;
  guide.entries = got.entries;
  if (guide.tour && !guide.entries.some((entry) => entry.id === guide.tour.id)) guide.tour = null;
  drawGuide();
}

/** Asks for what happened, again and again, while the conversation is on show or under way. */
async function guidePoll() {
  if (guide.polling) return;
  guide.polling = true;
  const wanted = () =>
    guide.loaded && !guide.problem && state.rootPath && state.view !== "roots" && (guide.running || (guideOpen() && document.visibilityState === "visible"));
  try {
    while (wanted()) {
      let got;
      try {
        got = await api("/api/guide/events", { page: guide.page, after: guide.seq });
      } catch {
        // The server may be restarting; it answers again soon enough.
        await new Promise((resolve) => setTimeout(resolve, 3000));
        continue;
      }
      if (got.resync) takeSnapshot(got);
      else for (const event of got.events) guideEvent(event);
      guide.seq = got.seq;
      guide.running = got.running;
      drawGuide();
    }
  } finally {
    guide.polling = false;
  }
}

function guideEvent(event) {
  if (event.type === "entry") {
    const at = guide.entries.findIndex((entry) => entry.id === event.entry.id);
    if (at < 0) guide.entries.push(event.entry);
    else guide.entries[at] = event.entry;
    // A new tour starts at its first step, which Codex goes on to explain.
    if (event.entry.kind === "tour" && at < 0) guide.tour = { id: event.entry.id, step: 0 };
  } else if (event.type === "delta") {
    const entry = guide.entries.find((candidate) => candidate.id === event.id);
    if (entry) entry.text = (entry.text || "") + event.text;
  } else if (event.type === "status") {
    guide.running = event.running;
  } else if (event.type === "reset") {
    guide.entries = [];
    guide.tour = null;
    setMarks([]);
  } else if (event.type === "call" && event.page === guide.page) {
    guide.calls = guide.calls.then(() => answerCall(event));
  }
}

/** Does what Codex asked of the page, and tells the server how it went. */
async function answerCall({ call, tool, args }) {
  let answer;
  try {
    answer = { call, result: await GUIDE_TOOLS[tool](args || {}) };
  } catch (err) {
    answer = { call, error: err.message || String(err) };
  }
  await post("/api/guide/answer", answer).catch(() => {});
}

const GUIDE_TOOLS = {
  view: async () => describePage(),
  show: async (args) => showPlace(await placeOf(args), args.in === "editor"),
  highlight: (args) => highlight(args),
  clear_highlights: async (args) => {
    const path = args.path ? await resolvePath(args.path) : null;
    const before = guide.marks.length;
    setMarks(path ? guide.marks.filter((mark) => mark.path !== path) : []);
    return { cleared: before - guide.marks.length };
  },
};

async function sendGuide(text) {
  text = text.trim();
  if (!text) return;
  if (!guide.loaded) await loadGuide();
  if (guide.problem) return;
  $("#guide-input").value = "";
  try {
    await post("/api/guide/send", { page: guide.page, text, context: describePage(), effort: setting("guide-effort", "") || null });
  } catch (err) {
    // The server put what went wrong in the conversation.
    if (!err.status || err.status >= 500) fail(err);
  }
  guidePoll();
}

// ---- where things are

/**
 * The root-relative path that `path` names. Codex sometimes gives a path
 * within a workspace rather than within the root; that is fine as long as
 * only one file ends that way.
 */
async function resolvePath(path) {
  path = String(path || "").trim().replace(/^\.?\/+/, "");
  if (!path) throw new Error("a path is needed");
  const changed = (state.status?.entries || []).map((entry) => entry.path);
  if (changed.includes(path) || editor.files.includes(path)) return path;
  if (!editor.files.length) await refreshFiles();
  if (editor.files.includes(path)) return path;
  const matches = [...new Set([...changed, ...editor.files])].filter((candidate) => candidate.endsWith(`/${path}`));
  if (matches.length === 1) return matches[0];
  if (matches.length) throw new Error(`${path} could be any of ${matches.slice(0, 6).join(", ")}; give the path from the root`);
  throw new Error(`there is no file ${path} in this root`);
}

/** A place, as Codex or a link gives it, made definite. */
async function placeOf(args) {
  const whole = (value) => (Number.isInteger(value) && value > 0 ? value : null);
  const line = whole(args.line);
  const end = line ? Math.max(line, whole(args.end) || line) : null;
  return {
    path: await resolvePath(args.path),
    side: args.side === "old" ? "old" : "new",
    line,
    end,
    hunk: typeof args.hunk === "string" && args.hunk ? args.hunk : null,
  };
}

/** Which review side shows the file: the one on screen if it can. */
function reviewSideOf(path) {
  const has = (side) => reviewEntries(side).some((entry) => entry.path === path && !entry.directory);
  if (has(state.side)) return state.side;
  const other = state.side === "staged" ? "unstaged" : "staged";
  return has(other) ? other : null;
}

/** The file's section in the stream, its diff loaded. */
async function sectionFor(path) {
  if (!state.sections.has(path)) {
    // Its workspace may be folded away.
    const entry = reviewEntries().find((candidate) => candidate.path === path);
    if (entry && state.folded.delete(entry.workspace || "")) {
      renderSidebar();
      await syncStream();
    }
  }
  const section = state.sections.get(path);
  if (!section) return null;
  section.el.classList.remove("collapsed");
  while (section.loading) await new Promise((resolve) => setTimeout(resolve, 30));
  if (!section.loaded) await loadSection(section);
  return section;
}

/**
 * The diff rows of a place: the lines asked for, along with what is
 * interleaved with them (the lines they replaced, the other column), or a
 * whole hunk. Lines are looked for in the hunk asked for, if it is shown.
 */
function rowsFor(section, place) {
  const hunks = [...section.body.querySelectorAll(".hunk")];
  const named = place.hunk && hunks.filter((el) => el.hunk.id === place.hunk);
  const rows = [];
  for (const el of named?.length ? named : hunks) {
    const all = [...el.querySelectorAll(".line")];
    if (!place.line) {
      if (el.hunk.id === place.hunk) rows.push(...all);
      continue;
    }
    const number = (row) => {
      const line = el.hunk.lines[row.dataset.index];
      return place.side === "old" ? line.old : line.new;
    };
    const indexes = all.filter((row) => number(row) >= place.line && number(row) <= place.end).map((row) => Number(row.dataset.index));
    if (!indexes.length) continue;
    const [low, high] = [Math.min(...indexes), Math.max(...indexes)];
    rows.push(...all.filter((row) => row.dataset.index >= low && row.dataset.index <= high));
  }
  return rows;
}

/** Scrolls the stream so that the rows sit a little above its middle. */
function bringIntoView(rows) {
  const stream = $("#stream");
  const box = stream.getBoundingClientRect();
  const top = rows[0].getBoundingClientRect().top;
  const height = rows[rows.length - 1].getBoundingClientRect().bottom - top;
  // Room under the sticky file header.
  const room = box.height - 48;
  const above = height < room ? (room - height) / 3 : 0;
  releasePin();
  scrollStreamTo(stream.scrollTop + top - box.top - 48 - above);
}

/** Line numbers of the first and last rows, on the side asked for. */
function rowSpan(rows, side) {
  const numbers = rows
    .map((row) => row.closest(".hunk").hunk.lines[row.dataset.index][side === "old" ? "old" : "new"])
    .filter((number) => number != null);
  return numbers.length ? [Math.min(...numbers), Math.max(...numbers)] : null;
}

/**
 * Shows a place: in its diff, on whichever side of the review has it, or in
 * the editor if it has no diff (or `inEditor`).
 */
async function showPlace(place, inEditor = false) {
  if (state.view !== "review") setView("review");
  if (!state.status) await loadStatus();
  const side = inEditor ? null : reviewSideOf(place.path);
  if (side && side !== state.side) {
    await switchSide(side);
    writeHash();
  }
  const section = side && (await sectionFor(place.path));
  if (!section) return showInEditor(place);
  const rows = rowsFor(section, place);
  if ((place.line || place.hunk) && !rows.length) {
    if (place.hunk && !place.line) {
      throw new Error(`${place.path} has no hunk ${place.hunk} on the page now; it may have been reviewed or changed. Give lines instead.`);
    }
    // Lines outside every hunk: the editor has the whole file.
    if (place.side === "new" && onDisk(section.entry)) return showInEditor(place);
  }
  if (rows.length) {
    focusHunk(rows[0].closest(".hunk"), false);
    bringIntoView(rows);
  } else {
    scrollToKey(place.path);
  }
  return {
    shown: "diff",
    path: place.path,
    review: side === "staged" ? "reviewed" : "unreviewed",
    lines: rows.length ? rowSpan(rows, place.side) : null,
  };
}

/** Opens a file in the editor at a place, leaving the focus where it is. */
async function showInEditor(place) {
  if (place.side === "old") throw new Error(`the old lines of ${place.path} are only in its diff, and it has no diff on the page`);
  showEditor();
  if (!editor.buffers.has(place.path)) {
    const file = await api("/api/file", { path: place.path });
    if (!editor.buffers.has(place.path)) createBuffer(file);
  }
  activate(place.path);
  const buffer = editor.buffers.get(place.path);
  const { text } = buffer;
  const count = text.value.split("\n").length;
  const span = editorSpan(buffer, place);
  const lineHeight = parseFloat(getComputedStyle(text).lineHeight);
  text.scrollTop = span ? Math.max(0, (span[0] - 1) * lineHeight - text.clientHeight / 3) : 0;
  paintEditorMarks(buffer);
  return { shown: "editor", path: place.path, lines: span, of: count };
}

/** The lines of a place in an open file: those asked for, or those of the hunk named. */
function editorSpan(buffer, place) {
  const count = buffer.text.value.split("\n").length;
  if (place.line) return [Math.min(place.line, count), Math.min(place.end, count)];
  const hunk = place.hunk && [...(state.sections.get(place.path)?.body.querySelectorAll(".hunk") || [])].find((el) => el.hunk.id === place.hunk)?.hunk;
  if (!hunk || !hunk.new_count) return null;
  return [hunk.new_start, Math.min(count, hunk.new_start + hunk.new_count - 1)];
}

// ---- pointing

async function highlight(args) {
  const place = await placeOf(args);
  const mark = { ...place, note: typeof args.note === "string" ? args.note.trim().slice(0, 400) : "" };
  setMarks(args.add ? [...guide.marks, mark] : [mark]);
  if (args.scroll === false) return { highlighted: place.path };
  const shown = await showPlace(place);
  paintMarks();
  return { highlighted: true, ...shown };
}

function setMarks(marks) {
  guide.marks = marks;
  paintMarks();
}

/** Draws the guide's highlights wherever they are on show. */
function paintMarks() {
  keepPlace(() => {
    for (const section of state.sections.values()) if (section.loaded) markSection(section);
  });
  for (const buffer of editor.buffers.values()) paintEditorMarks(buffer);
}

function markSection(section) {
  section.el.classList.remove("guided");
  for (const el of section.el.querySelectorAll(".line.guided")) el.classList.remove("guided");
  for (const el of section.el.querySelectorAll(".guide-note")) el.remove();
  for (const mark of guide.marks) {
    if (mark.path !== section.path) continue;
    if (!mark.line && !mark.hunk) {
      section.el.classList.add("guided");
      continue;
    }
    const rows = rowsFor(section, mark);
    for (const row of rows) row.classList.add("guided");
    if (!mark.note || !rows.length) continue;
    const last = rows[rows.length - 1];
    const split = last.closest(".pair");
    (split || last).after(h("p", { class: split ? "guide-note split" : "guide-note" }, mark.note));
  }
}

/** Bands behind the editor's text for the highlights in its file, where it is scrolled to. */
function paintEditorMarks(buffer) {
  const marks = guide.marks.filter((mark) => mark.path === buffer.path && mark.side === "new");
  if (!marks.length && !buffer.marks.firstChild) return;
  const { text } = buffer;
  const style = getComputedStyle(text);
  const lineHeight = parseFloat(style.lineHeight);
  const top = parseFloat(style.paddingTop) - text.scrollTop;
  buffer.marks.replaceChildren(
    ...marks.map((mark) => {
      const span = editorSpan(buffer, mark);
      if (!span) return null;
      const placed = `top:${top + (span[0] - 1) * lineHeight}px;height:${(span[1] - span[0] + 1) * lineHeight}px`;
      return h("div", { class: "editor-mark", style: placed }, mark.note && h("span", { title: mark.note }, mark.note));
    }),
  );
}

// ---- what the page shows, for Codex

const clip = (text, most = 4000) => (text.length > most ? `${text.slice(0, most)}…` : text);

/** What the person selected in a diff: which file, which lines, what text. */
function describeSelection(selection) {
  const range = selection.getRangeAt(0);
  const start = range.startContainer.nodeType === Node.ELEMENT_NODE ? range.startContainer : range.startContainer.parentElement;
  const card = start?.closest("#stream .file");
  if (!card) return null;
  const rows = [...card.querySelectorAll(".line")].filter((row) => range.intersectsNode(row));
  return {
    path: card.dataset.key,
    lines: rows.length ? rowSpan(rows, "new") || rowSpan(rows, "old") : null,
    text: clip(selection.toString()),
  };
}

function describeEditor(buffer) {
  const { text } = buffer;
  const lineHeight = parseFloat(getComputedStyle(text).lineHeight);
  const count = text.value.split("\n").length;
  const first = Math.floor(text.scrollTop / lineHeight) + 1;
  const described = {
    file: buffer.path,
    lines_on_screen: [Math.min(first, count), Math.min(count, first + Math.floor(text.clientHeight / lineHeight))],
    unsaved_edits: isDirty(buffer),
  };
  if (text.selectionStart !== text.selectionEnd) {
    const from = text.value.slice(0, text.selectionStart).split("\n").length;
    const chosen = text.value.slice(text.selectionStart, text.selectionEnd);
    described.selected = { lines: [from, from + chosen.split("\n").length - 1], text: clip(chosen) };
  }
  return described;
}

/** What is on the page: what the guide's view tool answers, and what goes with each message. */
function describePage() {
  const described = { view: state.view, review: state.side === "staged" ? "reviewed" : "unreviewed" };
  if (state.view === "history") {
    described.history = { workspace: state.ws, revset: state.revset || null, commit: state.selected || null, file: state.filePath || null };
  } else {
    const box = $("#stream").getBoundingClientRect();
    const visible = (el) => {
      const rect = el.getBoundingClientRect();
      return rect.bottom > box.top && rect.top < box.bottom;
    };
    described.on_screen = [...document.querySelectorAll("#stream .file")].filter(visible).map((el) => ({
      path: el.dataset.key,
      hunks: [...el.querySelectorAll(".hunk")].filter(visible).map(({ hunk }) => ({ id: hunk.id, new: [hunk.new_start, hunk.new_count], old: [hunk.old_start, hunk.old_count] })),
    }));
    if (state.focused?.isConnected) described.focused = { path: state.focused.closest(".file").dataset.key, hunk: state.focused.hunk.id };
  }
  if (guide.selection) described.selected_in_diff = guide.selection;
  if (editorOpen()) described.editor = activeBuffer() ? describeEditor(activeBuffer()) : { file: null };
  if (guide.marks.length) described.highlights = guide.marks;
  const tour = guide.tour && guide.entries.find((entry) => entry.id === guide.tour.id);
  if (tour) described.tour = { title: tour.title, step: guide.tour.step + 1, of: tour.steps.length, step_title: tour.steps[guide.tour.step]?.title };
  return described;
}

/** Says, under the message box, what goes with a message. */
function describeContext() {
  if (!guideOpen()) return;
  const name = (path) => splitPath(path)[1];
  const buffer = editorOpen() && activeBuffer();
  const selected = buffer && buffer.text.selectionStart !== buffer.text.selectionEnd;
  const tour = guide.tour && guide.entries.find((entry) => entry.id === guide.tour.id);
  const parts = [
    guide.selection ? `your selection in ${name(guide.selection.path)}` : selected ? `your selection in ${name(buffer.path)}` : "what is on screen",
    tour && `step ${guide.tour.step + 1} of the tour`,
  ].filter(Boolean);
  $("#guide-context").textContent = `Sent along: ${parts.join(", ")}`;
}

// ---- the drawer

function drawGuide() {
  if (guide.frame) return;
  guide.frame = requestAnimationFrame(() => {
    guide.frame = 0;
    renderGuide();
  });
}

/** A short form of a path, for saying what was done to it. */
const shortPlace = (args = {}) => {
  const name = args.path ? splitPath(String(args.path))[1] : "";
  if (!args.line) return name;
  return `${name}:${args.line}${args.end && args.end !== args.line ? `-${args.end}` : ""}`;
};

function actLabel(entry) {
  const args = entry.args || {};
  switch (entry.tool) {
    case "changes": return args.side === "reviewed" ? "Listed the reviewed changes" : "Listed the changes";
    case "diff": return `Read the diff of ${shortPlace(args)}`;
    case "read": return `Read ${shortPlace(args)}${args.from ? ` from line ${args.from}` : ""}`;
    case "files": return args.match ? `Looked for files matching “${args.match}”` : "Listed the files";
    case "workspaces": return "Listed the workspaces";
    case "jj_log": return `Read the jj history of ${args.workspace || "a workspace"}`;
    case "jj_diff": return `Read jj change ${String(args.to || "").slice(0, 12)}`;
    case "view": return "Looked at your screen";
    case "show": return `Showed ${shortPlace(args)}`;
    case "highlight": return `Highlighted ${shortPlace(args)}`;
    case "clear_highlights": return "Cleared the highlights";
    case "tour": return "Planned a tour";
    default: return String(entry.tool || "Used a tool");
  }
}

function actRow(entry) {
  const running = entry.status === "inProgress";
  const failed = entry.ok === false || entry.status === "failed" || entry.status === "declined" || (entry.exit != null && entry.exit !== 0);
  const what =
    entry.kind === "command"
      ? h("code", null, String(entry.command || "").replace(/^\S+\s+-l?c\s+'([\s\S]*)'$/, "$1"))
      : ["show", "highlight"].includes(entry.tool) && entry.args?.path
        ? h("button", { type: "button", class: "g-redo", title: "Show it again", onclick: () => highlight({ ...entry.args, add: false }).catch(fail) }, actLabel(entry))
        : actLabel(entry);
  return h(
    "div",
    { class: `g-act${running ? " running" : ""}${failed ? " failed" : ""}` },
    h("span", { class: "g-what" }, entry.kind === "command" && "Ran ", what),
  );
}

/** The steps Codex took between messages, folded away once there are a few. */
function actsNode(entries) {
  const id = `acts:${entries[0].id}`;
  const key = JSON.stringify(entries.map((entry) => [entry.id, entry.status, entry.ok, entry.exit]));
  const cached = guide.nodes.get(id);
  if (cached?.key === key) return cached.node;
  const folded = entries.length > 2;
  // A group that folds stays one element, so that it stays open if opened.
  let node = cached?.node;
  if (!node || (node.tagName === "DETAILS") !== folded) node = h(folded ? "details" : "div", { class: "g-acts" });
  const busy = entries.some((entry) => entry.status === "inProgress");
  node.replaceChildren(...[folded && h("summary", null, `${entries.length} steps${busy ? " …" : ""}`), ...entries.map(actRow)].filter(Boolean));
  guide.nodes.set(id, { key, node });
  return node;
}

function tourNode(entry) {
  const current = guide.tour?.id === entry.id ? guide.tour.step : -1;
  const count = entry.steps.length;
  const step = (index) => () => goToStep(entry, index).catch(fail);
  return h(
    "section",
    { class: "g-tour", "aria-label": entry.title || "Walkthrough" },
    h(
      "header",
      null,
      h("h3", null, entry.title || "Walkthrough"),
      h("span", null, current < 0 ? `${count} steps` : `${current + 1} of ${count}`),
      h("button", { type: "button", disabled: current <= 0, onclick: step(current - 1) }, "Back"),
      h("button", { type: "button", class: current < 0 ? "mark" : null, disabled: current >= count - 1, onclick: step(current + 1) }, current < 0 ? "Start" : "Next"),
    ),
    h(
      "ol",
      null,
      entry.steps.map((item, index) =>
        h(
          "li",
          { class: index === current ? "g-step current" : "g-step" },
          h("button", { type: "button", "aria-current": index === current ? "step" : null, onclick: step(index) }, h("strong", null, item.title), item.explain && h("span", null, item.explain)),
        ),
      ),
    ),
  );
}

/** Goes to a step of a tour: highlights its places and shows the first. */
async function goToStep(entry, index) {
  const step = entry.steps[index];
  if (!step) return;
  guide.tour = { id: entry.id, step: index };
  drawGuide();
  describeContext();
  const places = [];
  for (const place of step.places || []) {
    try {
      places.push(await placeOf(place));
    } catch {
      // A place that is gone takes nothing else with it.
    }
  }
  setMarks(places.map((place, at) => ({ ...place, note: at === 0 ? step.title : "" })));
  if (places.length) await showPlace(places[0]);
  paintMarks();
}

function entryNode(entry) {
  const key = JSON.stringify([entry.text, entry.done, entry.kind === "tour" && guide.tour?.id === entry.id ? guide.tour.step : -1]);
  const cached = guide.nodes.get(entry.id);
  if (cached?.key === key) return cached.node;
  let node;
  if (entry.kind === "user") {
    node = h("div", { class: "g-user" }, entry.text);
  } else if (entry.kind === "agent") {
    node = cached?.node || h("div");
    node.className = `g-agent${entry.phase === "commentary" ? " commentary" : ""}${entry.done ? "" : " writing"}`;
    node.replaceChildren(...renderMarkdown(entry.text || ""));
  } else if (entry.kind === "tour") {
    node = tourNode(entry);
  } else {
    node = h("div", { class: entry.kind === "error" ? "g-error" : "g-note" }, entry.text);
  }
  guide.nodes.set(entry.id, { key, node });
  return node;
}

function renderGuide() {
  const log = $("#guide-log");
  const atEnd = log.scrollHeight - log.scrollTop - log.clientHeight < 48;
  $("#guide-state").textContent = guide.problem ? "Not available" : guide.running ? "Codex is working…" : guide.loaded ? "Codex, reading only" : "";
  $("#guide-stop").hidden = !guide.running;
  $("#guide-send").disabled = Boolean(guide.problem);
  $("#guide-input").disabled = Boolean(guide.problem);
  const nodes = [];
  if (guide.problem) {
    nodes.push(
      h(
        "div",
        { class: "g-problem" },
        h("p", null, guide.problem),
        h("button", { type: "button", onclick: () => loadGuide().catch(fail) }, "Try again"),
      ),
    );
  } else if (!guide.entries.length && guide.loaded) {
    nodes.push(
      h(
        "div",
        { class: "g-welcome" },
        h("h3", null, "Talk the changes through"),
        h("p", null, "Codex reads this root and can scroll and highlight the page as it explains. It cannot change anything."),
        h("div", { class: "g-starts" }, GUIDE_STARTS.map(([label, text]) => h("button", { type: "button", onclick: () => sendGuide(text) }, label))),
      ),
    );
  }
  let acts = [];
  const endActs = () => {
    if (acts.length) nodes.push(actsNode(acts));
    acts = [];
  };
  for (const entry of guide.problem ? [] : guide.entries) {
    if (entry.kind === "tool" || entry.kind === "command") {
      acts.push(entry);
      continue;
    }
    endActs();
    nodes.push(entryNode(entry));
  }
  endActs();
  const current = [...log.children];
  if (nodes.length !== current.length || nodes.some((node, i) => node !== current[i])) log.replaceChildren(...nodes);
  // Follow the conversation while reading its end, and after saying something.
  const said = guide.entries.at(-1)?.kind === "user" && guide.entries.length !== guide.drawnCount;
  guide.drawnCount = guide.entries.length;
  if (atEnd || said) log.scrollTop = log.scrollHeight;
}

// ---- replies, from Markdown

/** A place in the root that a link or a code span names: `path`, `path:12`, `path:12-20` or `path#L12-L20`. */
function parsePlace(target) {
  let text;
  try {
    text = decodeURIComponent(target);
  } catch {
    text = target;
  }
  const match = text.replace(/^file:\/\//, "").match(/^([^\s:#]+?)(?::(\d+)(?:-(\d+))?|#L(\d+)(?:-L?(\d+))?)?$/);
  if (!match || !/[./]/.test(match[1])) return null;
  const line = Number(match[2] || match[4]) || undefined;
  const end = Number(match[3] || match[5]) || line;
  return { path: match[1], line, end };
}

function placeButton(label, place) {
  const title = `Show ${place.path}${place.line ? `:${place.line}` : ""}`;
  return h("button", { type: "button", class: "g-place", title, onclick: () => highlight(place).catch(fail) }, label);
}

/** Code, bold and links in a line; `places` false for a link's own label, which cannot hold another. */
function inlineMarkdown(text, places = true) {
  const nodes = [];
  const pattern = /`([^`]+)`|\*\*([^*]+)\*\*|\[([^\]]+)\]\(([^)\s]+)\)|(https?:\/\/[^\s)<>]+)/g;
  let at = 0;
  for (const match of text.matchAll(pattern)) {
    nodes.push(text.slice(at, match.index));
    at = match.index + match[0].length;
    const [, code, bold, label, target, bare] = match;
    if (code != null) {
      const place = places && /^[\w.@~+-]+(\/[\w.@~+-]+)*:\d+(-\d+)?$/.test(code) && parsePlace(code);
      nodes.push(place ? placeButton(h("code", null, code), place) : h("code", null, code));
    } else if (bold != null) {
      nodes.push(h("strong", null, bold));
    } else {
      const url = target ?? bare;
      // A label may have code in it, but not another link.
      const shown = label == null ? url : inlineMarkdown(label, false);
      const place = !/^[a-z][a-z0-9+.-]*:\/\//i.test(url) && parsePlace(url);
      if (!places) nodes.push(...[shown].flat());
      else if (place) nodes.push(placeButton(shown, place));
      else if (/^https?:\/\//i.test(url)) nodes.push(h("a", { href: url, target: "_blank", rel: "noreferrer noopener" }, shown));
      else nodes.push(...[shown].flat());
    }
  }
  nodes.push(text.slice(at));
  return nodes.filter((node) => node !== "");
}

/** Paragraphs, lists, headings and code blocks, as elements: never as HTML, whatever the text says. */
function renderMarkdown(text) {
  const blocks = [];
  const lines = text.replace(/\r/g, "").split("\n");
  let paragraph = [];
  let list = null;
  const flush = () => {
    if (paragraph.length) blocks.push(h("p", null, inlineMarkdown(paragraph.join(" "))));
    paragraph = [];
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = line.match(/^\s*(```+|~~~+)/);
    if (fence) {
      flush();
      list = null;
      const body = [];
      while (++i < lines.length && !lines[i].trim().startsWith(fence[1])) body.push(lines[i]);
      blocks.push(h("pre", null, h("code", null, body.join("\n"))));
      continue;
    }
    if (!line.trim()) {
      flush();
      list = null;
      continue;
    }
    const heading = line.match(/^#{1,6}\s+(.*)$/);
    if (heading) {
      flush();
      list = null;
      blocks.push(h("h4", null, inlineMarkdown(heading[1])));
      continue;
    }
    const item = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
    if (item) {
      flush();
      const ordered = /\d/.test(item[1]);
      if (!list || list.ordered !== ordered) {
        const start = ordered && parseInt(item[1], 10) !== 1 ? { start: parseInt(item[1], 10) } : null;
        list = { ordered, el: h(ordered ? "ol" : "ul", start) };
        blocks.push(list.el);
      }
      list.el.append(h("li", null, inlineMarkdown(item[2])));
      continue;
    }
    if (list && /^\s+\S/.test(line)) {
      list.el.lastChild.append(" ", ...inlineMarkdown(line.trim()));
      continue;
    }
    list = null;
    paragraph.push(line.trim());
  }
  flush();
  return blocks;
}

function bindGuide() {
  widthHandle($("#guide"), "guide-width", "Resize jjent", keepPlace, true);
  $("#guide-toggle").addEventListener("click", () => showGuide(!guideOpen()));
  $("#guide-close").addEventListener("click", () => showGuide(false));
  const input = $("#guide-input");
  $("#guide-form").addEventListener("submit", (event) => {
    event.preventDefault();
    sendGuide(input.value).catch(fail);
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      sendGuide(input.value).catch(fail);
    } else if (event.key === "Escape") {
      $("#stream").focus({ preventScroll: true });
    }
  });
  input.addEventListener("focus", describeContext);
  $("#guide-stop").addEventListener("click", () => post("/api/guide/stop").catch(fail));
  $("#guide-reset").addEventListener("click", async () => {
    if (guide.entries.length && !confirm("Start the conversation over? Codex forgets what was said.")) return;
    await post("/api/guide/reset").catch(fail);
    setMarks([]);
  });
  const effort = $("#guide-effort");
  effort.value = setting("guide-effort", "");
  effort.addEventListener("change", () => saveSetting("guide-effort", effort.value || null));
  // A selection in a diff is kept while the person writes about it, and
  // dropped once they select nothing there.
  let noting = 0;
  document.addEventListener("selectionchange", () => {
    if (noting) return;
    noting = requestAnimationFrame(() => {
      noting = 0;
      const selection = getSelection();
      if (!selection.anchorNode || !$("#stream").contains(selection.anchorNode)) return;
      guide.selection = selection.isCollapsed ? null : describeSelection(selection);
      describeContext();
    });
  });
  document.addEventListener("visibilitychange", () => document.visibilityState === "visible" && guidePoll());
  addEventListener("pagehide", () => guide.loaded && post("/api/guide/leave", { page: guide.page }).catch(() => {}));
}

// ---------------------------------------------------------------- menu
//
// The projj button at the top left opens a panel for what is not about the
// root on show: for now, choosing another.

function showMenu(open) {
  $("#menu").hidden = !open;
  $("#menu-toggle").setAttribute("aria-expanded", open);
  if (open) $("#menu-choose").focus();
}

function bindMenu() {
  const toggle = $("#menu-toggle");
  const menu = $("#menu");
  toggle.addEventListener("click", () => showMenu(menu.hidden));
  document.addEventListener(
    "keydown",
    (event) => {
      if (event.key !== "Escape" || menu.hidden) return;
      event.stopPropagation();
      showMenu(false);
      toggle.focus();
    },
    true,
  );
  document.addEventListener("pointerdown", (event) => {
    if (!menu.hidden && !menu.contains(event.target) && !toggle.contains(event.target)) showMenu(false);
  });
}

function renderMenu() {
  const root = state.view !== "roots" && state.root;
  $("#menu-current").replaceChildren(
    h("span", { class: "menu-label" }, root ? "Reviewing" : "Nothing open"),
    root && h("strong", null, root.name),
    // Cut at its start when long; the end says which root it is.
    root && h("span", { class: "menu-path", title: root.path }, h("bdi", null, root.label)),
  );
  const choose = $("#menu-choose");
  if (root) choose.removeAttribute("aria-current");
  else choose.setAttribute("aria-current", "page");
}

// ---------------------------------------------------------------- roots
//
// Choosing a root: where a server with nothing to review opens, and where
// "Choose a projj", in the menu behind the projj button, leads. One box does it all. Words search the
// repositories found on this device, recent ones first; a path (starting
// with / or ~) completes folder by folder, and a folder that is in no
// repository yet offers to become a new root, as does one picked with the
// folder chooser.

const rootUrl = (path) => `?root=${encodeURIComponent(path)}`;
const openRoot = (path) => location.assign(rootUrl(path));
const isTyped = (query) => /^[/~]/.test(query.trim());

const roots = { query: null, hits: [], typed: null, scanning: false, lookedAt: 0, active: -1, seq: 0, timer: null, busy: false };

const KIND_LABEL = { projj: "projj root", git: "git", folder: "folder" };

/** The options the list shows for the latest answer, in order. */
function rootOptions() {
  const options = [];
  const typed = roots.typed;
  let fresh = null;
  if (typed?.kind === "root") {
    options.push({
      action: "open",
      path: typed.root,
      label: typed.root_label,
      kind: typed.projj ? "projj" : "git",
      title: typed.inside ? "Open the root this folder is in" : "Open",
    });
  } else if (typed?.kind === "folder" || (typed?.kind === "missing" && typed.creatable)) {
    fresh = {
      action: "init",
      path: typed.path,
      label: typed.label,
      kind: "new",
      title: typed.kind === "folder" ? "Start a new root here" : "Make the folder, start a root",
    };
  }
  const typing = isTyped(roots.query || "");
  for (const hit of roots.hits) {
    if (typed?.kind === "root" && hit.path === typed.root) continue;
    options.push({
      action: hit.kind === "folder" ? "into" : "open",
      path: hit.path,
      label: hit.label,
      kind: hit.kind,
      opened: hit.opened,
      group: typing ? "Folders" : hit.opened != null ? "Recent" : "On this device",
    });
  }
  // Last, so that going through folders on the way somewhere is not a
  // string of offers; first when there is nowhere further to go.
  if (fresh) options[options.length ? "push" : "unshift"]({ ...fresh, group: options.length ? "New root" : null });
  return options;
}

function optionRow(option, index) {
  const [dir, name] = splitPath(option.label);
  const row = h(
    "li",
    {
      role: "option",
      id: `roots-option-${index}`,
      class: `roots-option kind-${option.kind}`,
      "aria-selected": index === roots.active ? "true" : "false",
      dataset: { index },
      onpointerdown: (event) => event.preventDefault(),
      onclick: () => act(option),
      onpointermove: () => index !== roots.active && setActive(index, false),
    },
    h("span", { class: "roots-kind" }, option.kind === "new" ? "new root" : KIND_LABEL[option.kind]),
    h("span", { class: "roots-path" }, dir && h("span", { class: "dir" }, dir), h("span", { class: "name" }, name || dir)),
    option.title && h("span", { class: "roots-action" }, option.title),
    option.opened != null && h("span", { class: "roots-when" }, relativeTime(option.opened)),
    option.kind === "folder" && h("span", { class: "roots-action" }, "Tab to go in"),
    option.opened != null &&
      h("button", {
        type: "button",
        class: "quiet roots-forget",
        title: "Take this off the recent list",
        "aria-label": `Take ${option.label} off the recent list`,
        onclick: async (event) => {
          event.stopPropagation();
          await request("DELETE", "/api/roots/recent", { path: option.path }).catch(fail);
          searchRoots(true);
        },
      }, "×"),
  );
  return row;
}

function renderRoots() {
  const list = $("#roots-list");
  const options = rootOptions();
  roots.options = options;
  if (roots.active >= options.length) roots.active = options.length - 1;
  if (roots.active < 0 && options.length) roots.active = 0;
  const rows = [];
  let group = null;
  options.forEach((option, index) => {
    if (option.group && option.group !== group) {
      group = option.group;
      rows.push(h("li", { role: "presentation", class: "roots-group" }, group));
    }
    rows.push(optionRow(option, index));
  });
  list.replaceChildren(...rows);
  const open = document.activeElement === $("#roots-query") && rows.length > 0;
  list.hidden = !open;
  $("#roots-query").setAttribute("aria-expanded", open);
  setActive(roots.active, false);
  renderRootsStatus();
}

function renderRootsStatus() {
  const typed = roots.typed;
  const query = (roots.query || "").trim();
  let text = "";
  if (typed?.kind === "file") text = `${typed.label} is a file, not a folder.`;
  else if (typed?.kind === "missing" && !typed.creatable) text = `Nothing is at ${typed.label}, nor at the folder it would go in.`;
  else if (roots.scanning) text = `Looking through your folders for repositories… ${roots.lookedAt.toLocaleString()} so far.`;
  else if (!isTyped(query) && query && !roots.hits.length) text = "No repository found by that name. Try a path, or choose a folder.";
  $("#roots-status").textContent = text;
}

function setActive(index, scroll = true) {
  roots.active = index;
  for (const row of document.querySelectorAll("#roots-list [role=option]")) {
    row.setAttribute("aria-selected", Number(row.dataset.index) === index ? "true" : "false");
  }
  const row = $(`#roots-option-${index}`);
  const input = $("#roots-query");
  if (row) input.setAttribute("aria-activedescendant", row.id);
  else input.removeAttribute("aria-activedescendant");
  if (row && scroll) row.scrollIntoView({ block: "nearest" });
}

/** Asks for what matches the box, and again while the device is still being looked through. */
async function searchRoots(force = false) {
  const query = $("#roots-query").value;
  if (!force && query === roots.query) return;
  const seq = ++roots.seq;
  clearTimeout(roots.timer);
  let found;
  try {
    found = await api("/api/roots", { q: query.trim() });
  } catch (err) {
    if (seq === roots.seq) $("#roots-status").textContent = err.message;
    return;
  }
  if (seq !== roots.seq) return;
  const changed = query !== roots.query;
  Object.assign(roots, { query, hits: found.hits, typed: found.typed, scanning: found.scanning, lookedAt: found.looked_at });
  if (changed) roots.active = 0;
  renderRoots();
  if (found.scanning) roots.timer = setTimeout(() => searchRoots(true), 700);
}

/** Puts `path` in the box, ready to go further into it. */
function goInto(path) {
  const input = $("#roots-query");
  input.value = path.endsWith("/") ? path : `${path}/`;
  input.classList.add("typed");
  input.focus();
  searchRoots();
}

async function startRoot(path) {
  if (roots.busy) return;
  roots.busy = true;
  try {
    const made = await post("/api/roots/init", { path });
    toast(`Started a new root at ${made.root}.`);
    openRoot(made.root);
  } catch (err) {
    fail(err);
  } finally {
    roots.busy = false;
  }
}

function act(option) {
  if (!option) return;
  if (option.action === "open") openRoot(option.path);
  else if (option.action === "init") startRoot(option.path);
  else if (option.action === "into") goInto(option.label);
}

/**
 * Offers to start a root in a folder that is in no repository: the one
 * projj was started in, or one just chosen. A root opens straight away.
 */
function offer(look, lede) {
  const box = $("#roots-offer");
  if (!look) return void (box.hidden = true);
  if (look.kind === "root") return openRoot(look.root);
  if (look.kind === "file") return toast(`${look.label} is a file, not a folder.`, true);
  if (look.kind === "missing" && !look.creatable) return toast(`Nothing is at ${look.label}.`, true);
  box.replaceChildren(
    h("p", null, lede, " ", h("strong", { class: "roots-offer-path" }, look.label), look.kind === "missing" ? ", which does not exist yet." : ", which is not in a git repository yet."),
    h("p", { class: "roots-offer-note" }, "Starting a root there makes it a git repository with an empty track/ folder, ready for jj repositories. Nothing else is written."),
    h(
      "div",
      { class: "roots-offer-actions" },
      h("button", { type: "button", class: "mark", onclick: () => startRoot(look.path) }, "Start a new root here"),
      h("button", { type: "button", class: "quiet", onclick: () => offer(null) }, "Not now"),
    ),
  );
  box.hidden = false;
}

async function chooseFolder() {
  const button = $("#roots-choose");
  button.disabled = true;
  // Starts where the box points, if it points at a folder.
  const start = roots.typed?.kind === "folder" || roots.typed?.kind === "root" ? roots.typed.path : null;
  try {
    const look = await post("/api/roots/choose", { start });
    if (look) offer(look, "You chose");
  } catch (err) {
    fail(err);
  } finally {
    button.disabled = false;
  }
}

function bindRoots() {
  const input = $("#roots-query");
  let timer;
  input.addEventListener("input", () => {
    input.classList.toggle("typed", isTyped(input.value));
    clearTimeout(timer);
    timer = setTimeout(searchRoots, isTyped(input.value) ? 60 : 120);
  });
  input.addEventListener("focus", () => renderRoots());
  input.addEventListener("blur", () => {
    $("#roots-list").hidden = true;
    input.setAttribute("aria-expanded", "false");
  });
  input.addEventListener("keydown", async (event) => {
    const acting = event.key === "Enter" || (event.key === "Tab" && !event.shiftKey && isTyped(input.value));
    if (acting) event.preventDefault();
    // What was typed last may not have been looked up yet.
    if (acting && input.value !== roots.query) {
      clearTimeout(timer);
      await searchRoots();
    }
    const options = roots.options || [];
    const option = options[roots.active];
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if ($("#roots-list").hidden) return renderRoots();
      if (!options.length) return;
      const step = event.key === "ArrowDown" ? 1 : -1;
      setActive((roots.active + step + options.length) % options.length);
    } else if (event.key === "Enter") {
      act(option);
    } else if (acting) {
      // Tab completes the path, folder by folder, as a shell does.
      const into = option && option.kind !== "new" ? option : options.find((each) => each.kind !== "new");
      if (into) goInto(into.label);
    } else if (event.key === "Escape") {
      if (input.value) {
        input.value = "";
        searchRoots();
      } else input.blur();
    }
  });
  $("#roots-choose").addEventListener("click", chooseFolder);
}

/** Shows the chooser instead of a review. `path` is a folder to offer to start a root in, `notice` why it is shown. */
async function enterRoots({ begin = null, path = null, notice = null } = {}) {
  state.rootPath = null;
  state.view = "roots";
  document.body.dataset.mode = "roots";
  for (const id of ["#review", "#history"]) $(id).hidden = true;
  $("#roots").hidden = false;
  document.title = "Choose a projj \u2013 projj";
  renderMenu();
  desktop?.showing(null, document.title);
  bindRoots();
  if (notice) toast(notice, true);
  begin ??= await api("/api/start").catch((err) => (fail(err), null));
  $("#roots-choose").hidden = !begin?.chooser;
  if (path) {
    const look = await api("/api/roots/inspect", { path }).catch(() => null);
    if (look && look.kind !== "root") offer(look, "This is");
  } else if (begin?.here && begin.here.kind !== "root") {
    offer(begin.here, "projj was started in");
  }
  $("#roots-query").focus();
  await searchRoots(true);
}

// ------------------------------------------------------------- app shell

function writeHash() {
  const params = new URLSearchParams();
  if (state.view === "history") {
    if (state.ws) params.set("ws", state.ws);
    if (state.revset) params.set("revset", state.revset);
    if (state.filePath) params.set("file", state.filePath);
    if (state.selected) params.set("rev", state.selected);
    if (state.mode !== "changes") params.set("mode", state.mode);
  } else if (state.side !== "unstaged") {
    params.set("side", state.side);
  }
  const query = params.toString();
  history.replaceState(null, "", `#${state.view}${query ? `?${query}` : ""}`);
}

function readHash() {
  const [view, query] = location.hash.slice(1).split("?");
  const params = new URLSearchParams(query || "");
  if (view === "history" || view === "review") state.view = view;
  if (params.get("side") === "staged") state.side = "staged";
  state.ws = params.get("ws") || state.ws;
  state.revset = params.get("revset") || "";
  state.filePath = params.get("file");
  state.selected = params.get("rev");
  if (params.get("mode") === "operations") state.mode = "operations";
}

function setView(view) {
  state.view = view;
  $("#review").hidden = view !== "review";
  $("#history").hidden = view !== "history";
  for (const button of document.querySelectorAll(".views button")) {
    button.setAttribute("aria-pressed", button.dataset.view === view);
  }
  writeHash();
  if (view === "history") {
    $("#revset").value = state.revset;
    loadHistory();
  } else {
    loadStatus().catch(fail);
  }
}

async function reload() {
  await loadRoot();
  if (state.view === "history") return loadHistory();
  state.statusText = "";
  for (const section of state.sections.values()) section.signature = null;
  await loadStatus();
}

// ------------------------------------------------------------- settings
//
// Display settings are kept by the server, in the store that every projj
// server on the device shares, so they are the same on every port and in
// every browser. This browser keeps a copy only so that the first paint has
// the right theme before the server has answered.

const SETTINGS_COPY = "projj-settings";
const settings = { values: {}, timers: new Map() };

function setting(key, fallback) {
  return Object.hasOwn(settings.values, key) ? settings.values[key] : fallback;
}

/** Records a setting; null forgets it. Rapid changes, as from a slider, are sent once they settle. */
function saveSetting(key, value) {
  if (value == null) delete settings.values[key];
  else settings.values[key] = value;
  localStorage.setItem(SETTINGS_COPY, JSON.stringify(settings.values));
  clearTimeout(settings.timers.get(key));
  settings.timers.set(
    key,
    setTimeout(() => {
      settings.timers.delete(key);
      post("/api/settings", { key, value: value ?? null }).catch((err) => toast(`Settings could not be kept: ${err.message}`, true));
    }, 250),
  );
}

async function loadSettings() {
  try {
    settings.values = JSON.parse(localStorage.getItem(SETTINGS_COPY)) || {};
  } catch {
    settings.values = {};
  }
  applyTheme(setting("theme", matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));
  try {
    settings.values = (await api("/api/settings")).settings;
    localStorage.setItem(SETTINGS_COPY, JSON.stringify(settings.values));
  } catch (err) {
    toast(`Settings could not be read: ${err.message}`, true);
  }
  applyTheme(setting("theme", document.documentElement.dataset.theme));
}

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  const label = theme === "dark" ? "Switch to light" : "Switch to dark";
  $("#theme").setAttribute("aria-label", label);
  $("#theme").title = label;
  applyTweaks();
  if ($("#tweaks").open) renderTweaks();
}

function setTheme(theme) {
  applyTheme(theme);
  saveSetting("theme", theme);
}

// --------------------------------------------------------------- tweaks

/**
 * Reading settings, chosen in the Tweaks panel, separately for the light
 * and the dark theme. `initial` is the default, or one per theme.
 */
const TWEAKS = [
  { key: "lineTint", label: "Changed line background", min: 0, max: 100, step: 5, initial: 100, show: (v) => `${v}%` },
  { key: "markStrength", label: "Changed word marks", min: 0, max: 100, step: 5, initial: 50, show: (v) => `${v}%` },
  { key: "underline", label: "Underline changed words", initial: false },
  { key: "syntax", label: "Syntax color", min: 0, max: 100, step: 5, initial: 100, show: (v) => `${v}%` },
  { key: "codeSize", label: "Code size", min: 10, max: 18, step: 0.5, initial: 12.5, show: (v) => `${v}px` },
  { key: "leading", label: "Line spacing", min: 1.2, max: 2.2, step: 0.05, initial: 1.5, show: (v) => `${v.toFixed(2)}×` },
];
const THEMES = ["light", "dark"];

const currentTheme = () => (document.documentElement.dataset.theme === "dark" ? "dark" : "light");
const defaultTweak = (tweak, theme) => (typeof tweak.initial === "object" ? tweak.initial[theme] : tweak.initial);

/** A tweak's value made safe, or undefined if it cannot be. */
function cleanTweak(tweak, value) {
  if (typeof tweak.initial === "boolean") return typeof value === "boolean" ? value : undefined;
  if (typeof value !== "number" || !Number.isFinite(value)) return undefined;
  const stepped = Math.round((value - tweak.min) / tweak.step) * tweak.step + tweak.min;
  return Math.min(tweak.max, Math.max(tweak.min, Number(stepped.toFixed(4))));
}

/** Every tweak for `theme`: what was chosen, or else the default. */
function tweaksFor(theme) {
  const chosen = setting("tweaks", {})[theme] || {};
  return Object.fromEntries(TWEAKS.map((tweak) => [tweak.key, cleanTweak(tweak, chosen[tweak.key]) ?? defaultTweak(tweak, theme)]));
}

/** Keeps `values` for `theme`, recording only what differs from the defaults. */
function saveTweaks(theme, values) {
  const all = { ...setting("tweaks", {}) };
  const changed = Object.fromEntries(TWEAKS.filter((tweak) => values[tweak.key] !== defaultTweak(tweak, theme)).map(({ key }) => [key, values[key]]));
  if (Object.keys(changed).length) all[theme] = changed;
  else delete all[theme];
  saveSetting("tweaks", Object.keys(all).length ? all : null);
  applyTweaks();
}

/** Turns the current theme's tweaks into the CSS variables that style.css reads. */
function applyTweaks() {
  const values = tweaksFor(currentTheme());
  const root = document.documentElement;
  const mix = (color, percent, base) => `color-mix(in srgb, var(${color}) ${percent}%, ${base})`;
  for (const side of ["add", "del"]) {
    root.style.setProperty(`--${side}-line`, mix(`--${side}`, values.lineTint, "var(--sheet)"));
    // Up to the middle, a mark grows out of its line's background into the
    // stock mark color; past it, the mark deepens toward its side's ink.
    const strength = values.markStrength;
    const mark =
      strength <= 50
        ? mix(`--${side}-strong`, strength * 2, `var(--${side}-line)`)
        : mix(`--${side}-ink`, (strength - 50) * 0.9, `var(--${side}-strong)`);
    root.style.setProperty(`--${side}-mark`, mark);
  }
  root.style.setProperty("--syntax", `${values.syntax}%`);
  root.style.setProperty("--code-size", `${values.codeSize}px`);
  // Whole pixels, so that the editor's text and its colored copy agree.
  root.style.setProperty("--code-line", `${Math.round(values.codeSize * values.leading)}px`);
  root.toggleAttribute("data-mark-underline", values.underline);
  const buffer = activeBuffer();
  if (buffer) paintColors(buffer, true);
}

/** Both themes' tweaks in full, as JSON a person can keep, share or edit. */
const exportTweaks = () => JSON.stringify(Object.fromEntries(THEMES.map((theme) => [theme, tweaksFor(theme)])), null, 2);

/**
 * Takes tweaks from JSON shaped like `exportTweaks` makes. Themes and keys it
 * does not know are ignored, values are kept within range, and keys left out
 * keep their current value. Returns what it did, for telling the person.
 */
function importTweaks(text) {
  const given = JSON.parse(text);
  if (!given || typeof given !== "object" || Array.isArray(given)) throw new Error("expected an object with light and dark");
  const report = [];
  for (const theme of THEMES) {
    if (!given[theme] || typeof given[theme] !== "object") continue;
    const values = tweaksFor(theme);
    let taken = 0;
    for (const tweak of TWEAKS) {
      if (!Object.hasOwn(given[theme], tweak.key)) continue;
      const value = cleanTweak(tweak, given[theme][tweak.key]);
      if (value === undefined) throw new Error(`${theme}.${tweak.key} is not a ${typeof tweak.initial}`);
      values[tweak.key] = value;
      taken++;
    }
    saveTweaks(theme, values);
    report.push(`${taken} for ${theme}`);
  }
  if (!report.length) throw new Error("no light or dark settings found");
  return report.join(", ");
}

/** A few lines that show every setting at work, whatever the page holds. */
function tweakSample() {
  const at = (text, part, kind) => {
    const start = text.indexOf(part);
    return kind ? [start, start + part.length, kind] : [start, start + part.length];
  };
  const line = (kind, old, number, text, marks, syn) => ({ kind, old, new: number, text, hl: marks, syn });
  const head = "fn total(xs: &[Item]) -> u32 {";
  const before = "  xs.iter().map(|x| x.price).sum()";
  const after = "  xs.iter().map(|x| x.cost * x.count).sum()";
  const note = "  // counts were left out";
  const calls = (text) => ["iter", "map", "sum"].map((name) => at(text, name, "f"));
  const lines = [
    line(" ", 1, 1, head, null, [at(head, "fn", "k"), at(head, "total", "f"), at(head, "Item", "t"), at(head, "u32", "t")]),
    line("-", 2, null, before, [at(before, "price")], calls(before)),
    line("+", null, 2, after, [at(after, "cost * x.count")], calls(after)),
    line("+", null, 3, note, [[2, note.length]], [[2, note.length, "c"]]),
    line(" ", 3, 4, "}", null, []),
  ];
  return h("div", { class: "lines" }, lines.map((one, index) => renderLine(one, index)));
}

function renderTweaks() {
  const theme = currentTheme();
  const values = tweaksFor(theme);
  const change = (key, value) => {
    values[key] = value;
    saveTweaks(theme, values);
    showTweaksJson();
  };
  const controls = TWEAKS.map((tweak) => {
    const { key, label } = tweak;
    if (typeof tweak.initial === "boolean") {
      const box = h("input", { type: "checkbox", checked: values[key] });
      box.addEventListener("change", () => change(key, box.checked));
      return h("label", { class: "tweak check" }, box, label);
    }
    const output = h("output", null, tweak.show(values[key]));
    const range = h("input", { type: "range", min: tweak.min, max: tweak.max, step: tweak.step, value: values[key], "aria-label": label });
    range.addEventListener("input", () => {
      output.textContent = tweak.show(Number(range.value));
      change(key, Number(range.value));
    });
    return h("label", { class: "tweak" }, h("span", null, label), output, range);
  });
  for (const button of document.querySelectorAll("#tweaks-theme button")) {
    button.setAttribute("aria-pressed", String(button.dataset.theme === theme));
  }
  $("#tweaks-reset").textContent = `Reset ${theme} to defaults`;
  $("#tweaks-controls").replaceChildren(...controls);
  $("#tweaks-sample").replaceChildren(tweakSample());
  showTweaksJson();
}

/** Shows the current settings as JSON, unless the person is editing it. */
function showTweaksJson(force = false) {
  const text = $("#tweaks-json-text");
  if (force || (document.activeElement !== text && !text.dataset.edited)) {
    text.value = exportTweaks();
    delete text.dataset.edited;
  }
}

function showTweaks(shown) {
  const panel = $("#tweaks");
  if (shown && !panel.open) {
    renderTweaks();
    panel.show();
    $("#tweaks-controls input").focus();
  } else if (!shown && panel.open) {
    panel.close();
  }
  $("#tweaks-open").setAttribute("aria-pressed", String(shown));
}

function bindTweaks() {
  $("#tweaks-open").addEventListener("click", () => showTweaks(!$("#tweaks").open));
  $("#tweaks-close").addEventListener("click", () => showTweaks(false));
  $("#tweaks-done").addEventListener("click", () => showTweaks(false));
  $("#tweaks-reset").addEventListener("click", () => {
    const theme = currentTheme();
    saveTweaks(theme, Object.fromEntries(TWEAKS.map((tweak) => [tweak.key, defaultTweak(tweak, theme)])));
    renderTweaks();
  });
  for (const button of document.querySelectorAll("#tweaks-theme button")) {
    button.addEventListener("click", () => setTheme(button.dataset.theme));
  }
  $("#tweaks").addEventListener("keydown", (event) => event.key === "Escape" && showTweaks(false));

  const text = $("#tweaks-json-text");
  const note = (message, isError = false) => {
    $("#tweaks-json-note").textContent = message;
    $("#tweaks-json-note").classList.toggle("error", isError);
  };
  text.addEventListener("input", () => (text.dataset.edited = "1"));
  $("#tweaks-copy").addEventListener("click", async () => {
    showTweaksJson(true);
    try {
      await navigator.clipboard.writeText(text.value);
      note("Copied.");
    } catch {
      text.select();
      note("Select all and copy by hand; the browser would not allow it.", true);
    }
  });
  // The desktop app's window has nowhere to save a download to.
  $("#tweaks-download").hidden = Boolean(desktop);
  $("#tweaks-download").addEventListener("click", () => {
    const link = h("a", { href: URL.createObjectURL(new Blob([exportTweaks()], { type: "application/json" })), download: "projj-tweaks.json" });
    link.click();
    URL.revokeObjectURL(link.href);
  });
  const apply = (json) => {
    try {
      note(`Imported ${importTweaks(json)}.`);
      renderTweaks();
      showTweaksJson(true);
    } catch (err) {
      note(`Not imported: ${err.message}`, true);
    }
  };
  $("#tweaks-apply").addEventListener("click", () => apply(text.value));
  const file = $("#tweaks-file");
  $("#tweaks-open-file").addEventListener("click", () => file.click());
  file.addEventListener("change", async () => {
    if (file.files[0]) apply(await file.files[0].text());
    file.value = "";
  });
}

function bind() {
  for (const button of document.querySelectorAll(".views button")) {
    button.addEventListener("click", () => setView(button.dataset.view));
  }
  for (const tab of document.querySelectorAll(".sides [data-side]")) {
    tab.addEventListener("click", () => { switchSide(tab.dataset.side); writeHash(); });
  }
  for (const tab of document.querySelectorAll(".seg [data-mode]")) {
    tab.addEventListener("click", () => { state.mode = tab.dataset.mode; loadHistory(); });
  }
  $("#context").addEventListener("change", (event) => {
    state.context = Number(event.target.value);
    saveSetting("context", state.context);
    reload().catch(fail);
  });
  $("#layout").addEventListener("change", (event) => {
    state.layout = event.target.value;
    saveSetting("layout", state.layout);
    reload().catch(fail);
  });
  $("#refresh").addEventListener("click", () => reload().catch(fail));
  $("#theme").addEventListener("click", () => setTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark"));
  $("#help-open").addEventListener("click", () => $("#help").showModal());
  bindTweaks();
  $("#workspace").addEventListener("change", (event) => {
    Object.assign(state, { ws: event.target.value, filePath: null, selected: null, base: null, allFiles: false });
    renderWorkspacePicker();
    loadHistory();
  });
  $("#revset-form").addEventListener("submit", (event) => {
    event.preventDefault();
    Object.assign(state, { revset: $("#revset").value.trim(), filePath: null, mode: "changes" });
    loadHistory();
  });
  $("#snapshot").addEventListener("click", async (event) => {
    event.target.disabled = true;
    try {
      const result = await post("/api/jj/snapshot", null, { ws: state.ws });
      toast(result.message || "Working copy snapshotted.");
      await loadHistory();
    } catch (err) {
      fail(err);
    } finally {
      event.target.disabled = false;
    }
  });
  let ticking = false;
  $("#stream").addEventListener("scroll", () => {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(() => { ticking = false; markCurrentFile(); });
  });
  // A pinned file lets go as soon as the person scrolls some other way.
  const stream = $("#stream");
  for (const type of ["wheel", "touchstart", "pointerdown"]) stream.addEventListener(type, releasePin, { passive: true });
  stream.addEventListener("scroll", () => Math.abs(stream.scrollTop - pin.top) > 1 && releasePin(), { passive: true });
  const SCROLL_KEYS = new Set(["PageUp", "PageDown", "Home", "End", "ArrowUp", "ArrowDown", " "]);
  document.addEventListener("keydown", (event) => SCROLL_KEYS.has(event.key) && releasePin(), true);
  bindEditor();
  bindGuide();
  document.addEventListener("keydown", onKey);
  document.addEventListener("visibilitychange", () => document.visibilityState === "visible" && poll());
  setInterval(poll, 4000);
}

/** Picks up edits made by agents or in a terminal while the page is open. */
function poll() {
  if (document.visibilityState !== "visible" || state.view !== "review" || !state.rootPath) return;
  loadStatus().catch(() => {});
}

function onKey(event) {
  if (event.metaKey || event.ctrlKey || event.altKey || state.view === "roots") return;
  if (event.target.closest?.("input, select, textarea, dialog")) return;
  const review = state.view === "review";
  const actions = {
    "1": () => setView("review"),
    "2": () => setView("history"),
    "?": () => $("#help").showModal(),
    j: () => (review ? moveFocus(1) : moveCommit(1)),
    k: () => (review ? moveFocus(-1) : moveCommit(-1)),
    "]": () => review && moveFile(1),
    "[": () => review && moveFile(-1),
    t: () => review && (switchSide(state.side === "staged" ? "unstaged" : "staged"), writeHash()),
    r: () => review && state.side === "unstaged" && state.focused?.apply(),
    u: () => review && state.side === "staged" && state.focused?.apply(),
    e: () => review && editSelection(),
    g: () => showGuide(true),
    "/": () => !review && $("#revset").focus(),
  };
  const action = actions[event.key];
  if (!action) return;
  event.preventDefault();
  action();
}

async function start() {
  await loadSettings();
  state.context = Number(setting("context", 3)) || 3;
  $("#context").value = String(state.context);
  state.layout = setting("layout", "unified") === "split" ? "split" : "unified";
  $("#layout").value = state.layout;
  // Wrapping changes as the stream narrows or widens; keep the reader's place.
  widthHandle($("#review .side"), "side-width", "Resize the review file list", keepPlace);
  bind();
  bindMenu();
  const params = new URLSearchParams(location.search);
  if (params.has("roots")) return enterRoots({ path: params.get("path") });
  state.rootPath = params.get("root");
  if (!state.rootPath) {
    const begin = await api("/api/start").catch((err) => (fail(err), null));
    if (!begin?.root) return enterRoots({ begin });
    state.rootPath = begin.root;
    history.replaceState(null, "", rootUrl(begin.root) + location.hash);
  }
  document.body.dataset.mode = "review";
  try {
    await loadRoot();
  } catch (err) {
    return enterRoots({ path: state.rootPath, notice: `${state.rootPath} cannot be reviewed: ${err.message}` });
  }
  readHash();
  if (!allWorkspaces().some((ws) => ws.path === state.ws)) state.ws = allWorkspaces()[0]?.path || null;
  renderWorkspacePicker();
  restoreDrafts().catch(fail);
  setView(state.view);
  // The header shows the git branch on every view.
  if (state.view !== "review") loadStatus().catch(fail);
  if (setting("guide-open", false)) showGuide(true, false);
}

start();
