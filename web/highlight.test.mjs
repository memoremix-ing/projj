// Run with `node --test web/highlight.test.mjs`. Needs nothing but Node.

import assert from "node:assert/strict";
import { test } from "node:test";

import { grammarFor, tokenize } from "./highlight.js";

/** Each line's tokens as "class:text", which reads better than offsets. */
function tokens(path, content) {
  const lines = content.split("\n");
  return tokenize(grammarFor(path), content).map((spans, i) => spans.map(([start, end, kind]) => `${kind}:${lines[i].slice(start, end)}`));
}

test("chooses a grammar by file name", () => {
  for (const path of ["src/main.rs", "web/app.js", "a/b/Makefile", "Dockerfile.dev", ".env", "x.TSX", "Cargo.lock", "notes.md"]) {
    assert.ok(grammarFor(path), path);
  }
  for (const path of ["LICENSE", "photo.png", "c", "yarn.lock"]) assert.equal(grammarFor(path), null, path);
});

test("rust: keywords, names, lifetimes, attributes and macros", () => {
  assert.deepEqual(tokens("a.rs", "#[derive(Debug)]\npub fn parse<'a>(input: &'a str) -> Option<u8> {\n    let c = 'x'; println!(\"{c}\\n\");\n    MAX_LEN != 0x1f\n}"), [
    ["a:#[derive(Debug)]"],
    ["k:pub", "k:fn", "f:parse", "a:'a", "a:'a", "t:str", "t:Option", "t:u8"],
    ["k:let", "s:'x'", "f:println", 's:"{c}', "e:\\n", 's:"'],
    ["n:MAX_LEN", "n:0x1f"],
    [],
  ]);
});

test("rust: block comments nest and raw strings run over lines", () => {
  assert.deepEqual(tokens("a.rs", "a /* one /* two */\nstill */ b\nlet s = r#\"say \"hi\"\n\\n\"# + 1;"), [
    ["c:/* one /* two */"],
    ["c:still */"],
    ["k:let", 's:r#"say "hi"'],
    ['s:\\n"#', "n:1"],
  ]);
});

test("javascript: regular expressions, templates and JSX", () => {
  assert.deepEqual(tokens("a.js", "const re = /a\\/[/]b/g, half = total / 2 / n;\nreturn `line ${one}\ntwo` + <b>bold</b>;"), [
    ["k:const", "s:/a\\/[/]b/g", "n:2"],
    ["k:return", "s:`line ${one}"],
    ["s:two`"],
  ]);
  // A quote in JSX text cannot swallow the lines after it.
  assert.deepEqual(tokens("a.tsx", "<p>Don't</p>\nconst x: string = 1;")[1], ["k:const", "t:string", "n:1"]);
  assert.deepEqual(tokens("a.js", "function (a) { new Thing(a); }"), [["k:function", "k:new", "t:Thing"]]);
});

test("python: triple quotes, prefixes and decorators", () => {
  assert.deepEqual(tokens("a.py", '@cache\ndef go(self):\n    """Doc\n    more"""\n    return rb"\\d" + f\'{x}\'  # done'), [
    ["a:@cache"],
    ["k:def", "f:go", "k:self"],
    ['s:"""Doc'],
    ['s:    more"""'],
    ["k:return", 's:rb"\\d"', "s:f'{x}'", "c:# done"],
  ]);
});

test("shell: comments need a space, quotes span lines", () => {
  assert.deepEqual(tokens("a.sh", "echo $# ${x#y} \"a\nb\" # end"), [
    ["a:$#", "a:${x#y}", 's:"a'],
    ['s:b"', "c:# end"],
  ]);
});

test("shell: here-documents are text until their end word", () => {
  assert.deepEqual(tokens("a.sh", "python3 - <<'EOF' # go\nit's = 'x\n  EOF\necho $((1 << n)) done"), [
    ["s:'EOF'", "c:# go"],
    ["s:it's = 'x"],
    [],
    ["n:1", "k:done"],
  ]);
});

