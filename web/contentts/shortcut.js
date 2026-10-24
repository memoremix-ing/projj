// The keyboard shortcuts: one that starts, pauses and resumes reading (⌘⇧ and
// a letter on a Mac, Ctrl+Shift and the letter elsewhere), and one that reads
// from the selection (⌘⌥ and a letter on a Mac, Ctrl+Alt and the letter
// elsewhere). No DOM, so a main process can use it too, as for a menu item's
// accelerator.

/**
 * K, as the letter least taken by browsers and the OS: on a Mac, none of
 * Chrome, Edge, Firefox, Safari or macOS's Services use ⌘⇧K. Elsewhere,
 * Ctrl+Shift+K is Edge's Duplicate tab and Firefox's Web Console, which a page
 * in an app's own webview never meets.
 */
export const DEFAULT_KEY = "K";

/**
 * K for reading from the selection too, on its other modifiers. ⌘⌥K is
 * Firefox's Web Console on a Mac and nothing in Chrome, Edge, Safari or
 * macOS. Ctrl+Alt+K takes nothing in Chrome, Edge or Firefox, but Windows
 * types with Ctrl+Alt as AltGr, and AltGr+K is "ł" on the Hungarian and
 * Croatian layouts: see `matchesSelectionShortcut`.
 */
export const DEFAULT_SELECTION_KEY = "K";

/**
 * The letter a `shortcut` or `selectionShortcut` option names, upper case:
 * `true` (or nothing) for `fallback`, the default, `false` or `null` for none,
 * else a single letter A–Z. Throws on anything else, so a mistyped shortcut
 * fails where it is set rather than never firing.
 */
export function shortcutKey(option = true, fallback = DEFAULT_KEY) {
  if (option === true) return fallback;
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

/**
 * Whether a keydown is the shortcut that reads from the selection, for `key`.
 * On a Mac, Option changes the character a key reports (⌘⌥K reports "˚"),
 * so a key that reports no letter is matched by its place on the keyboard;
 * with ⌘ held nothing is typed, so nothing is lost. Elsewhere Ctrl+Alt is also AltGr, which types
 * characters on many layouts, so only a key that reports the letter itself
 * counts: AltGr+K typing "ł" stays typing.
 */
export function matchesSelectionShortcut(event, key, mac) {
  if (!key || !event.altKey || event.shiftKey) return false;
  if (mac) return event.metaKey && !event.ctrlKey && (/^[a-z]$/i.test(event.key) ? event.key.toUpperCase() === key : event.code === `Key${key}`);
  return event.ctrlKey && !event.metaKey && /^[a-z]$/i.test(event.key) && event.key.toUpperCase() === key;
}

/** The shortcut as people read it: "⌘⇧K" on a Mac, "Ctrl+Shift+K" elsewhere. */
export function shortcutLabel(key, mac) {
  return mac ? `⌘⇧${key}` : `Ctrl+Shift+${key}`;
}

/** The selection shortcut as people read it: "⌘⌥K" on a Mac, "Ctrl+Alt+K" elsewhere. */
export function selectionShortcutLabel(key, mac) {
  return mac ? `⌘⌥${key}` : `Ctrl+Alt+${key}`;
}

/** The shortcut as Electron and Electrobun menus take it, as in "CommandOrControl+Shift+K". */
export function accelerator(key) {
  return `CommandOrControl+Shift+${key}`;
}

/**
 * The selection shortcut as a menu takes it, "CommandOrControl+Alt+K". Only
 * for macOS menus: a menu elsewhere would take AltGr+K from typing, where
 * the page's own check (`matchesSelectionShortcut`) leaves it be.
 */
export function selectionAccelerator(key) {
  return `CommandOrControl+Alt+${key}`;
}
