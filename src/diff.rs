//! Diff model shared by the git (review) and jj (history) sides, so the web
//! client renders both with one component.

use bstr::BStr;
use jj_lib::diff::ContentDiff;
use jj_lib::diff_presentation::DiffTokenType;
use jj_lib::diff_presentation::LineCompareMode;
use jj_lib::diff_presentation::unified::DiffLineType;
use jj_lib::diff_presentation::unified::UnifiedDiffHunk;
use jj_lib::diff_presentation::unified::unified_diff_hunks;
use jj_lib::diff_presentation::unzip_diff_hunks_to_lines;
use jj_lib::merge::Diff;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum LineKind {
    #[serde(rename = " ")]
    Context,
    #[serde(rename = "-")]
    Removed,
    #[serde(rename = "+")]
    Added,
}

#[derive(Clone, Debug, Serialize)]
pub struct Line {
    pub kind: LineKind,
    /// Line content without the trailing newline, lossily decoded.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<usize>,
    /// The line has no trailing newline (it is the last line of its side).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_eol: bool,
    /// Intra-line changed ranges, as UTF-16 offsets into `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hl: Option<Vec<[usize; 2]>>,
    /// Syntax tokens as `[start, end, class]`, in UTF-16 offsets into `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syn: Option<Vec<crate::syntax::Span>>,
    /// Exact bytes of the line without the trailing newline. Patches are
    /// rebuilt from these, never from `text`.
    #[serde(skip)]
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Hunk {
    /// Content-derived id; stable until the hunk itself changes.
    pub id: String,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    /// Function/section context printed after the `@@` markers, if any.
    pub section: String,
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    pub binary: bool,
    /// Human-readable reason this file can only be handled as a whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
    /// Hunks were left out to keep the response small; fetch the file alone.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deferred: bool,
    /// Raw header lines preceding the first hunk (git side only).
    #[serde(skip)]
    pub header: Vec<Vec<u8>>,
}

impl Hunk {
    pub fn compute_id(&mut self) {
        let mut hasher = Sha256::new();
        hasher.update(
            format!(
                "{},{},{},{}\n",
                self.old_start, self.old_count, self.new_start, self.new_count
            )
            .as_bytes(),
        );
        for line in &self.lines {
            hasher.update(match line.kind {
                LineKind::Context => b" ",
                LineKind::Removed => b"-",
                LineKind::Added => b"+",
            });
            hasher.update(&line.raw);
            hasher.update(if line.no_eol { b"\0" } else { b"\n" });
        }
        self.id = hex::encode(&hasher.finalize()[..8]);
    }
}

/// Converts a hunk computed by `jj-lib` into the shared model.
pub fn convert_hunk(hunk: &UnifiedDiffHunk<'_>) -> Hunk {
    let old_count = hunk.left_line_range.len();
    let new_count = hunk.right_line_range.len();
    // Unified diff numbers lines from 1, except that an empty range names the
    // line it follows.
    let start = |first: usize, count: usize| if count == 0 { first } else { first + 1 };
    let mut old_no = hunk.left_line_range.start + 1;
    let mut new_no = hunk.right_line_range.start + 1;
    let mut lines = Vec::with_capacity(hunk.lines.len());
    for (kind, tokens) in &hunk.lines {
        let kind = match kind {
            DiffLineType::Context => LineKind::Context,
            DiffLineType::Removed => LineKind::Removed,
            DiffLineType::Added => LineKind::Added,
        };
        let (text, hl) = tokens_to_text(tokens);
        let raw: Vec<u8> = tokens
            .iter()
            .flat_map(|(_, bytes)| *bytes)
            .copied()
            .collect();
        let no_eol = !raw.ends_with(b"\n");
        let mut line = Line {
            kind,
            text,
            old: None,
            new: None,
            no_eol,
            hl: if kind == LineKind::Context { None } else { hl },
            syn: None,
            raw: raw.strip_suffix(b"\n").unwrap_or(&raw).to_vec(),
        };
        if kind != LineKind::Added {
            line.old = Some(old_no);
            old_no += 1;
        }
        if kind != LineKind::Removed {
            line.new = Some(new_no);
            new_no += 1;
        }
        lines.push(line);
    }
    let mut hunk = Hunk {
        id: String::new(),
        old_start: start(hunk.left_line_range.start, old_count),
        old_count,
        new_start: start(hunk.right_line_range.start, new_count),
        new_count,
        section: String::new(),
        lines,
    };
    hunk.compute_id();
    hunk
}

/// Diffs two versions of a text file, with syntax tokens. `path` only decides
/// how the content is colored.
pub fn text_diff(path: &str, old: &str, new: &str, context: usize) -> FileDiff {
    let contents = Diff::new(BStr::new(old), BStr::new(new));
    let mut hunks: Vec<Hunk> = unified_diff_hunks(contents, context, LineCompareMode::Exact)
        .iter()
        .map(convert_hunk)
        .collect();
    let count = |kind| {
        let lines = hunks.iter().flat_map(|hunk| &hunk.lines);
        lines.filter(|line| line.kind == kind).count()
    };
    let (added, removed) = (count(LineKind::Added), count(LineKind::Removed));
    let old = crate::syntax::highlight(path, old.as_bytes());
    let new = crate::syntax::highlight(path, new.as_bytes());
    crate::syntax::apply(&mut hunks, old.as_deref(), new.as_deref());
    FileDiff {
        path: path.to_owned(),
        status: FileStatus::Modified,
        old_mode: None,
        new_mode: None,
        binary: false,
        note: None,
        added,
        removed,
        hunks,
        deferred: false,
        header: vec![],
    }
}

/// Converts word-level diff tokens of one line into display text plus the
/// changed ranges. A line that was edited gets the words that changed; one
/// that shares no word with its counterpart, or has none, gets the whole line
/// past its indentation, so that it reads as changed in the same way.
pub fn tokens_to_text(tokens: &[(DiffTokenType, &[u8])]) -> (String, Option<Vec<[usize; 2]>>) {
    let mut text = String::new();
    let mut ranges: Vec<[usize; 2]> = vec![];
    let mut offset = 0;
    let mut has_matching = false;
    // End of unchanged whitespace or punctuation that directly follows the
    // last range, so that consecutive changed words read as one phrase.
    let mut bridge = None;
    for (kind, bytes) in tokens {
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        if bytes.is_empty() {
            continue;
        }
        let piece = String::from_utf8_lossy(bytes);
        let len = piece.encode_utf16().count();
        let end = offset + len;
        match kind {
            DiffTokenType::Matching if !piece.chars().any(char::is_alphanumeric) => {
                let follows_range = ranges.last().is_some_and(|last| last[1] == offset);
                bridge = follows_range.then_some(end);
            }
            DiffTokenType::Matching => {
                has_matching = true;
                bridge = None;
            }
            DiffTokenType::Different => {
                match ranges.last_mut() {
                    Some(last) if last[1] == offset || bridge == Some(offset) => last[1] = end,
                    _ => ranges.push([offset, end]),
                }
                bridge = None;
            }
        }
        offset = end;
        text.push_str(&piece);
    }
    if ranges.is_empty() {
        return (text, None);
    }
    // A line that shares no word with what it replaced was rewritten.
    let hl = if has_matching {
        Some(ranges)
    } else {
        whole_line(&text)
    };
    (text, hl)
}

/// The range of a line from its first to its last visible character, in
/// UTF-16 units; none for a blank line.
pub fn whole_line(text: &str) -> Option<Vec<[usize; 2]>> {
    let units = |text: &str| text.encode_utf16().count();
    let start = units(&text[..text.len() - text.trim_start().len()]);
    let end = units(text.trim_end());
    (start < end).then(|| vec![[start, end]])
}

/// Adds intra-line highlights to a hunk parsed from plain unified diff text.
pub fn highlight_hunk(hunk: &mut Hunk) {
    const MAX_RUN_BYTES: usize = 64 * 1024;
    // A line with no counterpart is new or gone as a whole. Runs of removed
    // lines followed by added ones are then compared word by word.
    for line in &mut hunk.lines {
        if line.kind != LineKind::Context {
            line.hl = whole_line(&line.text);
        }
    }
    let mut i = 0;
    while i < hunk.lines.len() {
        if hunk.lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < hunk.lines.len() && hunk.lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let added_start = i;
        while i < hunk.lines.len() && hunk.lines[i].kind == LineKind::Added {
            i += 1;
        }
        if added_start == i {
            continue;
        }
        let join = |lines: &[Line]| {
            let mut buf = Vec::new();
            for line in lines {
                buf.extend_from_slice(&line.raw);
                buf.push(b'\n');
            }
            buf
        };
        let before = join(&hunk.lines[removed_start..added_start]);
        let after = join(&hunk.lines[added_start..i]);
        if before.len() + after.len() > MAX_RUN_BYTES {
            continue;
        }
        let diff = ContentDiff::by_word([&before, &after]);
        let lines = unzip_diff_hunks_to_lines(diff.hunks());
        let sides = [
            (removed_start, added_start, &lines.before),
            (added_start, i, &lines.after),
        ];
        for (start, end, tokens) in sides {
            if tokens.len() != end - start {
                continue;
            }
            for (line, tokens) in hunk.lines[start..end].iter_mut().zip(tokens) {
                line.hl = tokens_to_text(tokens).1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiffTokenType::Different as D;
    use DiffTokenType::Matching as M;

    #[test]
    fn highlights_an_edited_word() {
        let tokens: &[(DiffTokenType, &[u8])] =
            &[(M, b"let total: "), (D, b"u32"), (M, b" = 0;\n")];
        let (text, hl) = tokens_to_text(tokens);
        assert_eq!(text, "let total: u32 = 0;");
        assert_eq!(hl, Some(vec![[11, 14]]));
    }

    #[test]
    fn joins_changes_across_spaces_and_punctuation() {
        let tokens: &[(DiffTokenType, &[u8])] = &[
            (M, b"call"),
            (D, b"(a"),
            (M, b", "),
            (D, b"b)"),
            (M, b" done"),
        ];
        assert_eq!(tokens_to_text(tokens).1, Some(vec![[4, 10]]));
    }

    #[test]
    fn joins_changed_words_across_spaces() {
        let tokens: &[(DiffTokenType, &[u8])] = &[
            (M, b"the quick brown fox jumps over "),
            (D, b"a"),
            (M, b" "),
            (D, b"lazy"),
            (M, b" dog"),
        ];
        assert_eq!(tokens_to_text(tokens).1, Some(vec![[31, 37]]));
    }

    #[test]
    fn highlights_many_small_edits() {
        // Emphasis added around three words of a long line.
        let tokens: &[(DiffTokenType, &[u8])] = &[
            (M, b"Contains the \"root\" "),
            (D, b"*"),
            (M, b"DataNode"),
            (D, b"*"),
            (M, b" of a directed-acyclic "),
            (D, b"*"),
            (M, b"DataGraph"),
            (D, b"*"),
            (M, b", located at a "),
            (D, b"*"),
            (M, b"Viewpoint"),
            (D, b"*"),
            (M, b".\n"),
        ];
        let (text, hl) = tokens_to_text(tokens);
        let marked: Vec<_> = hl.unwrap().iter().map(|[a, b]| &text[*a..*b]).collect();
        assert_eq!(marked, ["*"; 6]);
    }

    #[test]
    fn marks_new_and_rewritten_lines_whole() {
        let whole: &[(DiffTokenType, &[u8])] = &[(D, b"    entirely new  \n")];
        assert_eq!(
            tokens_to_text(whole),
            ("    entirely new  ".to_owned(), Some(vec![[4, 16]]))
        );
        // Nothing but punctuation in common is no edit.
        let rewritten: &[(DiffTokenType, &[u8])] = &[
            (D, b"let a"),
            (M, b" = "),
            (D, b"b"),
            (M, b"("),
            (D, b"c"),
            (M, b");"),
        ];
        assert_eq!(tokens_to_text(rewritten).1, Some(vec![[0, 13]]));
        // A word in common is: what is around it was edited, however much.
        let mostly: &[(DiffTokenType, &[u8])] = &[(M, b"    20"), (D, b" + step_1()"), (M, b"\n")];
        assert_eq!(tokens_to_text(mostly).1, Some(vec![[6, 17]]));
        let blank: &[(DiffTokenType, &[u8])] = &[(D, b"   \n")];
        assert_eq!(tokens_to_text(blank).1, None);
        // Offsets count UTF-16 units, which is what the browser indexes by.
        let wide: &[(DiffTokenType, &[u8])] =
            &[(M, "h\u{e9}llo w\u{f6}rld and more ".as_bytes()), (D, b"x")];
        assert_eq!(tokens_to_text(wide).1, Some(vec![[21, 22]]));
    }
}
