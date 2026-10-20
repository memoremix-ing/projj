// read-aloud: an Edge-style Read Aloud for pages in webviews. No build step:
// these files are served as written.

import { ReadAloudControls } from "./controls.js";
import { ReadAloud } from "./reader.js";

export { ReadAloud, localStore, MAX_RATE, MIN_RATE } from "./reader.js";
export { ReadAloudControls } from "./controls.js";
export { WebSpeech } from "./speech.js";
export * as voices from "./voices.js";

/**
 * Sets up reading aloud for a page in one call: a ReadAloud (given the same
 * options), its controls, added to `container` (default `document.body`),
 * and the shortcut Edge uses, ⌘⇧U on a Mac and Ctrl+Shift+U elsewhere,
 * unless `shortcut: false`.
 *
 * Returns `{ reader, controls, destroy() }`.
 */
export function mountReadAloud({ container, shortcut = true, ...options } = {}) {
  const reader = new ReadAloud(options);
  const controls = new ReadAloudControls();
  controls.reader = reader;
  (container || document.body).append(controls);

  const mac = /mac/i.test(navigator.userAgentData?.platform || navigator.platform || "");
  const onKey = (event) => {
    if (event.key.toLowerCase() !== "u" || !event.shiftKey || event.altKey) return;
    if (mac ? !event.metaKey || event.ctrlKey : !event.ctrlKey || event.metaKey) return;
    event.preventDefault();
    reader.toggle();
  };
  if (shortcut) addEventListener("keydown", onKey);

  return {
    reader,
    controls,
    destroy() {
      removeEventListener("keydown", onKey);
      reader.destroy();
      controls.remove();
    },
  };
}
