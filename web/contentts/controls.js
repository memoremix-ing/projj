// <contentts-controls>: play and pause, skipping by sentence, reading from
// the selection, speed, and a voice picker, for a ContenTTS given as its
// `reader` property.
//
// It shows itself while reading, or always with `persistent`. It floats at
// the foot of the window, or with `placement="inline"` sits where the app
// puts it, as in a toolbar, without a frame of its own.

import { couldBeBetter, groups, label, language, languages } from "./voices.js";

const RATES = [0.75, 1, 1.25, 1.5, 1.75, 2, 2.5];

const MESSAGES = {
  unavailable: "Speech isn't available here.",
  empty: "There's nothing to read here.",
  changed: "The page changed, so reading stopped.",
  "not-allowed": "Press play to start reading.",
  timeout: "The voice didn't start. Try another.",
  "voice-unavailable": "That voice isn't available. Try another.",
};

const BETTER_VOICES = {
  mac: "Better voices are free from Apple. In System Settings, go to Accessibility → Spoken Content → System Voice → Manage Voices…, and download one marked Premium or Enhanced. If it doesn't appear here, restart the app.",
  windows: "More voices are free from Microsoft. In Settings, go to Time & language → Speech → Manage voices → Add voices. If a new voice doesn't appear here, restart the app.",
};

const STYLE = `
:host { position: fixed; z-index: 2147483000; inset: auto 0 16px 0; margin: 0 auto; width: max-content; max-width: calc(100vw - 24px); color-scheme: light dark; font: 13px/1.3 system-ui, sans-serif; }
:host([placement="inline"]) { position: static; margin: 0; max-width: none; font: inherit; }
:host([placement="inline"]) .bar { padding: 0; border: 0; background: none; box-shadow: none; flex-wrap: nowrap; }
:host([placement="inline"]) details { position: relative; flex-basis: auto; white-space: nowrap; }
:host([placement="inline"]) details p { position: absolute; right: 0; bottom: calc(100% + 8px); width: 30em; margin: 0; padding: 8px 10px; white-space: normal; background: Canvas; color: CanvasText; border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); border-radius: 8px; box-shadow: 0 6px 24px rgb(0 0 0 / 0.18); }
:host([hidden]) { display: none; }
.bar { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; padding: 6px 8px; border-radius: 12px; background: Canvas; color: CanvasText; border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); box-shadow: 0 6px 24px rgb(0 0 0 / 0.18); }
button { display: inline-grid; place-items: center; width: 30px; height: 30px; padding: 0; border: 0; border-radius: 8px; background: transparent; color: inherit; cursor: pointer; }
button:hover:not(:disabled) { background: color-mix(in srgb, CanvasText 10%, transparent); }
button:disabled { opacity: 0.35; cursor: default; }
/* Highlight stands in where AccentColor is unknown, as in CEF 147. */
button.play { background: Highlight; color: HighlightText; background: AccentColor; color: AccentColorText; }
svg { width: 18px; height: 18px; fill: currentColor; }
svg.stroke { fill: none; stroke: currentColor; stroke-width: 2; stroke-linecap: round; }
select { max-width: 19em; font: inherit; color: inherit; background: color-mix(in srgb, CanvasText 6%, Canvas); border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); border-radius: 6px; padding: 3px 4px; }
.status:empty { display: none; }
.status { padding: 0 4px; }
details { flex-basis: 100%; padding: 0 4px; }
details[hidden] { display: none; }
summary { cursor: pointer; color: LinkText; width: max-content; }
details p { margin: 4px 0 2px; max-width: 46em; }
.sep { width: 1px; align-self: stretch; margin: 4px 2px; background: color-mix(in srgb, CanvasText 18%, transparent); }
`;

const SVG = "http://www.w3.org/2000/svg";
const ICONS = {
  play: "M8 5v14l11-7z",
  pause: "M6 5h4v14H6zM14 5h4v14h-4z",
  previous: "M6 6h2v12H6zm3.5 6 8.5 6V6z",
  next: "M6 18l8.5-6L6 6zM16 6h2v12h-2z",
  selection: "M4 4h6v2H8v12h2v2H4v-2h2V6H4zm9 3v10l8-5z",
  close: "M6 6l12 12M18 6 6 18",
};

