// Reading a page aloud: which sentence is being read, highlighting it and its
// words as they are spoken, and keeping them on screen.

import { collect, intact, rangeOf, rangesOf } from "./collect.js";
import { WebSpeech } from "./speech.js";
import { choose, language } from "./voices.js";

export const MIN_RATE = 0.5;
export const MAX_RATE = 3;

/** How long after the reader scrolls the page by hand the page is left where they put it. */
const HOLD_AFTER_SCROLL = 4000;

const SENTENCE = "contentts-sentence";
const WORD = "contentts-word";

// Apps can recolor these with the custom properties.
const SHEET = `
::highlight(${SENTENCE}) { background-color: var(--contentts-sentence, rgb(255 210 70 / 0.22)); }
::highlight(${WORD}) { background-color: var(--contentts-word, rgb(255 200 40 / 0.9)); color: var(--contentts-word-text, #111); }
`;

const SCROLL_KEYS = new Set(["PageUp", "PageDown", "Home", "End", "ArrowUp", "ArrowDown", " "]);

/** Settings kept in `localStorage` under `key`. */
export function localStore(key) {
  return {
    load() {
      try {
        return JSON.parse(localStorage.getItem(key));
      } catch {
        return null;
      }
    },
    save(value) {
      try {
        localStorage.setItem(key, JSON.stringify(value));
      } catch {
        // Storage can be full or refused; the settings just do not stick.
      }
    },
  };
}

/** Stored settings are untrusted, as anything else on the origin could have written them. */
function readSettings(store) {
  const raw = store?.load?.();
  const rate = Number(raw?.rate);
  const voices = {};
  if (raw?.voices && typeof raw.voices === "object") {
    for (const [lang, uri] of Object.entries(raw.voices)) {
      if (/^[a-z]{2,3}$/.test(lang) && typeof uri === "string" && uri.length <= 500) voices[lang] = uri;
    }
  }
  return { rate: rate >= MIN_RATE && rate <= MAX_RATE ? rate : 1, voices };
}

const styled = new WeakSet();

function addStyles(document) {
  if (styled.has(document)) return;
  styled.add(document);
  try {
    // A constructed sheet is not inline style, so even a CSP without 'unsafe-inline' lets it apply.
    const sheet = new document.defaultView.CSSStyleSheet();
    sheet.replaceSync(SHEET);
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
  } catch {
    const style = document.createElement("style");
    style.textContent = SHEET;
    document.head.append(style);
  }
}

/**
 * Reads the text under `root` aloud.
 *
 * Options: `root` (default `document.body`); `lang`, the text's language
 * (default: the nearest `lang` attribute, else the browser's); `skip`, a
 * selector for more to leave unread; `paragraph`, to group text into
 * paragraphs other than by layout (see `collect`); `engine` (default `WebSpeech`);
 * `storage`, `{ load(), save(value) }` for the voice and speed, or `null` to
 * keep them for the page's life only; `follow`, whether to scroll to what is
 * being read (default true).
 *
 * Fires `change` whenever `state`, the sentence, the voice or the speed
 * changes, and `voices` when the system's voices do.
 */
export class ContenTTS extends EventTarget {
  /** "idle", "starting" (waiting on the engine), "speaking" or "paused". */
  state = "idle";
  /** Why reading last stopped short: "unavailable", "empty", "changed", or the engine's error code. */
  error = null;

  #root;
  #lang;
  #skip;
  #paragraph;
  #engine;
  #store;
  #follow;
  #settings;
  #voices = [];
  #sentences = [];
  #index = 0;
  #word = null;
  #turn = 0;
  #restart = false;
  #heldUntil = 0;
  #listeners = [];

  constructor({ root = document.body, lang, skip, paragraph, engine = new WebSpeech(), storage = localStore("contentts"), follow = true } = {}) {
    super();
    this.#root = root;
    this.#lang = lang || root.closest("[lang]")?.lang || navigator.language || "en";
    this.#skip = skip;
    this.#paragraph = paragraph;
    this.#engine = engine;
    this.#store = storage;
    this.#follow = follow;
    this.#settings = readSettings(storage);
    addStyles(root.ownerDocument);

    // Voices are loaded now, so that starting needs no wait: some engines
    // refuse to speak once the click that asked for it is long past.
    this.refreshVoices();
    this.#listen(engine, "voiceschanged", () => this.refreshVoices());
    const hold = () => (this.#heldUntil = performance.now() + HOLD_AFTER_SCROLL);
    const view = root.ownerDocument.defaultView;
    this.#listen(view, "wheel", hold);
    this.#listen(view, "touchmove", hold);
    this.#listen(view, "keydown", (event) => SCROLL_KEYS.has(event.key) && hold());
    this.#listen(view, "pagehide", () => this.stop());
  }

  #listen(target, type, listener) {
    if (typeof target?.addEventListener !== "function") return;
    target.addEventListener(type, listener, { capture: true, passive: true });
    this.#listeners.push(() => target.removeEventListener(type, listener, { capture: true }));
  }

  /** The language being read, as in "en-US". */
  get lang() {
    return this.#lang;
  }

