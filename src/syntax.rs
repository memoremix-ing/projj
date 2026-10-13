//! Syntax highlighting for diff lines.
//!
//! A whole file is tokenized at once, so that a hunk in the middle of a block
//! comment or a fenced code block is colored correctly, and each diff line
//! then picks up the tokens of the file line it shows. Tokens are reduced to a
//! handful of classes that the client styles; no colors are decided here.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use sha2::Digest as _;
use sha2::Sha256;
use syntect::parsing::ParseState;
use syntect::parsing::Scope;
use syntect::parsing::ScopeStack;
use syntect::parsing::SyntaxReference;
use syntect::parsing::SyntaxSet;

use crate::diff::Hunk;
use crate::diff::LineKind;

/// `[start, end, class]`, with offsets in UTF-16 units into the line.
pub type Span = (usize, usize, &'static str);

/// Larger files are shown without highlighting rather than slowly.
const MAX_BYTES: usize = 1024 * 1024;
/// Minified and generated files have lines the grammars cope badly with.
const MAX_LINE_BYTES: usize = 4000;
const CACHE_ENTRIES: usize = 64;

/// The tokens of every line of one version of a file.
pub struct Highlighted {
    /// Line text without its newline, and that line's spans.
    lines: Vec<(String, Vec<Span>)>,
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn find_syntax(path: &str, text: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, extension)| extension);
    // Grammars list bare file names such as `Makefile` among their extensions.
    set.find_syntax_by_extension(name)
        .or_else(|| extension.and_then(|extension| set.find_syntax_by_extension(extension)))
        .or_else(|| set.find_syntax_by_first_line(text.lines().next().unwrap_or_default()))
        .filter(|syntax| syntax.name != "Plain Text")
}

/// Tokenizes `content` as the kind of file `path` names. Returns `None` for
/// content that is not text, has no known grammar, or is too large.
pub fn highlight(path: &str, content: &[u8]) -> Option<Arc<Highlighted>> {
    if content.len() > MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let syntax = find_syntax(path, text)?;
    if text.lines().any(|line| line.len() > MAX_LINE_BYTES) {
        return None;
    }

    // The same version of a file is asked for again each time a hunk of it is
    // marked, so recent results are kept.
    static CACHE: Mutex<VecDeque<([u8; 32], Arc<Highlighted>)>> = Mutex::new(VecDeque::new());
    let mut hasher = Sha256::new();
    hasher.update(syntax.name.as_bytes());
    hasher.update([0]);
    hasher.update(content);
    let key: [u8; 32] = hasher.finalize().into();
    if let Some((_, found)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return Some(found.clone());
    }
    let highlighted = Arc::new(tokenize(syntax, text));
    let mut cache = CACHE.lock().unwrap();
    if cache.len() >= CACHE_ENTRIES {
        cache.pop_front();
    }
    cache.push_back((key, highlighted.clone()));
    Some(highlighted)
}

fn tokenize(syntax: &SyntaxReference, text: &str) -> Highlighted {
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut classes: HashMap<Scope, Class> = HashMap::new();
    let mut lines = Vec::new();
    let mut failed = false;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let mut spans: Vec<Span> = Vec::new();
        if !failed {
            // The grammars expect every line to end with a newline.
            let ops = if raw.len() == line.len() {
                state.parse_line(&format!("{line}\n"), set)
            } else {
                state.parse_line(raw, set)
            };
            match ops {
                Ok(ops) => {
                    let mut units = Utf16Offsets::new(line);
                    let mut from = 0;
                    let boundaries = ops
                        .iter()
                        .map(|(at, op)| ((*at).min(line.len()), Some(op)))
                        .chain([(line.len(), None)]);
                    for (at, op) in boundaries {
                        if at > from
                            && let Some(class) = class_of(&stack, &mut classes)
                        {
                            let (start, end) = (units.at(from), units.at(at));
                            match spans.last_mut() {
                                Some(last) if last.1 == start && last.2 == class => last.1 = end,
                                _ => spans.push((start, end, class)),
                            }
                        }
                        from = from.max(at);
                        if let Some(op) = op
                            && stack.apply(op).is_err()
                        {
                            failed = true;
                            break;
                        }
                    }
                }
                Err(_) => failed = true,
            }
        }
        lines.push((line.to_owned(), spans));
    }
    Highlighted { lines }
}