/** Creates an element. `props` keys starting with "on" are listeners. */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value == null || value === false) continue;
    if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "class") el.className = value;
    else el.setAttribute(key, value === true ? "" : value);
  }
  el.append(...children.flat().filter((child) => child != null && child !== false));
  return el;
}

function icon(name) {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  if (name === "close") svg.classList.add("stroke");
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", ICONS[name]);
  svg.append(path);
  return svg;
}

function platform() {
  const name = navigator.userAgentData?.platform || navigator.platform || "";
  if (/mac/i.test(name)) return "mac";
  if (/win/i.test(name)) return "windows";
  return null;
}

function rateLabel(rate) {
  return `${rate}×`;
}

export class ContenTTSControls extends HTMLElement {
  #reader = null;
  #parts;
  #listLanguage = null;
  #dismissed = false;
  #onChange = () => this.#render();
  #onVoices = () => this.#fillVoices();
  #onSelection = () => this.#renderSelection();
  #shortcuts = {};

  constructor() {
    super();
    const shadow = this.attachShadow({ mode: "open" });
    try {
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(STYLE);
      shadow.adoptedStyleSheets = [sheet];
    } catch {
      shadow.append(h("style", null, STYLE));
    }
    const button = (name, text, onclick) => h("button", { type: "button", class: name, title: text, "aria-label": text, onclick }, icon(name));
    const parts = {
      previous: button("previous", "Previous sentence", () => this.#reader?.previous()),
      play: button("play", "Start Speech Mode", () => this.#reader?.toggle()),
      next: button("next", "Next sentence", () => this.#reader?.next()),
      selection: button("selection", "Read from selection", () => this.#reader?.readSelection()),
      status: h("span", { class: "status", role: "status", "aria-live": "polite" }),
      rate: h("select", { title: "Speed", "aria-label": "Speed", onchange: (event) => this.#reader && (this.#reader.rate = event.target.value) }),
      language: h("select", { title: "Voices for", "aria-label": "Voices for", onchange: (event) => this.#pickLanguage(event.target.value) }),
      voice: h("select", { title: "Voice", "aria-label": "Voice", onchange: (event) => this.#pickVoice(event.target.value) }),
      hint: h("details", { hidden: true }, h("summary", null, "Get better voices"), h("p")),
      close: button("close", "Stop and close", () => this.#close()),
    };
    // Pressing it must not take the selection away, as some engines do when the focus moves.
    parts.selection.addEventListener("mousedown", (event) => event.preventDefault());
    this.#parts = parts;
    shadow.append(
      h("div", { class: "bar", role: "toolbar", "aria-label": "Speech Mode" },
        parts.previous, parts.play, parts.next, parts.selection, parts.status,
        h("span", { class: "sep" }),
        parts.rate, parts.language, parts.voice, parts.close, parts.hint),
    );
  }

  get reader() {
    return this.#reader;
  }

  set reader(reader) {
    this.#reader?.removeEventListener("change", this.#onChange);
    this.#reader?.removeEventListener("voices", this.#onVoices);
    this.#reader = reader;
    this.#listLanguage = null;
    reader?.addEventListener("change", this.#onChange);
    reader?.addEventListener("voices", this.#onVoices);
    this.#fillVoices();
  }

  /**
   * The shortcuts, as people read them, to show with the buttons they stand
   * for: `{ toggle, selection }`, as in `{ toggle: "⌘⇧K" }`.
   */
  get shortcuts() {
    return this.#shortcuts;
  }

  set shortcuts(value) {
    this.#shortcuts = { toggle: value?.toggle || null, selection: value?.selection || null };
    const selection = this.#shortcuts.selection;
    this.#parts.selection.title = selection ? `Read from selection (${selection})` : "Read from selection";
    this.#render();
  }

  connectedCallback() {
    document.addEventListener("selectionchange", this.#onSelection);
    this.#render();
  }

  disconnectedCallback() {
    document.removeEventListener("selectionchange", this.#onSelection);
  }

  #renderSelection() {
    this.#parts.selection.disabled = !this.#reader?.selection;
  }

  #close() {
    this.#reader?.stop();
    this.#dismissed = true;
    this.#render();
  }

  #pickLanguage(code) {
    this.#listLanguage = code;
    this.#fillVoices();
  }

  #pickVoice(voiceURI) {
    const reader = this.#reader;
    if (!reader || !voiceURI) return;
    reader.setVoice(voiceURI);
    reader.preview(voiceURI);
    this.#fillVoices();
  }

  /** The language whose voices are listed: the one picked in the list, else the voice's, else the reader's. */
  #listed() {
    const reader = this.#reader;
    const code = this.#listLanguage || language(reader.voice?.lang || reader.lang);
    // Rank by the reader's region when listing the reader's own language.
    return code === language(reader.lang) ? reader.lang : code;
  }

  #fillVoices() {
    const reader = this.#reader;
    const { language: languageSelect, voice: voiceSelect, hint } = this.#parts;
    if (!reader) return;
    const voices = reader.voices;
    const listed = this.#listed();
    const locale = reader.lang;

    languageSelect.replaceChildren(...languages(voices, locale).map(({ code, label: name }) => h("option", { value: code }, name)));
    languageSelect.value = language(listed);
    languageSelect.hidden = languageSelect.options.length < 2;

    const current = reader.voice;
    const found = groups(voices, listed);
    voiceSelect.replaceChildren(
      ...(found.some((group) => group.voices.includes(current)) ? [] : [h("option", { value: "", disabled: true }, voices.length ? "Choose a voice" : "No voices")]),
      ...found.map((group) => h("optgroup", { label: group.label }, group.voices.map((voice) => h("option", { value: voice.voiceURI }, label(voice, locale))))),
    );
    voiceSelect.value = current && found.some((group) => group.voices.includes(current)) ? current.voiceURI : "";

    const steps = BETTER_VOICES[platform()];
    hint.hidden = !steps || !voices.length || !couldBeBetter(voices, listed);
    hint.querySelector("p").textContent = steps || "";
    this.#render();
  }

  #render() {
    const reader = this.#reader;
    const parts = this.#parts;
    if (!parts) return;
    const state = reader?.state || "idle";
    if (state !== "idle") this.#dismissed = false;

    const speaking = state === "speaking" || state === "starting";
    const playLabel = speaking ? "Pause" : state === "paused" ? "Resume" : "Start Speech Mode";
    parts.play.replaceChildren(icon(speaking ? "pause" : "play"));
    parts.play.title = this.#shortcuts.toggle ? `${playLabel} (${this.#shortcuts.toggle})` : playLabel;
    parts.play.setAttribute("aria-label", playLabel);
    parts.previous.disabled = parts.next.disabled = state === "idle";
    this.#renderSelection();

    const error = state === "idle" ? reader?.error : null;
    parts.status.textContent = state === "starting" ? "Starting…" : error ? MESSAGES[error] || `Speech failed (${error}).` : "";

    const rate = reader?.rate ?? 1;
    const rates = RATES.includes(rate) ? RATES : [...RATES, rate].sort((a, b) => a - b);
    if ([...parts.rate.options].map((option) => option.value).join() !== rates.join()) {
      parts.rate.replaceChildren(...rates.map((value) => h("option", { value }, rateLabel(value))));
    }
    parts.rate.value = String(rate);

    const voice = reader?.voice;
    if (voice && [...parts.voice.options].some((option) => option.value === voice.voiceURI)) parts.voice.value = voice.voiceURI;

    this.hidden = !this.hasAttribute("persistent") && (this.#dismissed || (state === "idle" && !error));
  }
}

if (!customElements.get("contentts-controls")) customElements.define("contentts-controls", ContenTTSControls);
