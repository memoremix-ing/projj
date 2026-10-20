// Sorting the system's voices for the picker, and choosing one when the
// reader has not. Works on anything shaped like SpeechSynthesisVoice:
// `{ name, lang, voiceURI, localService, default }`.

/** Quality tiers, best first, with the names the picker shows for them. */
export const TIERS = [
  ["premium", "Premium"],
  ["enhanced", "Enhanced"],
  ["standard", "Standard"],
  ["basic", "Basic"],
  ["novelty", "Novelty"],
];

const RANK = Object.fromEntries(TIERS.map(([tier], i) => [tier, i]));

// macOS ships these for fun; nobody wants to be read a page by Bubbles.
const NOVELTY = new Set([
  "Albert", "Bad News", "Bahh", "Bells", "Boing", "Bubbles", "Cellos", "Deranged", "Good News",
  "Hysterical", "Jester", "Organ", "Pipe Organ", "Superstar", "Trinoids", "Whisper", "Wobble", "Zarvox",
]);

// Older, robotic voices: macOS's Eloquence voices and its oldest MacinTalk ones.
const BASIC = new Set([
  "Eddy", "Flo", "Grandma", "Grandpa", "Reed", "Rocko", "Sandy", "Shelley", "Fred", "Junior", "Kathy", "Ralph",
]);

/**
 * How good a voice can be expected to sound. macOS marks its downloadable
 * voices "(Premium)" and "(Enhanced)" in their names; Edge names its online
 * neural voices "… Online (Natural)".
 */
export function tier(voice) {
  const name = voice.name;
  if (/\(Premium\)/i.test(name) || /\(Natural\)/i.test(name)) return "premium";
  if (/\(Enhanced\)/i.test(name)) return "enhanced";
  const base = name.replace(/\s*\(.*$/, "");
  if (NOVELTY.has(base)) return "novelty";
  if (BASIC.has(base)) return "basic";
  return "standard";
}

/** `"en-US"` → `"en"`. Engines write both `en-US` and `en_US`. */
export function language(lang) {
  return String(lang || "").split(/[-_]/)[0].toLowerCase();
}

function region(lang) {
  return String(lang || "").split(/[-_]/)[1]?.toUpperCase() || "";
}

function displayNames(locale, type) {
  try {
    return new Intl.DisplayNames(locale ? [locale] : undefined, { type, fallback: "code" });
  } catch {
    return { of: (code) => code };
  }
}

/** The languages there are voices for, as `[{ code, label }]` sorted by label, labels in `locale`. */
export function languages(voices, locale) {
  const names = displayNames(locale, "language");
  const codes = [...new Set(voices.map((voice) => language(voice.lang)).filter(Boolean))];
  return codes.map((code) => ({ code, label: names.of(code) })).sort((a, b) => a.label.localeCompare(b.label, locale));
}

/**
 * What the picker shows for a voice. Some engines put the language in the
 * name, as in "Eddy (English (United States))"; that is cut, and the region
 * added after a dash, so that "Daniel — United Kingdom" stands apart from
 * voices of the same name elsewhere.
 */
export function label(voice, locale) {
  const name = voice.name.replace(/\s*\([^()]*\([^()]*\)\)\s*$/, "");
  const code = region(voice.lang);
  return code ? `${name} — ${displayNames(locale, "region").of(code)}` : name;
}

/**
 * Orders voices best first: by tier, then those of the wanted region (from
 * `lang`, as in `en-US`), then the system's default, then on-device ones
 * (which keep working offline and send nothing anywhere), then by name.
 */
export function rank(voices, lang) {
  const wanted = region(lang);
  return [...voices].sort((a, b) =>
    RANK[tier(a)] - RANK[tier(b)] ||
    Number(region(b.lang) === wanted) - Number(region(a.lang) === wanted) ||
    Number(Boolean(b.default)) - Number(Boolean(a.default)) ||
    Number(Boolean(b.localService)) - Number(Boolean(a.localService)) ||
    a.name.localeCompare(b.name),
  );
}

/** The voices for `lang`'s language, ranked and grouped by tier: `[{ tier, label, voices }]`, empty groups left out. */
export function groups(voices, lang) {
  const ranked = rank(voices.filter((voice) => language(voice.lang) === language(lang)), lang);
  return TIERS.map(([tier_, name]) => ({ tier: tier_, label: name, voices: ranked.filter((voice) => tier(voice) === tier_) }))
    .filter((group) => group.voices.length);
}

/**
 * The voice to read `lang` with: the one chosen before (`voiceURI`), if the
 * system still has it, else the best for the language, else the system's
 * default. Novelty voices are never chosen for the reader.
 */
export function choose(voices, lang, voiceURI) {
  const chosen = voiceURI && voices.find((voice) => voice.voiceURI === voiceURI);
  if (chosen) return chosen;
  const best = rank(voices.filter((voice) => language(voice.lang) === language(lang) && tier(voice) !== "novelty"), lang)[0];
  return best || voices.find((voice) => voice.default) || voices[0] || null;
}

/** Whether a better voice than any there is could be installed for `lang`: none is Premium or Enhanced. */
export function couldBeBetter(voices, lang) {
  return !voices.some((voice) => language(voice.lang) === language(lang) && RANK[tier(voice)] <= RANK.enhanced);
}
