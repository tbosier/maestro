//! Line comments: `toggle_comment` comments or uncomments the lines each
//! selection touches, with the language's line comment token.

use std::path::Path;

use maestro_core::text::line_end_excluding_terminator;
use maestro_core::{Assoc, Change, ChangeSet, Range, RopeSlice, Selection, Transaction};
use maestro_syntax::Language;

use crate::editor::{Editor, Severity};

crate::commands! {
    toggle_comment => "Comment or uncomment the selected lines",
}

/// Default normal and select mode keys, bound by `keymap.rs`. Most
/// terminals send Ctrl+/ as the byte Ctrl+7 also sends (0x1f), which
/// arrives as `C-7`; terminals with the kitty keyboard protocol send `C-/`.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("C-c", "toggle_comment"),
    ("C-/", "toggle_comment"),
    ("C-7", "toggle_comment"),
];

/// The line comment token for a buffer: from its detected language, else
/// from a short list of file names, extensions and `#!` programs for files
/// Maestro has no grammar for. `Err` names what has none.
pub fn line_comment_token(
    language: Option<Language>,
    path: Option<&Path>,
    first_line: &str,
) -> Result<&'static str, String> {
    if let Some(lang) = language {
        return match lang {
            Language::Python | Language::Toml => Ok("#"),
            Language::Rust
            | Language::JavaScript
            | Language::TypeScript
            | Language::Tsx
            | Language::C => Ok("//"),
            Language::Json | Language::Markdown => {
                Err(format!("{} has no line comments", lang.name()))
            }
        };
    }
    if let Some(token) = path.and_then(token_from_path) {
        return Ok(token);
    }
    if let Some(token) = token_from_shebang(first_line) {
        return Ok(token);
    }
    Err("no line comment is known for this file type".to_string())
}

fn token_from_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    match name {
        "Makefile" | "makefile" | "GNUmakefile" | "Dockerfile" | "Containerfile" | ".gitignore"
        | ".gitattributes" | ".dockerignore" | ".bashrc" | ".bash_profile" | ".zshrc"
        | ".profile" => return Some("#"),
        _ => {}
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "sh" | "bash" | "zsh" | "fish" | "yaml" | "yml" | "mk" | "dockerfile" => Some("#"),
        "go" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "java" => Some("//"),
        "lua" | "sql" => Some("--"),
        _ => None,
    }
}

fn token_from_shebang(first_line: &str) -> Option<&'static str> {
    let rest = first_line.strip_prefix("#!")?;
    let mut words = rest.split_whitespace();
    let mut program = words.next()?.rsplit('/').next()?;
    if program == "env" {
        program = words.find(|w| !w.starts_with('-'))?;
    }
    match program {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => Some("#"),
        _ => None,
    }
}

