// Text helpers that need no DOM: splitting a block of text into sentences and
// words, and finding where an offset in the block falls among the pieces of
// text it was built from.

/**
 * The most a sentence hands the speech engine at once. Longer stretches (code,
 * lists without punctuation) are cut at a word boundary, so that pausing,
 * skipping and changing speed stay quick, and engines that give up on long
 * utterances never see one.
 */
export const MAX_SENTENCE = 300;

const speakable = /[\p{L}\p{N}]/u;
const space = /\s/;

// Browsers' sentence breaking (ICU's) ends a sentence after these when a
// capital follows, as in "Dr. Moore", which would put a pause in the name.
const TITLES = { en: /\b(?:Mr|Mrs|Ms|Mx|Dr|Prof|Rev|Hon|Sr|Jr|St|Mt|Gen|Col|Capt|Lt|Sgt|No|Nos|Fig|Vol|vs)\.\s*$/ };

/**
 * Splits `text` into sentences, as `[{ start, end }]` offsets into it, trimmed
 * of white space. Sentences with nothing to say (only punctuation or symbols)
 * are left out, and ones longer than `max` are cut.
 */
export function sentences(text, lang, max = MAX_SENTENCE) {
  const out = [];
  const title = TITLES[String(lang || "").split(/[-_]/)[0].toLowerCase()];
  let from = 0;
  for (const { index, segment } of new Intl.Segmenter(lang, { granularity: "sentence" }).segment(text)) {
    const end = index + segment.length;
    if (title?.test(segment) && end < text.length) continue;
    const start = from;
    from = end;
    for (const piece of cut(text, start, end, lang, max)) {
      const trimmed = trim(text, piece.start, piece.end);
      if (speakable.test(text.slice(trimmed.start, trimmed.end))) out.push(trimmed);
    }
  }
  return out;
}

function trim(text, start, end) {
  while (start < end && space.test(text[start])) start++;
  while (end > start && space.test(text[end - 1])) end--;
  return { start, end };
}

/** Cuts `text[start, end)` into pieces of at most `max`, after white space. A single longer word stays whole. */
function cut(text, start, end, lang, max) {
  if (end - start <= max) return [{ start, end }];
  const out = [];
  let from = start;
  let lastBreak = start;
  for (const { index, segment } of new Intl.Segmenter(lang, { granularity: "word" }).segment(text.slice(start, end))) {
    const segmentEnd = start + index + segment.length;
    if (segmentEnd - from > max && lastBreak > from) {
      out.push({ start: from, end: lastBreak });
      from = lastBreak;
    }
    if (space.test(segment)) lastBreak = segmentEnd;
  }
  out.push({ start: from, end });
  return out;
}

/** The words of `text`, as `[{ start, end }]`. */
export function words(text, lang) {
  const out = [];
  for (const { index, segment, isWordLike } of new Intl.Segmenter(lang, { granularity: "word" }).segment(text)) {
    if (isWordLike) out.push({ start: index, end: index + segment.length });
  }
  return out;
}

/**
 * The word at `index` in `text`, or the first after it, as `{ start, end }`;
 * `null` past the last word. Speech engines report where a word starts, and
 * not all of them say how long it is.
 */
export function wordAt(text, index, lang) {
  for (const word of words(text, lang)) if (word.end > index) return word;
  return null;
}

/**
 * Where offset `offset` of a block falls, as `{ node, offset }`. A block's
 * text is joined from pieces, `[{ node, start, length }]` in order, and some
 * pieces stand for a break between nodes rather than a node (`node: null`).
 * An offset in one of those moves to the next node when it starts a range
 * (`edge: "start"`) and to the previous one when it ends a range.
 */
export function pointAt(pieces, offset, edge = "start") {
  let low = 0;
  let high = pieces.length - 1;
  while (low < high) {
    const mid = (low + high + 1) >> 1;
    if (pieces[mid].start <= offset) low = mid;
    else high = mid - 1;
  }
  // An offset where one piece ends and the next starts belongs to the next
  // piece for a start and to the earlier one for an end, so that a range
  // never begins at the very end of a node or ends at the start of one.
  if (edge === "end" && low > 0 && pieces[low].start === offset) low--;
  let i = low;
  if (pieces[i].node === null) {
    const step = edge === "start" ? 1 : -1;
    while (pieces[i] && pieces[i].node === null) i += step;
    if (!pieces[i]) return null;
    return { node: pieces[i].node, offset: edge === "start" ? 0 : pieces[i].length };
  }
  const piece = pieces[i];
  return { node: piece.node, offset: Math.max(0, Math.min(piece.length, offset - piece.start)) };
}