/// Converts ascending byte offsets within a line to UTF-16 offsets.
struct Utf16Offsets<'a> {
    line: &'a str,
    byte: usize,
    unit: usize,
}

impl<'a> Utf16Offsets<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            line,
            byte: 0,
            unit: 0,
        }
    }

    fn at(&mut self, byte: usize) -> usize {
        let mut byte = byte.min(self.line.len());
        while !self.line.is_char_boundary(byte) {
            byte -= 1;
        }
        if byte > self.byte {
            self.unit += self.line[self.byte..byte].encode_utf16().count();
            self.byte = byte;
        }
        self.unit
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Named(&'static str),
    /// Says nothing by itself; the enclosing scope decides.
    Transparent,
    /// Transparent, and marks the key of a key/value pair.
    MappingKey,
    /// Plain text, whatever encloses it.
    Plain,
}

/// The class of text under the given scopes.
fn class_of(stack: &ScopeStack, classes: &mut HashMap<Scope, Class>) -> Option<&'static str> {
    let mut found = None;
    let mut in_key = false;
    // Innermost scope first.
    for scope in stack.as_slice().iter().rev() {
        let class = *classes
            .entry(*scope)
            .or_insert_with(|| classify(&scope.build_string()));
        match class {
            Class::Named(name) if found.is_none() => found = Some(Some(name)),
            Class::Plain if found.is_none() => found = Some(None),
            Class::MappingKey => in_key = true,
            _ => {}
        }
    }
    match found.flatten() {
        // Object keys read better apart from string values.
        Some("s") if in_key => Some("a"),
        class => class,
    }
}