/// The edits that toggle `token` on the lines `first..=last`: when every
/// non-blank line already starts (after its indentation) with the token,
/// each loses it and one space after it; otherwise each non-blank line gets
/// `token` and a space after the indentation the lines share, so the block
/// stays aligned. Blank lines are never touched, and neither is a `#!`
/// line at the top of the file: commenting it would stop the file from
/// running (and from being recognized by it), and uncommenting it would
/// turn it into `!/usr/bin/...`. `None` when no line in the block is left.
fn toggle_block(s: RopeSlice, first: usize, last: usize, token: &str) -> Option<Vec<Change>> {
    // (line start, leading whitespace) of each non-blank line.
    let lines: Vec<(usize, String)> = (first..=last)
        .filter_map(|l| {
            let start = s.line_to_char(l);
            let end = line_end_excluding_terminator(s, l);
            let indent: String = s
                .slice(start..end)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            let blank = indent.chars().count() == end - start;
            let shebang = l == 0 && s.slice(start..end).chars().take(2).eq("#!".chars());
            (!blank && !shebang).then_some((start, indent))
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let token_len = token.chars().count();
    let starts_with_token = |start: usize, indent: &str| {
        let at = start + indent.chars().count();
        let end = (at + token_len).min(s.len_chars());
        s.slice(at..end).chars().eq(token.chars())
    };
    if lines
        .iter()
        .all(|(start, ind)| starts_with_token(*start, ind))
    {
        return Some(
            lines
                .iter()
                .map(|(start, ind)| {
                    let at = start + ind.chars().count();
                    let mut to = at + token_len;
                    if to < s.len_chars() && s.char(to) == ' ' {
                        to += 1;
                    }
                    Change::delete(at, to)
                })
                .collect(),
        );
    }
    // The longest indentation every line starts with: the smallest indent
    // when they are all spaces or all tabs, and never inside a tab when
    // they mix.
    let common = lines.iter().fold(lines[0].1.len(), |n, (_, ind)| {
        // Indentation is ASCII spaces and tabs, so bytes are chars.
        lines[0]
            .1
            .bytes()
            .zip(ind.bytes())
            .take(n)
            .take_while(|(x, y)| x == y)
            .count()
    });
    Some(
        lines
            .iter()
            .map(|(start, _)| Change::insert(start + common, format!("{token} ")))
            .collect(),
    )
}

fn toggle_comment(ed: &mut Editor) {
    let text = ed.doc().text().clone();
    let s = text.slice(..);
    let first_line: String = s.line(0).chars().take(256).collect();
    let token =
        match line_comment_token(ed.buffer().syntax.language(), ed.doc().path(), &first_line) {
            Ok(t) => t,
            Err(why) => {
                ed.set_status(Severity::Warning, format!("toggle comment: {why}"));
                return;
            }
        };
    // Line spans of the selections, merged where they overlap so no line
    // is toggled twice; each span decides for itself.
    let mut spans: Vec<(usize, usize)> = ed
        .view()
        .selection
        .iter()
        .map(|r| (s.char_to_line(r.from()), s.char_to_line(r.last())))
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in spans {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let changes: Vec<Change> = merged
        .into_iter()
        .filter_map(|(a, b)| toggle_block(s, a, b, token))
        .flatten()
        .collect();
    if changes.is_empty() {
        ed.info("toggle comment: nothing to toggle on the selected lines");
        return;
    }
    let cs = ChangeSet::from_changes_clipped(text.len_chars(), changes);
    // A cursor stays on its character; a selection keeps covering the text
    // it covered, with a token added at its start inside it.
    let ranges: Vec<Range> = ed
        .view()
        .selection
        .iter()
        .map(|r| map_range(*r, &cs))
        .collect();
    let primary = ed.view().selection.primary_index();
    let tx = Transaction::new(cs).with_selection(Selection::new(ranges, primary));
    ed.apply_or_report(tx);
}

fn map_range(r: Range, cs: &ChangeSet) -> Range {
    if r.anchor == r.head {
        return Range::point(cs.map_pos(r.head, Assoc::After));
    }
    let from = cs.map_pos(r.from(), Assoc::Before);
    let last = cs.map_pos(r.last(), Assoc::After);
    if r.anchor <= r.head {
        Range::new(from, last)
    } else {
        Range::new(last, from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_core::Rope;

    fn toggle(src: &str, first: usize, last: usize, token: &str) -> String {
        let mut text = Rope::from_str(src);
        let s = text.slice(..);
        let Some(changes) = toggle_block(s, first, last, token) else {
            return src.to_string();
        };
        let cs = ChangeSet::from_changes_clipped(text.len_chars(), changes);
        cs.apply(&mut text).unwrap();
        text.to_string()
    }

    #[test]
    fn comments_at_the_shared_indent_and_skips_blank_lines() {
        let src = "    if x:\n\n        y()\n    z\n";
        assert_eq!(
            toggle(src, 0, 3, "#"),
            "    # if x:\n\n    #     y()\n    # z\n"
        );
    }

    #[test]
    fn uncomments_when_every_non_blank_line_is_commented() {
        let src = "    # if x:\n  \n    #     y()\n    #z\n";
        assert_eq!(
            toggle(src, 0, 3, "#"),
            "    if x:\n  \n        y()\n    z\n"
        );
    }

    #[test]
    fn a_mix_of_commented_and_plain_lines_gets_commented() {
        let src = "# a\nb\n";
        assert_eq!(toggle(src, 0, 1, "#"), "# # a\n# b\n");
    }

    #[test]
    fn mixed_tabs_and_spaces_comment_after_the_shared_prefix() {
        let src = "\tx\n    y\n";
        assert_eq!(toggle(src, 0, 1, "//"), "// \tx\n//     y\n");
        let src = "\t\tx\n\ty\n";
        assert_eq!(toggle(src, 0, 1, "//"), "\t// \tx\n\t// y\n");
    }

    #[test]
    fn a_shebang_line_is_left_alone() {
        let src = "#!/usr/bin/env python3\n# a\n# b\n";
        assert_eq!(toggle(src, 0, 2, "#"), "#!/usr/bin/env python3\na\nb\n");
        let src = "#!/bin/sh\necho hi\n";
        assert_eq!(toggle(src, 0, 1, "#"), "#!/bin/sh\n# echo hi\n");
        assert_eq!(toggle("#!/bin/sh\n", 0, 0, "#"), "#!/bin/sh\n");
        // Only the first line can be one.
        assert_eq!(toggle("x\n#!y\n", 1, 1, "#"), "x\n!y\n");
    }

    #[test]
    fn only_blank_lines_change_nothing() {
        assert_eq!(toggle("\n   \n", 0, 1, "#"), "\n   \n");
    }

    #[test]
    fn tokens_by_language_path_and_shebang() {
        let p = |s: &str| Some(Path::new(s).to_path_buf());
        assert_eq!(
            line_comment_token(Some(Language::Python), None, ""),
            Ok("#")
        );
        assert_eq!(line_comment_token(Some(Language::Toml), None, ""), Ok("#"));
        assert_eq!(line_comment_token(Some(Language::Rust), None, ""), Ok("//"));
        assert_eq!(line_comment_token(Some(Language::Tsx), None, ""), Ok("//"));
        assert_eq!(line_comment_token(Some(Language::C), None, ""), Ok("//"));
        assert!(line_comment_token(Some(Language::Json), None, "").is_err());
        assert!(line_comment_token(Some(Language::Markdown), None, "").is_err());
        for (name, want) in [
            ("ci.yml", "#"),
            ("x.yaml", "#"),
            ("run.sh", "#"),
            ("Makefile", "#"),
            ("main.go", "//"),
            ("a.cpp", "//"),
            ("init.lua", "--"),
            ("q.sql", "--"),
        ] {
            assert_eq!(
                line_comment_token(None, p(name).as_deref(), ""),
                Ok(want),
                "{name}"
            );
        }
        assert_eq!(
            line_comment_token(None, p("deploy").as_deref(), "#!/usr/bin/env bash"),
            Ok("#")
        );
        assert!(line_comment_token(None, p("notes.txt").as_deref(), "").is_err());
        assert!(line_comment_token(None, None, "").is_err());
    }
}
