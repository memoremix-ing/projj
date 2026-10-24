// The Web Speech API (`speechSynthesis`) behind a small interface, which any
// other engine can offer too:
//
//   voices()                    → Promise of voices
//   speak(text, options)        → Promise of "end" or "cancel"; rejects on failure
//   pause(), resume(), cancel()
//
// `options` is `{ voice, rate, lang, onStart(), onWord(start, end) }`, with
// word offsets into `text`.

import { wordAt } from "./text.js";

/** How long voices may take to arrive: Chromium lists them only after a `voiceschanged`. */
const VOICES_WAIT = 3000;

/** How long an utterance may take to start before it counts as failed; a cold voice took about 2 s in tests. */
const START_WAIT = 10_000;

export class WebSpeech extends EventTarget {
  #synth;
  #turn = 0;
  #finish = null;
  // Chromium can stop sending an utterance's events once nothing refers to it.
  #utterance = null;

  constructor(synth = globalThis.speechSynthesis) {
    super();
    this.#synth = synth;
    // Voices can arrive late, and change when the user installs one.
    synth?.addEventListener?.("voiceschanged", () => this.dispatchEvent(new Event("voiceschanged")));
  }

  get available() {
    return Boolean(this.#synth) && typeof SpeechSynthesisUtterance === "function";
  }

  voices() {
    if (!this.available) return Promise.resolve([]);
    const now = this.#synth.getVoices();
    if (now.length) return Promise.resolve(now);
    return new Promise((resolve) => {
      const done = () => {
        this.#synth.removeEventListener("voiceschanged", done);
        clearTimeout(timer);
        resolve(this.#synth.getVoices());
      };
      const timer = setTimeout(done, VOICES_WAIT);
      this.#synth.addEventListener("voiceschanged", done);
    });
  }

  speak(text, { voice, rate = 1, lang, onStart, onWord } = {}) {
    this.cancel();
    const turn = ++this.#turn;
    return new Promise((resolve, reject) => {
      const utterance = new SpeechSynthesisUtterance(text);
      if (voice) utterance.voice = voice;
      utterance.lang = voice?.lang || lang || "";
      utterance.rate = rate;
      const current = () => turn === this.#turn;
      const timer = setTimeout(() => current() && fail("timeout"), START_WAIT);
      const settle = (result) => {
        clearTimeout(timer);
        if (this.#finish === settle) this.#finish = null;
        resolve(result);
      };
      const fail = (error) => {
        clearTimeout(timer);
        if (this.#finish === settle) this.#finish = null;
        this.#synth.cancel();
        reject(Object.assign(new Error(`speech failed: ${error}`), { code: error }));
      };
      utterance.onstart = () => {
        if (!current()) return;
        clearTimeout(timer);
        onStart?.();
      };
      utterance.onboundary = (event) => {
        if (!current() || event.name !== "word") return;
        // Not every engine says how long the word is.
        const word = event.charLength
          ? { start: event.charIndex, end: event.charIndex + event.charLength }
          : wordAt(text, event.charIndex, utterance.lang);
        if (word) onWord?.(word.start, word.end);
      };
      utterance.onend = () => current() && settle("end");
      utterance.onerror = (event) => {
        if (!current()) return;
        // Cancelling, ours or another page's, ends an utterance with these.
        if (event.error === "interrupted" || event.error === "canceled") settle("cancel");
        else fail(event.error);
      };
      this.#finish = settle;
      this.#utterance = utterance;
      // A pause can outlive the utterance it paused, and hold the next one.
      if (this.#synth.paused) this.#synth.resume();
      this.#synth.speak(utterance);
    });
  }

  pause() {
    this.#synth?.pause();
  }

  resume() {
    this.#synth?.resume();
  }

  cancel() {
    this.#turn++;
    this.#utterance = null;
    const finish = this.#finish;
    this.#finish = null;
    if (this.#synth?.speaking || this.#synth?.pending) this.#synth.cancel();
    finish?.("cancel");
  }
}