fn classify(scope: &str) -> Class {
    let is = |prefix: &str| {
        scope
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    let named = |pairs: &[(&str, &'static str)]| {
        pairs
            .iter()
            .find(|(prefix, _)| is(prefix))
            .map(|(_, class)| Class::Named(class))
    };
    // Code embedded in a string or in prose starts from a clean slate.
    if is("source") || is("text") || scope.contains("interpolation") || scope.contains("embedded") {
        return Class::Plain;
    }
    if is("meta.mapping.key") {
        return Class::MappingKey;
    }
    if is("punctuation") || is("meta") {
        return Class::Transparent;
    }
    if is("keyword.operator") {
        // Symbols stay plain; `and`, `in`, `as` read as keywords.
        return if scope.contains(".word") || scope.contains(".logical.python") {
            Class::Named("k")
        } else {
            Class::Plain
        };
    }
    named(&[
        ("comment", "c"),
        ("string", "s"),
        ("constant.character.escape", "e"),
        ("constant.other.placeholder", "e"),
        ("constant", "n"),
        ("keyword", "k"),
        ("storage", "k"),
        ("variable.language", "k"),
        ("entity.name.function", "f"),
        ("support.function", "f"),
        ("support.macro", "f"),
        ("variable.function", "f"),
        ("entity.name.tag", "a"),
        ("entity.other.attribute-name", "a"),
        ("entity.name.section", "h"),
        ("entity.name", "t"),
        ("entity.other.inherited-class", "t"),
        ("support.type", "t"),
        ("support.class", "t"),
        ("markup.heading", "h"),
        ("markup.bold", "b"),
        ("markup.italic", "i"),
        ("markup.underline", "l"),
        ("markup.raw", "r"),
        ("markup.quote", "c"),
    ])
    .unwrap_or(Class::Plain)
}

/// Attaches tokens to the lines of `hunks`. Removed lines are read from the
/// old version of the file and everything else from the new one. A line keeps
/// no tokens unless the file line it names has exactly the same text.
pub fn apply(hunks: &mut [Hunk], old: Option<&Highlighted>, new: Option<&Highlighted>) {
    for line in hunks.iter_mut().flat_map(|hunk| &mut hunk.lines) {
        let (side, number) = match line.kind {
            LineKind::Removed => (old, line.old),
            LineKind::Added | LineKind::Context => (new, line.new),
        };
        let found = side
            .zip(number)
            .and_then(|(side, number)| side.lines.get(number.checked_sub(1)?));
        if let Some((text, spans)) = found
            && *text == line.text
            && !spans.is_empty()
        {
            line.syn = Some(spans.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of each span of each line, paired with its class.
    fn tokens(path: &str, source: &str) -> Vec<Vec<(String, &'static str)>> {
        let highlighted = highlight(path, source.as_bytes()).expect("a known grammar");
        highlighted
            .lines
            .iter()
            .map(|(text, spans)| {
                let units: Vec<u16> = text.encode_utf16().collect();
                spans
                    .iter()
                    .map(|(start, end, class)| {
                        (String::from_utf16(&units[*start..*end]).unwrap(), *class)
                    })
                    .collect()
            })
            .collect()
    }

    fn has(line: &[(String, &'static str)], text: &str, class: &str) -> bool {
        line.iter().any(|(t, c)| t == text && *c == class)
    }

    #[test]
    fn classifies_rust_tokens() {
        let lines = tokens(
            "src/lib.rs",
            "// note\npub fn total(n: u32) -> u32 {\n    let s = \"h\u{e9}llo\\n\"; n + 42\n}\n",
        );
        assert!(has(&lines[0], "// note", "c"), "{lines:?}");
        assert!(has(&lines[1], "fn", "k"), "{lines:?}");
        assert!(has(&lines[1], "total", "f"), "{lines:?}");
        assert!(has(&lines[2], "\\n", "e"), "{lines:?}");
        assert!(has(&lines[2], "42", "n"), "{lines:?}");
        // The string is split around its escape; offsets survive the accent.
        assert!(has(&lines[2], "\"h\u{e9}llo", "s"), "{lines:?}");
        assert!(lines[3].is_empty());
    }

    #[test]
    fn carries_state_across_lines() {
        let lines = tokens(
            "a.ts",
            "/* open\n still const x = 1\n*/\nconst x: number = 1;\n",
        );
        assert_eq!(lines[1], [(" still const x = 1".to_owned(), "c")]);
        assert!(has(&lines[3], "const", "k"), "{lines:?}");
        assert!(has(&lines[3], "1", "n"), "{lines:?}");
    }

    #[test]
    fn knows_the_file_types_in_use() {
        let samples = [
            ("a.tsx", "const a = <b>hi</b>;\n"),
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("a.go", "package main\n"),
            ("a.json", "{\"a\": 1}\n"),
            ("a.md", "# Title\n"),
            ("a.py", "def f():\n    pass\n"),
            ("a.css", "a { color: red; }\n"),
            ("a.html", "<p>x</p>\n"),
            ("a.sh", "echo hi\n"),
            ("a.yaml", "a: 1\n"),
            ("a.js", "let a = 1;\n"),
            ("script", "#!/bin/bash\necho hi\n"),
        ];
        for (path, source) in samples {
            let lines = tokens(path, source);
            assert!(
                lines.iter().any(|line| !line.is_empty()),
                "{path}: {lines:?}"
            );
        }
        assert!(has(&tokens("a.md", "# Title\n")[0], "# Title", "h"));
        // Code in a fence is colored as its own language, not as the fence.
        let fenced = tokens("a.md", "```rust\nfn a() {}\n```\n");
        assert!(has(&fenced[1], "fn", "k"), "{fenced:?}");
        // Expressions inside a template string are not string-colored.
        let template = tokens("a.ts", "const s = `a ${b + 1} c`;\n");
        assert!(has(&template[0], "1", "n"), "{template:?}");
        assert!(!template[0].iter().any(|(text, _)| text.contains('+')));
        assert!(has(&tokens("a.json", "{\"a\": \"b\"}\n")[0], "\"a\"", "a"));
        assert!(has(&tokens("a.json", "{\"a\": \"b\"}\n")[0], "\"b\"", "s"));
    }

    #[test]
    fn declines_what_it_cannot_color() {
        assert!(highlight("notes.txt", b"plain words\n").is_none());
        assert!(highlight("a.rs", &[0xff, 0xfe, 0x00]).is_none());
        let long = format!("let a = \"{}\";\n", "x".repeat(MAX_LINE_BYTES));
        assert!(highlight("a.rs", long.as_bytes()).is_none());
    }
}
