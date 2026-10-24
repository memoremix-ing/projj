// contenTTS: reads pages in webviews aloud, heavily inspired by Read Aloud
// in Microsoft's Edge web browser. No build step: these files are served as
// written.

import { ContenTTSControls } from "./controls.js";
import { ContenTTS } from "./reader.js";
import { DEFAULT_SELECTION_KEY, matchesSelectionShortcut, matchesShortcut, selectionShortcutLabel, shortcutKey, shortcutLabel } from "./shortcut.js";

export { ContenTTS, localStore, MAX_RATE, MIN_RATE } from "./reader.js";
export { ContenTTSControls } from "./controls.js";
export {
  DEFAULT_KEY,
  DEFAULT_SELECTION_KEY,
  accelerator,
  matchesSelectionShortcut,
  matchesShortcut,
  selectionAccelerator,
  selectionShortcutLabel,
  shortcutKey,
  shortcutLabel,
} from "./shortcut.js";
export { WebSpeech } from "./speech.js";
export * as voices from "./voices.js";

/**
 * Sets up reading aloud for a page in one call: a ContenTTS (given the same
 * options), its controls, added to `container` (default `document.body`),
 * and a shortcut that starts, pauses and resumes: ⌘⇧ and `shortcut` on a
 * Mac, Ctrl+Shift and `shortcut` elsewhere; and one that reads from the
 * selection: ⌘⌥ and `selectionShortcut` on a Mac, Ctrl+Alt and it elsewhere.
 * Each is a letter, `true` for K, or `false` for none (see shortcut.js).
 *
 * Returns `{ reader, controls, shortcut, selectionShortcut, destroy() }`,
 * with the letters in use, or `null`.
 */
export function mountContenTTS({ container, shortcut = true, selectionShortcut = true, ...options } = {}) {
  // Before anything is made, so that a bad shortcut leaves nothing behind.
  const key = shortcutKey(shortcut);
  const selectionKey = shortcutKey(selectionShortcut, DEFAULT_SELECTION_KEY);
  const reader = new ContenTTS(options);
  const controls = new ContenTTSControls();
  controls.reader = reader;
  (container || document.body).append(controls);

  const mac = /mac/i.test(navigator.userAgentData?.platform || navigator.platform || "");
  controls.shortcuts = {
    toggle: key && shortcutLabel(key, mac),
    selection: selectionKey && selectionShortcutLabel(selectionKey, mac),
  };
  const onKey = (event) => {
    if (matchesShortcut(event, key, mac)) {
      event.preventDefault();
      reader.toggle();
    } else if (matchesSelectionShortcut(event, selectionKey, mac) && reader.readSelection()) {
      // With nothing selected, the key is the page's or the browser's.
      event.preventDefault();
    }
  };
  if (key || selectionKey) addEventListener("keydown", onKey);

  return {
    reader,
    controls,
    shortcut: key,
    selectionShortcut: selectionKey,
    destroy() {
      removeEventListener("keydown", onKey);
      reader.destroy();
      controls.remove();
    },
  };
}