test("data files: keys stand apart from values", () => {
  assert.deepEqual(tokens("a.json", '{ "name": "x\\n", "n": [1, true] }'), [['a:"name"', 's:"x', "e:\\n", 's:"', 'a:"n"', "n:1", "n:true"]]);
  assert.deepEqual(tokens("a.toml", '[package]\nname = "x" # c\nopt-level = 3'), [["t:[package]"], ["a:name", 's:"x"', "c:# c"], ["a:opt-level", "n:3"]]);
  assert.deepEqual(tokens("a.yaml", "- name: don't stop # c\n  on: true"), [["k:-", "a:name", "c:# c"], ["a:on", "n:true"]]);
});

test("css: properties, values and selectors", () => {
  assert.deepEqual(tokens("a.css", "a:hover, .x { /* c\n  c */ color: #fff; margin: -1.5px var(--gap) !important; }\n@media (min-width: 600px) {"), [
    ["a:a", "a:hover", "a:.x", "c:/* c"],
    ["c:  c */", "t:color", "n:#fff", "t:margin", "n:-1.5px", "f:var", "t:--gap", "k:!important"],
    ["k:@media", "a:min-width"],
  ]);
});

test("html: tags, attributes, comments and embedded script", () => {
  assert.deepEqual(tokens("a.html", '<!-- a\nb --><a href="x"\n  id=y>&amp;</a>\n<script type="module">\nconst a = `x\n${a}`; /* c */</script><b>'), [
    ["c:<!-- a"],
    ["c:b -->", "a:a", "a:href", 's:"x"'],
    ["a:id", "a:y", "e:&amp;", "a:a"],
    ["a:script", "a:type", 's:"module"'],
    ["k:const", "s:`x"],
    ["s:${a}`", "c:/* c */", "a:script", "a:b"],
  ]);
});

test("markdown: prose marks and fenced code in its own language", () => {
  assert.deepEqual(tokens("a.md", "# Title\n- **bold** and `code`, see [text](http://x)\n```rust\nlet a = \"x\n\";\n```\n> quote\n~~~\nplain\n~~~"), [
    ["h:# Title"],
    ["k:-", "b:**bold**", "r:`code`", "l:http://x"],
    ["r:```rust"],
    ["k:let", 's:"x'],
    ['s:"'],
    ["r:```"],
    ["c:> quote"],
    ["r:~~~"],
    ["r:plain"],
    ["r:~~~"],
  ]);
  // An unknown name is not looked up on the language table's prototype.
  assert.deepEqual(tokens("a.md", "```constructor\nx\n```")[1], ["r:x"]);
});

test("very long lines are left plain and do not disturb what is open", () => {
  const long = `x = "${"a".repeat(3000)}"`;
  assert.deepEqual(tokens("a.js", `/* open\n${long}\nstill */ let y;`), [["c:/* open"], [], ["c:still */", "k:let"]]);
});

test("tokens are in order, in range and never overlap", () => {
  const samples = {
    "a.rs": "fn main() { let s = b\"\\x00\"; /* c */ 'a: loop { break 'a; } } // é ✓",
    "a.ts": "export const f = async <T,>(x: T): Promise<T> => x ?? (await g(`${x}`)) /re/;",
    "a.css": ".a::before{content:\"{\";top:0}/*",
    "a.html": "<a b='c' d>&lt;</a><style>a{b:c}</style><script>1</script",
    "a.md": "* a _b_ **c** `d` ![e](f) <https://g>",
    "a.yaml": "a: &x [1, 'two', \"three\"] # four",
    "Makefile": "all: $(OBJ)\n\t$(CC) -o $@ $^ # link",
  };
  for (const [path, content] of Object.entries(samples)) {
    const lines = content.split("\n");
    tokenize(grammarFor(path), content).forEach((spans, i) => {
      let at = 0;
      for (const [start, end, kind] of spans) {
        assert.ok(start >= at && end > start && end <= lines[i].length, `${path}: ${JSON.stringify(spans)}`);
        assert.match(kind, /^[cksenftahbilr]$/);
        at = end;
      }
    });
  }
});
