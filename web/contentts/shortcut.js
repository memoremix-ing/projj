// The keyboard shortcut that starts, pauses and resumes reading: ⌘⇧ and a
// letter on a Mac, Ctrl+Shift and the letter elsewhere. No DOM, so a main
// process can use it too, as for a menu item's accelerator.

/**
 * K, as the letter least taken by browsers and the OS: on a Mac, none of
 * Chrome, Edge, Firefox, Safari or macOS's Services use ⌘⇧K. Elsewhere,
 * Ctrl+Shift+K is Edge's Duplicate tab and Firefox's Web Console, which a page
 * in an app's own webview never meets.
 */
export const DEFAULT_KEY = "K";

/**
 * The letter a `shortcut` option names, upper case: `true` (or nothing) for
 * the default, `false` or `null` for none, else a single letter A–Z. Throws on
 * anything else, so a mistyped shortcut fails where it is set rather than
 * never firing.
 */
export function shortcutKey(option = true) {
  if (option === true) return DEFAULT_KEY;
  if (option === false || option === null) return null;
  if (typeof option === "string" && /^[a-z]$/i.test(option)) return option.toUpperCase();
  throw new TypeError(`shortcut must be true, false or a letter A–Z, not ${JSON.stringify(option)}`);
}

/**
 * Whether a keydown is the shortcut for `key`. The letter is matched as the
 * keyboard layout types it, or by its place on the keyboard where the layout
 * types no Latin letter there (as in Cyrillic or Greek).
 */
export function matchesShortcut(event, key, mac) {
  if (!key || !event.shiftKey || event.altKey) return false;
  if (mac ? !event.metaKey || event.ctrlKey : !event.ctrlKey || event.metaKey) return false;
  return /^[a-z]$/i.test(event.key) ? event.key.toUpperCase() === key : event.code === `Key${key}`;
}

/** The shortcut as people read it: "⌘⇧K" on a Mac, "Ctrl+Shift+K" elsewhere. */
export function shortcutLabel(key, mac) {
  return mac ? `⌘⇧${key}` : `Ctrl+Shift+${key}`;
}

/** The shortcut as Electron and Electrobun menus take it, as in "CommandOrControl+Shift+K". */
export function accelerator(key) {
  return `CommandOrControl+Shift+${key}`;
}
