// Syntax colors for the editor, worked out in the page so that typing never
// waits on the server. No build step: this file is served as written.
//
// A grammar reads one line at a time: `grammar(text, state)` returns the
// line's tokens and the state the next line starts in. The state is a short
// string saying what is still open (a block comment, a multi-line string, a
// fenced code block), so after an edit only the lines from the change down to
// the bottom of the screen need reading again.
//
// Tokens are `[start, end, class]` with the same classes as the diff tokens
// the server sends (`.t-*` in style.css). The grammars are much simpler than
// the ones behind the diffs, so the two can differ in detail.

const words = (text) => new Set(text.split(/\s+/).filter(Boolean));

/** Keywords after which the next name is a function, or a type. */
const NAMES_FUNCTION = words("fn def func function fun sub");
const NAMES_TYPE = words("struct class enum trait interface type union impl extends implements new namespace module protocol extension");

/** Longer lines than this are left plain: they are not written by hand. */
const MAX_LINE = 2000;

const NUMBER = /(?:0[xXbBoO][\da-fA-F_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d[\d_]*)?)\w*/y;
const REGEX = /\/(?![/*])(?:\\.|\[(?:\\.|[^\]\\])*\]|[^/\\[])+\/[a-z]*/y;
const HEREDOC = /<<-?\s*(['"]?)([A-Za-z_]\w*)\1/;
const MACRO = /!(?!=)/y;
const CALL = /\s*\(/y;

/** Collects tokens in order, joining neighbors of one class. */
function collector() {
  const spans = [];
  const emit = (start, end, kind) => {
    if (end <= start) return;
    const last = spans[spans.length - 1];
    if (last && last[1] === start && last[2] === kind) last[1] = end;
    else spans.push([start, end, kind]);
  };
  return { spans, emit };
}

function at(pattern, text, index) {
  pattern.lastIndex = index;
  return pattern.exec(text);
}

/**
 * Reads the inside of a string from `index` to its `close`. Returns the index
 * after the close, or -1 if the line ended first.
 */
function readString(text, index, close, raw, emit, kind) {
  let start = index;
  while (index < text.length) {
    if (!raw && text[index] === "\\") {
      emit(start, index, kind);
      start = Math.min(text.length, index + 2);
      emit(index, start, "e");
      index = start;
    } else if (text.startsWith(close, index)) {
      emit(start, index + close.length, kind);
      return index + close.length;
    } else {
      index++;
    }
  }
  emit(start, index, kind);
  return -1;
}

const string = (open, close = open, options = {}) => ({ open: new RegExp(open.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "y"), close, ...options });
const QUOTES = [string('"'), string("'")];
const TRIPLE = [string('"""', '"""', { multi: true }), ...QUOTES];

/**
 * Builds a grammar for a programming or configuration language out of its
 * comment markers, string delimiters and word lists.
 *
 * `strings` are `{ open, close, raw, multi, kind }`, where `open` is a sticky
 * pattern and `close` and `raw` may be functions of its match. `rules` are
 * `[pattern, class, where]` tried before names and numbers; with `where` set
 * to "line" a rule only applies ahead of everything else on its line.
 *
 * States: "" between tokens, "C<depth>" inside a block comment,
 * "S<r|e><kind><close>" inside a string that runs over the line, and
 * "H<word>" inside a here-document that ends at `word`.
 */
function code({
  keywords = "",
  types = "",
  constants = "true false null",
  line = ["//"],
  block = ["/*", "*/"],
  nested = false,
  strings = QUOTES,
  rules = [],
  name = /[A-Za-z_]\w*/y,
  // Comments start only after a space, as in shells and YAML.
  spaced = false,
  ignoreCase = false,
  // Names starting with a capital are types, and all capitals constants.
  capitals = true,
  // A slash where a value is expected starts a regular expression.
  regex = false,
  // `name!` is a macro call.
  bang = false,
  // A quote opens a string even right after a letter, as it does in code
  // but not in prose such as an unquoted YAML value.
  quoteAfterWord = true,
  // `<<WORD` starts a here-document, as in shells.
  heredoc = false,
}) {
  const sets = { k: words(keywords), t: words(types), n: words(constants) };
  const classify = (word) => {
    const key = ignoreCase ? word.toLowerCase() : word;
    return sets.k.has(key) ? "k" : sets.n.has(key) ? "n" : sets.t.has(key) ? "t" : null;
  };

  const readComment = (text, index, depth) => {
    const [open, close] = block;
    while (index < text.length && depth) {
      if (text.startsWith(close, index)) {
        index += close.length;
        depth--;
      } else if (nested && text.startsWith(open, index)) {
        index += open.length;
        depth++;
      } else {
        index++;
      }
    }
    return [index, depth];
  };

  return (text, state) => {
    const { spans, emit } = collector();
    let i = 0;
    if (state[0] === "C") {
      const [end, depth] = readComment(text, 0, Number(state.slice(1)));
      emit(0, end, "c");
      if (depth) return [spans, `C${depth}`];
      i = end;
    } else if (state[0] === "S") {
      const end = readString(text, 0, state.slice(3), state[1] === "r", emit, state[2]);
      if (end < 0) return [spans, state];
      i = end;
    } else if (state[0] === "H") {
      if (text.trim() === state.slice(1)) return [spans, ""];
      emit(0, text.length, "s");
      return [spans, state];
    }
    // Whether the last token could end a value, and the last keyword seen.
    let value = false;
    let keyword = "";
    const first = text.search(/\S/);
    scan: while (i < text.length) {
      const ch = text[i];
      if (ch === " " || ch === "\t") {
        i++;
        continue;
      }
      const afterSpace = i === 0 || text[i - 1] === " " || text[i - 1] === "\t";
      if (block && text.startsWith(block[0], i)) {
        const [end, depth] = readComment(text, i + block[0].length, 1);
        emit(i, end, "c");
        if (depth) return [spans, `C${depth}`];
        i = end;
        continue;
      }
      if ((!spaced || afterSpace) && line.some((marker) => text.startsWith(marker, i))) {
        emit(i, text.length, "c");
        break;
      }
      for (const spec of strings) {
        const match = (quoteAfterWord || !/\w/.test(text[i - 1] || "")) && at(spec.open, text, i);
        if (!match) continue;
        const close = typeof spec.close === "function" ? spec.close(match) : spec.close;
        const raw = typeof spec.raw === "function" ? spec.raw(match) : Boolean(spec.raw);
        const kind = spec.kind || "s";
        const body = i + match[0].length;
        emit(i, body, kind);
        const end = readString(text, body, close, raw, emit, kind);
        if (end < 0) return [spans, spec.multi ? `S${raw ? "r" : "e"}${kind}${close}` : ""];
        i = end;
        value = true;
        continue scan;
      }
      for (const [pattern, kind, where] of rules) {
        if (where === "line" && i !== first) continue;
        const match = at(pattern, text, i);
        if (!match || !match[0]) continue;
        emit(i, i + match[0].length, kind);
        i += match[0].length;
        value = true;
        continue scan;
      }
      // In JSX, `</` closes a tag.
      if (regex && ch === "/" && !value && text[i - 1] !== "<") {
        const match = at(REGEX, text, i);
        if (match) {
          emit(i, i + match[0].length, "s");
          i += match[0].length;
          value = true;
          continue;
        }
      }
      let match = at(name, text, i);
      if (match) {
        const word = match[0];
        const end = i + word.length;
        let kind = classify(word);
        if (kind === "k") {
          keyword = word;
        } else if (!kind) {
          if (NAMES_FUNCTION.has(keyword)) kind = "f";
          else if (NAMES_TYPE.has(keyword)) kind = "t";
          else if ((bang && at(MACRO, text, end)) || at(CALL, text, end)) kind = "f";
          else if (capitals && /^[A-Z][A-Z0-9_]+$/.test(word)) kind = "n";
          else if (capitals && /^[A-Z]/.test(word)) kind = "t";
        }
        if (kind !== "k") keyword = "";
        if (kind) emit(i, end, kind);
        value = kind !== "k";
        i = end;
        continue;
      }
      match = /\d/.test(ch) && at(NUMBER, text, i);
      if (match) {
        emit(i, i + match[0].length, "n");
        i += match[0].length;
        value = true;
        continue;
      }
      value = ch === ")" || ch === "]" || ch === "}";
      keyword = "";
      i++;
    }
    const here = heredoc && HEREDOC.exec(text);
    // Not `<<` in a comment or a string, nor a shift inside `$(( ))`.
    if (here && !spans.some(([start, end]) => start <= here.index && here.index < end) && !/\(\([^)]*$/.test(text.slice(0, here.index))) {
      return [spans, `H${here[2]}`];
    }
    return [spans, ""];
  };
}

// ---- languages

const DOLLAR = [/\$(?:\w+|\{[^}]*\}|[#?@*!$-])/y, "a"];
const DECORATOR = [/@[A-Za-z_][\w.]*/y, "a"];
const C_LIKE = "break case catch class const continue default do else enum extends finally for if import new return static super switch this throw try while";

const javascript = (extra = "", types = "") =>
  code({
    keywords: `${C_LIKE} as async await debugger delete export from function in instanceof let of typeof var void with yield ${extra}`,
    types,
    constants: "true false null undefined NaN Infinity",
    strings: [...QUOTES, string("`", "`", { multi: true })],
    rules: [DECORATOR],
    name: /[A-Za-z_$][\w$]*/y,
    regex: true,
  });

const python = code({
  keywords:
    "and as assert async await break case class continue def del elif else except finally for from global if import in is lambda match nonlocal not or pass raise return self cls try while with yield",
  types: "int str float bool list dict set tuple bytes object",
  constants: "True False None",
  line: ["#"],
  block: null,
  strings: [
    { open: /[rRbBfFuU]{0,2}("""|''')/y, close: (m) => m[1], raw: (m) => /r/i.test(m[0]), multi: true },
    { open: /[rRbBfFuU]{0,2}(["'])/y, close: (m) => m[1], raw: (m) => /r/i.test(m[0]) },
  ],
  rules: [DECORATOR],
});

const c = (extra = "") =>
  code({
    keywords: `${C_LIKE} auto extern goto inline register restrict sizeof struct typedef union volatile ${extra}`,
    types: "bool char double float int long short signed unsigned void size_t ssize_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t",
    constants: "true false NULL nullptr",
    rules: [[/#\s*[a-z]+/y, "k", "line"]],
  });

const shell = code({
  keywords:
    "if then else elif fi for while until do done case esac in function select time return exit export local readonly declare set unset shift source alias",
  constants: "true false",
  line: ["#"],
  block: null,
  spaced: true,
  strings: [string('"', '"', { multi: true }), string("'", "'", { multi: true, raw: true })],
  rules: [DOLLAR],
  capitals: false,
  heredoc: true,
});

const KEY = (pattern, where) => [pattern, "a", where];

const config = (options) => code({ block: null, capitals: false, ...options });

const toml = config({
  line: ["#"],
  strings: [string('"""', '"""', { multi: true }), string("'''", "'''", { multi: true, raw: true }), string('"'), string("'", "'", { raw: true })],
  rules: [[/\[\[?[^\]#]*\]\]?/y, "t", "line"], KEY(/[\w.-]+(?=\s*=)/y)],
  constants: "true false inf nan",
});

const LANGUAGES = {
  __proto__: null,
  rust: code({
    keywords:
      "as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait type unsafe use where while",
    types: "bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64",
    constants: "true false",
    nested: true,
    strings: [
      { open: /b?r(#*)"/y, close: (m) => `"${m[1]}`, raw: true, multi: true },
      { open: /b?"/y, close: '"', multi: true },
    ],
    rules: [
      [/b?'(?:\\(?:x[\da-fA-F]{2}|u\{[\da-fA-F_]+\}|.)|[^\\'])'/y, "s"],
      [/'[A-Za-z_]\w*/y, "a"],
      [/#!?\[[^\]]*\]/y, "a"],
    ],
    bang: true,
  }),
  javascript: javascript(),
  typescript: javascript(
    "abstract declare implements interface is keyof namespace override private protected public readonly satisfies type",
    "any bigint boolean never number object string symbol unknown",
  ),
  python,
  go: code({
    keywords:
      "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var",
    types: "any bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr",
    constants: "true false nil iota",
    strings: [...QUOTES, string("`", "`", { multi: true, raw: true })],
  }),
  c: c(),
  cpp: c(
    "alignas alignof concept constexpr const_cast decltype dynamic_cast explicit export friend mutable namespace noexcept operator override private protected public reinterpret_cast requires static_assert static_cast template typeid typename using virtual final co_await co_return co_yield",
  ),
  java: code({
    keywords: `${C_LIKE} abstract assert final implements instanceof interface native package permits private protected public record sealed synchronized throws transient var volatile yield`,
    types: "boolean byte char double float int long short void",
    strings: TRIPLE,
    rules: [DECORATOR],
  }),
  kotlin: code({
    keywords: `${C_LIKE} abstract as by companion constructor data fun in init inline interface internal is lateinit object open operator override package private protected public sealed suspend typealias val var when`,
    strings: TRIPLE,
    rules: [DECORATOR],
  }),
  swift: code({
    keywords: `${C_LIKE} actor any as associatedtype async await defer deinit extension fallthrough fileprivate final func guard in init inout internal is lazy let mutating open operator override private protocol public repeat rethrows self Self some struct subscript throws typealias var weak where`,
    constants: "true false nil",
    nested: true,
    strings: TRIPLE,
    rules: [DECORATOR],
  }),
  csharp: code({
    keywords: `${C_LIKE} abstract as async await base checked delegate event explicit extern fixed foreach goto implicit in interface internal is lock namespace operator out override params private protected public readonly record ref sealed sizeof stackalloc struct typeof unchecked unsafe using var virtual volatile`,
    types: "bool byte char decimal double float int long object sbyte short string uint ulong ushort void",
    rules: [[/#\s*[a-z]+/y, "k", "line"]],
  }),
  ruby: code({
    keywords:
      "alias and begin break case class def do else elsif end ensure for if in module next not or redo require rescue retry return self super then undef unless until when while yield",
    constants: "true false nil",
    line: ["#"],
    block: null,
    rules: [[/(?<!:):[A-Za-z_]\w*/y, "n"], [/@@?[A-Za-z_]\w*/y, "a"]],
  }),
  php: code({
    keywords: `${C_LIKE} abstract and array as callable clone declare echo elseif empty final fn foreach function global implements include include_once instanceof interface isset list match namespace or print private protected public readonly require require_once trait unset use var yield`,
    line: ["//", "#"],
    rules: [DOLLAR],
  }),
  shell,
  sql: code({
    keywords:
      "add all alter and as asc begin between by case check column commit conflict constraint create cross default delete desc distinct do drop else end exists foreign from full group having if in index inner insert into is join key left like limit not nothing offset on or order outer pragma primary references returning right rollback select set table then transaction union unique update values view when where with",
    types: "bigint blob boolean char date int integer numeric real text timestamp varchar",
    line: ["--"],
    strings: [string("'", "'", { multi: true, raw: true }), string('"', '"', { raw: true })],
    ignoreCase: true,
    capitals: false,
  }),
  lua: code({
    keywords: "and break do else elseif end for function goto if in local not or repeat return then until while",
    constants: "true false nil",
    line: ["--"],
    block: ["--[[", "]]"],
    strings: [...QUOTES, string("[[", "]]", { multi: true, raw: true })],
    capitals: false,
  }),
  zig: code({
    keywords:
      "and async await break catch comptime const continue defer else enum errdefer error export extern fn for if inline or orelse pub return struct switch test try union unreachable usingnamespace var while",
    types: "bool f32 f64 i8 i16 i32 i64 isize u8 u16 u32 u64 usize void type anytype",
    constants: "true false null undefined",
    block: null,
  }),
  json: config({
    strings: [{ open: /"(?=(?:[^"\\]|\\.)*"\s*:)/y, close: '"', kind: "a" }, string('"')],
  }),
  toml,
  ini: config({
    line: ["#", ";"],
    rules: [[/\[[^\]]*\]/y, "t", "line"], KEY(/[\w.-]+(?=\s*[=:])/y, "line")],
  }),
  yaml: config({
    line: ["#"],
    spaced: true,
    // Past the dashes of a list, a key is still the first thing on its line.
    rules: [[/-(?=\s)/y, "k", "line"], KEY(/(?<=^[\s-]*)[\w.\/@$-][\w .\/@$-]*(?=:(?:\s|$))/y), [/[&*][\w-]+/y, "n"]],
    constants: "true false null yes no on off",
    quoteAfterWord: false,
  }),
  makefile: config({
    keywords: "ifeq ifneq ifdef ifndef else endif include define endef export override",
    line: ["#"],
    rules: [[/\$(?:\([^)]*\)|\{[^}]*\}|[@<^?*%+])/y, "a"], [/[\w.%\/-]+(?=\s*:(?!=))/y, "f", "line"]],
  }),
  dockerfile: config({
    keywords: "FROM RUN CMD LABEL EXPOSE ENV ADD COPY ENTRYPOINT VOLUME USER WORKDIR ARG ONBUILD STOPSIGNAL HEALTHCHECK SHELL AS",
    line: ["#"],
    rules: [DOLLAR],
  }),
};

// ---- stylesheets

const CSS_VALUE = code({
  constants: "",
  line: [],
  rules: [
    [/#[\da-fA-F]{3,8}\b/y, "n"],
    [/!important\b/y, "k"],
    [/-?(?:\d+\.?\d*|\.\d+)(?:%|[a-z]+)?/y, "n"],
    [/--[\w-]+/y, "t"],
  ],
  name: /-?[A-Za-z_][\w-]*/y,
  capitals: false,
});

/** Stylesheets: selectors and declarations are told apart a line at a time. */
function stylesheet(lineComments) {
  const VALUE = /\s*:(?!:)([^;{}]*)(?:[;}]|$)/y;
  const ITEM = /\/\*|\/\/|"(?:[^"\\]|\\.)*"?|'(?:[^'\\]|\\.)*'?|@[\w-]+|(?<![\w-])[.#]?-?[A-Za-z_][\w-]*|[{};]/g;
  return (text, state) => {
    const { spans, emit } = collector();
    let i = 0;
    const comment = (from) => {
      const end = text.indexOf("*/", from);
      emit(i, end < 0 ? text.length : end + 2, "c");
      i = end < 0 ? text.length : end + 2;
      return end >= 0;
    };
    if (state === "C" && !comment(0)) return [spans, "C"];
    while (i < text.length) {
      ITEM.lastIndex = i;
      const match = ITEM.exec(text);
      if (!match) break;
      const [item] = match;
      const start = match.index;
      i = start + item.length;
      if (item === "/*") {
        i = start;
        if (!comment(start + 2)) return [spans, "C"];
      } else if (item === "//") {
        if (!lineComments) continue;
        emit(start, text.length, "c");
        break;
      } else if (item[0] === '"' || item[0] === "'") {
        emit(start, i, "s");
      } else if (item[0] === "@") {
        emit(start, i, "k");
      } else if (item.length > 1 || /\w/.test(item)) {
        // A name is a property when a value follows it up to the end of the
        // declaration; otherwise it is part of a selector.
        const rest = at(VALUE, text, i);
        if (!rest || /^[.#]/.test(item) || /,\s*$/.test(rest[1])) {
          emit(start, i, "a");
          continue;
        }
        emit(start, i, "t");
        const from = i + rest[0].indexOf(":") + 1;
        const [values] = CSS_VALUE(rest[1], "");
        for (const [a, b, kind] of values) emit(from + a, from + b, kind);
        i = from + rest[1].length;
      }
    }
    return [spans, ""];
  };
}

LANGUAGES.css = stylesheet(false);
LANGUAGES.scss = stylesheet(true);

// ---- markup

/**
 * HTML and XML. States: "" in text, "C" in a comment, "T<name>" inside the
 * tag `name`, and "E<language>:<state>" inside a script or style element,
 * which is read with that language's grammar.
 */
function markup(text, state) {
  const { spans, emit } = collector();
  let i = 0;
  const EMBEDDED = { script: "javascript", style: "css" };
  const TEXT = /<!--|<\/?[A-Za-z][\w:.-]*|<![A-Za-z]+|&[#\w]+;/g;
  const INSIDE = /\/?>|"[^"]*"?|'[^']*'?|[^\s"'<>\/=]+/g;
  while (i < text.length) {
    if (state === "C") {
      const end = text.indexOf("-->", i);
      emit(i, end < 0 ? text.length : end + 3, "c");
      if (end < 0) return [spans, "C"];
      i = end + 3;
      state = "";
    } else if (state[0] === "E") {
      const cut = state.indexOf(":");
      const language = state.slice(1, cut);
      const tag = Object.keys(EMBEDDED).find((name) => EMBEDDED[name] === language);
      // As in a browser, the closing tag ends the element wherever it is.
      const close = text.toLowerCase().indexOf(`</${tag}`, i);
      const end = close < 0 ? text.length : close;
      const [inner, next] = LANGUAGES[language](text.slice(i, end), state.slice(cut + 1));
      for (const [a, b, kind] of inner) emit(i + a, i + b, kind);
      if (close < 0) return [spans, `E${language}:${next}`];
      i = end;
      state = "";
    } else if (state[0] === "T") {
      INSIDE.lastIndex = i;
      const match = INSIDE.exec(text);
      if (!match) return [spans, state];
      const [item] = match;
      i = match.index + item.length;
      if (item.endsWith(">")) {
        const language = item === ">" && EMBEDDED[state.slice(1).toLowerCase()];
        state = language ? `E${language}:` : "";
      } else {
        emit(match.index, i, item[0] === '"' || item[0] === "'" ? "s" : "a");
      }
    } else {
      TEXT.lastIndex = i;
      const match = TEXT.exec(text);
      if (!match) break;
      const [item] = match;
      i = match.index + item.length;
      if (item === "<!--") {
        i = match.index;
        state = "C";
      } else if (item[0] === "&") {
        emit(match.index, i, "e");
      } else {
        emit(match.index + (item[1] === "/" ? 2 : 1), i, "a");
        state = `T${item[1] === "/" || item[1] === "!" ? "" : item.slice(1)}`;
      }
    }
  }
  return [spans, state];
}

LANGUAGES.html = markup;

// ---- prose

const INLINE = /`[^`]*`|\*\*[^*]+\*\*|__[^_]+__|\*[^*\s][^*]*\*|\b_[^_\s][^_]*_\b|!?\[[^\]]*\]\([^)]*\)|<https?:[^>\s]+>/g;

/**
 * Markdown. States: "" in prose, and "F<fence>:<language>:<state>" inside a
 * fenced block, whose content is read with its language's grammar if known.
 */
function markdown(text, state) {
  const { spans, emit } = collector();
  if (state[0] === "F") {
    const [fence, language, ...rest] = state.slice(1).split(":");
    const closing = /^\s*(`{3,}|~{3,})\s*$/.exec(text);
    if (closing && closing[1][0] === fence[0] && closing[1].length >= fence.length) {
      emit(0, text.length, "r");
      return [spans, ""];
    }
    const grammar = named(language);
    if (!grammar) {
      emit(0, text.length, "r");
      return [spans, state];
    }
    const [inner, next] = grammar(text, rest.join(":"));
    return [inner, `F${fence}:${language}:${next}`];
  }
  const fence = /^\s*(`{3,}|~{3,})\s*([\w+#.-]*)/.exec(text);
  if (fence) {
    emit(0, text.length, "r");
    return [spans, `F${fence[1]}:${fence[2]}:`];
  }
  if (/^#{1,6}(\s|$)/.test(text)) {
    emit(0, text.length, "h");
    return [spans, ""];
  }
  if (/^\s*>/.test(text)) {
    emit(0, text.length, "c");
    return [spans, ""];
  }
  const marker = /^\s*(?:[-*+]|\d+[.)])(?=\s)/.exec(text);
  if (marker) emit(marker[0].length - marker[0].trimStart().length, marker[0].length, "k");
  INLINE.lastIndex = marker ? marker[0].length : 0;
  for (let match; (match = INLINE.exec(text)); ) {
    const [item] = match;
    const end = match.index + item.length;
    if (item[0] === "`") {
      emit(match.index, end, "r");
    } else if (item.startsWith("**") || item.startsWith("__")) {
      emit(match.index, end, "b");
    } else if (item[0] === "*" || item[0] === "_") {
      emit(match.index, end, "i");
    } else if (item[0] === "<") {
      emit(match.index, end, "l");
    } else {
      // The address of a link; its text stays as prose.
      emit(match.index + item.indexOf("](") + 2, end - 1, "l");
    }
  }
  return [spans, ""];
}

LANGUAGES.markdown = markdown;

// ---- choosing a grammar

const EXTENSIONS = {
  rust: "rs",
  javascript: "js mjs cjs jsx",
  typescript: "ts mts cts tsx",
  python: "py pyi",
  go: "go",
  c: "c h",
  cpp: "cc cpp cxx hpp hh hxx mm m",
  java: "java",
  kotlin: "kt kts",
  swift: "swift",
  csharp: "cs",
  ruby: "rb rake gemspec",
  php: "php",
  shell: "sh bash zsh fish env",
  sql: "sql",
  lua: "lua",
  zig: "zig",
  json: "json jsonc json5 webmanifest",
  toml: "toml",
  ini: "ini cfg conf properties editorconfig gitconfig",
  yaml: "yaml yml",
  makefile: "mk",
  css: "css",
  scss: "scss less",
  html: "html htm xml svg vue xhtml plist",
  markdown: "md markdown mdx",
};
const FILE_NAMES = {
  makefile: "makefile gnumakefile",
  dockerfile: "dockerfile containerfile",
  ruby: "gemfile rakefile",
  shell: ".bashrc .zshrc .profile .bash_profile .envrc",
  ini: ".gitconfig .editorconfig .npmrc",
  toml: "cargo.lock",
  json: ".prettierrc .eslintrc .babelrc",
};
/** Other names a fenced code block may give its language. */
const ALIASES = { __proto__: null, sh: "shell", bash: "shell", zsh: "shell", console: "shell", golang: "go", "c++": "cpp", "c#": "csharp", yml: "yaml", jsx: "javascript", tsx: "typescript" };

const byExtension = new Map();
for (const [language, list] of Object.entries(EXTENSIONS)) for (const extension of words(list)) byExtension.set(extension, language);
const byFileName = new Map();
for (const [language, list] of Object.entries(FILE_NAMES)) for (const name of words(list)) byFileName.set(name, language);

function named(name) {
  const key = name.toLowerCase();
  return LANGUAGES[ALIASES[key] || key] || LANGUAGES[byExtension.get(key)] || null;
}

/** The grammar for a file, chosen by its name, or null if none fits. */
export function grammarFor(path) {
  const file = path.slice(path.lastIndexOf("/") + 1).toLowerCase();
  const dot = file.lastIndexOf(".");
  const language = byFileName.get(file) || byFileName.get(file.split(".")[0]) || (dot >= 0 && byExtension.get(file.slice(dot + 1)));
  const grammar = LANGUAGES[language];
  if (!grammar) return null;
  return (text, state) => (text.length > MAX_LINE ? [[], state] : grammar(text, state));
}

/** Tokens for every line of `content`; for tests and one-off use. */
export function tokenize(grammar, content) {
  let state = "";
  return content.split("\n").map((line) => {
    const [spans, next] = grammar(line, state);
    state = next;
    return spans;
  });
}