  /** The system's voices, as last loaded. */
  get voices() {
    return this.#voices;
  }

  async refreshVoices() {
    this.#voices = await this.#engine.voices();
    this.dispatchEvent(new Event("voices"));
    this.#changed();
  }

  /** The voice reading, or that will read: the one chosen for this language, else the best there is. */
  get voice() {
    return choose(this.#voices, this.#lang, this.#settings.voices[language(this.#lang)]);
  }

  /** Reads with the voice whose `voiceURI` this is from now on, for this language. */
  setVoice(voiceURI) {
    this.#settings.voices[language(this.#lang)] = voiceURI;
    this.#save();
  }

  /** Says a few words in the voice whose `voiceURI` this is, when not reading, so it can be heard before it is chosen. */
  async preview(voiceURI) {
    const voice = this.#voices.find((candidate) => candidate.voiceURI === voiceURI);
    if (!voice || this.state !== "idle") return;
    const name = voice.name.replace(/\s*\(.*$/, "");
    const sample = language(voice.lang) === "en" ? `Hello! My name is ${name}.` : name;
    try {
      await this.#engine.speak(sample, { voice, rate: this.rate });
    } catch {
      // A sample that fails says nothing; reading with the voice will report it.
    }
  }

  get rate() {
    return this.#settings.rate;
  }

  set rate(value) {
    const rate = Number(value);
    if (!(rate >= MIN_RATE && rate <= MAX_RATE)) return;
    this.#settings.rate = rate;
    this.#save();
  }

  #save() {
    this.#store?.save?.(this.#settings);
    // Voice and speed are set per utterance, so the one under way starts again, from its word.
    if (this.state === "speaking" || this.state === "starting") this.#play(this.#index, this.#wordOffset());
    else if (this.state === "paused") this.#restart = true;
    this.#changed();
  }

  /** The text selected under the root, as a Range, or `null` if none is. */
  get selection() {
    const selection = this.#root.ownerDocument.getSelection();
    const range = selection?.rangeCount && !selection.isCollapsed ? selection.getRangeAt(0) : null;
    return range && this.#root.contains(range.startContainer) ? range : null;
  }

  /** Which sentence is being read, of how many: `{ index, count }`. */
  get position() {
    return { index: this.#index, count: this.#sentences.length };
  }

  /**
   * Starts reading: from `from` (a Range or Node) if given, else from the
   * selection if there is one under the root, else from the first sentence
   * on screen. Given `within` (an element, such as the pane last used), the
   * first sentence on screen is looked for there first. Starting while
   * reading moves reading there.
   */
  start({ from, within } = {}) {
    this.stop();
    this.error = null;
    if (!this.#engine.available) return this.#end("unavailable");
    this.#sentences = this.#gather();
    if (!this.#sentences.length) return this.#end("empty");
    this.#play(this.#locate(from, within));
  }

  pause() {
    if (this.state !== "speaking" && this.state !== "starting") return;
    this.#engine.pause();
    this.#set("paused");
  }

  resume() {
    if (this.state !== "paused") return;
    if (this.#restart) return void this.#play(this.#index, this.#wordOffset());
    this.#engine.resume();
    this.#set("speaking");
  }

  /** Starts, pauses or resumes, as a single play/pause button would. */
  toggle() {
    if (this.state === "idle") this.start();
    else if (this.state === "paused") this.resume();
    else this.pause();
  }

  next() {
    if (this.state === "idle") return;
    if (this.#index + 1 >= this.#sentences.length) return this.stop();
    this.#play(this.#index + 1);
  }

  previous() {
    if (this.state === "idle") return;
    this.#play(Math.max(0, this.#index - 1));
  }

  stop() {
    this.#turn++;
    this.#engine.cancel();
    this.#end(null);
  }

  /** Stops, and lets go of the page. */
  destroy() {
    this.stop();
    for (const remove of this.#listeners.splice(0)) remove();
  }

  #gather() {
    return collect(this.#root, { lang: this.#lang, skip: this.#skip, paragraph: this.#paragraph }).flatMap((block) =>
      block.sentences.map(({ start, end }) => ({ block, start, end })),
    );
  }

  #locate(from, within) {
    let point = null;
    if (from instanceof Range) point = from;
    else if (from instanceof Node) {
      point = from.ownerDocument.createRange();
      point.selectNodeContents(from);
    } else {
      point = this.selection;
    }
    if (point) {
      for (const [i, { block, start, end }] of this.#sentences.entries()) {
        // The first sentence that does not end before the point.
        if (rangeOf(block, start, end)?.comparePoint(point.startContainer, point.startOffset) <= 0) return i;
      }
      return 0;
    }
    if (within?.isConnected) {
      const found = this.#onScreen(within);
      if (found >= 0) return found;
    }
    return Math.max(0, this.#onScreen(null));
  }

  /** The first sentence on screen (within `area`, if given, and its box), else the first in `area`; -1 if none. */
  #onScreen(area) {
    const view = this.#root.ownerDocument.defaultView;
    const box = area?.getBoundingClientRect();
    const top = Math.max(0, box?.top ?? 0);
    const bottom = Math.min(view.innerHeight, box?.bottom ?? view.innerHeight);
    let first = -1;
    for (const [i, { block, start, end }] of this.#sentences.entries()) {
      const range = rangeOf(block, start, end);
      if (!range || (area && !area.contains(range.startContainer))) continue;
      if (first < 0) first = i;
      const rect = range.getBoundingClientRect();
      if ((rect.width || rect.height) && rect.bottom > top && rect.top < bottom) return i;
    }
    return first;
  }

  #wordOffset() {
    const sentence = this.#sentences[this.#index];
    return sentence && this.#word ? this.#word.start - sentence.start : 0;
  }

  /** Reads from sentence `index` on, starting `offset` characters into it. */
  async #play(index, offset = 0) {
    const turn = ++this.#turn;
    this.#restart = false;
    this.#engine.cancel();
    this.#set("starting");
    for (let i = index; i < this.#sentences.length; i++, offset = 0) {
      if (turn !== this.#turn) return;
      if (!intact(this.#sentences[i].block)) {
        i = this.#resync(i);
        if (i < 0) return this.#end("changed");
      }
      const sentence = this.#sentences[i];
      const from = sentence.start + offset;
      this.#index = i;
      this.#word = null;
      this.#mark(sentence, null);
      this.#changed();
      try {
        const result = await this.#engine.speak(sentence.block.text.slice(from, sentence.end), {
          voice: this.voice,
          rate: this.rate,
          lang: this.#lang,
          onStart: () => turn === this.#turn && this.state === "starting" && this.#set("speaking"),
          onWord: (start, end) => {
            if (turn !== this.#turn) return;
            this.#word = { start: from + start, end: from + end };
            this.#mark(sentence, this.#word);
          },
        });
        // Cancelled by something other than this reader, such as another page speaking.
        if (result === "cancel" && turn === this.#turn) return this.#end(null);
      } catch (error) {
        if (turn === this.#turn) this.#end(error.code || "failed");
        return;
      }
    }
    if (turn === this.#turn) this.#end(null);
  }

  /**
   * The page changed under the reader. Gathers its text again and finds the
   * sentence it was on, nearest where it was; -1 if that sentence is gone.
   */
  #resync(i) {
    const { block, start, end } = this.#sentences[i];
    const text = block.text.slice(start, end);
    this.#sentences = this.#gather();
    let found = -1;
    for (const [j, sentence] of this.#sentences.entries()) {
      if (sentence.block.text.slice(sentence.start, sentence.end) !== text) continue;
      if (found < 0 || Math.abs(j - i) < Math.abs(found - i)) found = j;
    }
    return found;
  }

  #mark(sentence, word) {
    if (typeof CSS === "undefined" || !CSS.highlights || typeof Highlight !== "function") return;
    if (!word) {
      CSS.highlights.delete(WORD);
      const ranges = rangesOf(sentence.block, sentence.start, sentence.end);
      if (!ranges.length) return void CSS.highlights.delete(SENTENCE);
      CSS.highlights.set(SENTENCE, new Highlight(...ranges));
      return this.#reveal(ranges[0]);
    }
    const ranges = rangesOf(sentence.block, word.start, word.end);
    if (!ranges.length) return void CSS.highlights.delete(WORD);
    const highlight = new Highlight(...ranges);
    highlight.priority = 1;
    CSS.highlights.set(WORD, highlight);
    this.#reveal(ranges[0]);
  }

  /** Scrolls what is being read to a third of the way down its scroller, if it is near an edge or out of sight. */
  #reveal(range) {
    if (!this.#follow || performance.now() < this.#heldUntil) return;
    const rect = range.getBoundingClientRect();
    if (!rect.width && !rect.height) return;
    const document = this.#root.ownerDocument;
    const view = document.defaultView;
    const scroller = scrollerOf(range.startContainer.parentElement);
    const box = scroller ? scroller.getBoundingClientRect() : { top: 0, bottom: view.innerHeight };
    const height = box.bottom - box.top;
    const margin = Math.min(64, height / 6);
    if (rect.top >= box.top + margin && rect.bottom <= box.bottom - margin) return;
    const behavior = view.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth";
    (scroller || document.scrollingElement || document.documentElement).scrollBy({ top: rect.top - (box.top + height / 3), behavior });
  }

  #set(state) {
    this.state = state;
    this.#changed();
  }

  #end(error) {
    this.error = error;
    this.#word = null;
    if (typeof CSS !== "undefined" && CSS.highlights) {
      CSS.highlights.delete(SENTENCE);
      CSS.highlights.delete(WORD);
    }
    this.#set("idle");
  }

  #changed() {
    this.dispatchEvent(new Event("change"));
  }
}

/** The nearest scrolling ancestor short of the page itself, or `null`. */
function scrollerOf(element) {
  const document = element?.ownerDocument;
  for (let el = element; el && el !== document.body && el !== document.documentElement; el = el.parentElement) {
    if (/(auto|scroll|overlay)/.test(getComputedStyle(el).overflowY) && el.scrollHeight > el.clientHeight) return el;
  }
  return null;
}
