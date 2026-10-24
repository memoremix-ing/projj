// contenTTS: reads pages in webviews aloud, heavily inspired by Read Aloud
// in Microsoft's Edge web browser. No build step: these files are served as
// written.

import { ContenTTSControls } from "./controls.js";
import { ContenTTS } from "./reader.js";
import { matchesShortcut, shortcutKey } from "./shortcut.js";

export { ContenTTS, localStore, MAX_RATE, MIN_RATE } from "./reader.js";
export { ContenTTSControls } from "./controls.js";
export { DEFAULT_KEY, accelerator, matchesShortcut, shortcutKey, shortcutLabel } from "./shortcut.js";
export { WebSpeech } from "./speech.js";
export * as voices from "./voices.js";

/**
 * Sets up reading aloud for a page in one call: a ContenTTS (given the same
 * options), its controls, added to `container` (default `document.body`),
 * and a shortcut that starts, pauses and resumes: ⌘⇧ and `shortcut` on a
 * Mac, Ctrl+Shift and `shortcut` elsewhere. `shortcut` is a letter, `true`
 * for K, or `false` for none (see shortcut.js).
 *
 * Returns `{ reader, controls, shortcut, destroy() }`, `shortcut` being the
 * letter in use, or `null`.
 */
export function mountContenTTS({ container, shortcut = true, ...options } = {}) {
  // Before anything is made, so that a bad shortcut leaves nothing behind.
  const key = shortcutKey(shortcut);
  const reader = new ContenTTS(options);
  const controls = new ContenTTSControls();
  controls.reader = reader;
  (container || document.body).append(controls);

  const mac = /mac/i.test(navigator.userAgentData?.platform || navigator.platform || "");
  const onKey = (event) => {
    if (!matchesShortcut(event, key, mac)) return;
    event.preventDefault();
    reader.toggle();
  };
  if (key) addEventListener("keydown", onKey);

  return {
    reader,
    controls,
    shortcut: key,
    destroy() {
      removeEventListener("keydown", onKey);
      reader.destroy();
      controls.remove();
    },
  };
}
