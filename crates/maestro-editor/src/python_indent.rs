//! Python block headers for auto-indent.
//!
//! A new line after `def f(x):`, `if x:` or `else:` belongs one level
//! deeper. Whether the text before the cursor ends such a header depends
//! on everything before it: a colon inside a string, a comment or an open
//! bracket is not one, and strings and brackets can start lines earlier.
//! So this reads the text from the start of the buffer with a small lexer
//! instead of looking at one line, and does not wait for the syntax tree,
//! which is often missing or full of errors while a header is being typed.

use maestro_core::RopeSlice;

/// The most text read before a cursor. Past this the lexer gives up and
/// the new line keeps the indentation of the line above.
const MAX_CHARS: usize = 4 << 20;

/// The keywords a statement that ends in a suite starts with.
const HEADER_KEYWORDS: &[&str] = &[
    "async", "case", "class", "def", "elif", "else", "except", "finally", "for", "if", "match",
    "try", "while", "with",
];

/// For each position in `positions`: when the text before it ends a Python
/// block header, the line where the header's statement starts (its first
/// physical line, which a header continued over several lines does not
/// end on); `None` otherwise. The text is read once for all positions.
pub fn header_start_lines(text: RopeSlice, positions: &[usize]) -> Vec<Option<usize>> {
    let mut out = vec![None; positions.len()];
    // Most new lines follow a line with no colon on it at all; those need
    // no lexing.
    let mut todo: Vec<(usize, usize)> = positions
        .iter()
        .enumerate()
        .filter(|&(_, &p)| {
            p <= MAX_CHARS && p <= text.len_chars() && {
                let line_start = text.line_to_char(text.char_to_line(p));
                text.slice(line_start..p).chars().any(|c| c == ':')
            }
        })
        .map(|(k, &p)| (p, k))
        .collect();
    let Some(&(end, _)) = todo.iter().max() else {
        return out;
    };
    todo.sort();
    let chars: Vec<char> = text.slice(..end).chars().collect();
    let mut lexer = Lexer::default();
    for (p, k) in todo {
        lexer.run_to(&chars, p);
        out[k] = lexer.header_at(&chars, p).map(|at| text.char_to_line(at));
    }
    out
}

#[derive(Clone, Copy)]
enum Ctx {
    /// A string literal: its quote, whether it is triple quoted, whether
    /// `{` starts a replacement field (f-strings and t-strings), whether it
    /// is raw, and whether it is bytes.
    Str {
        quote: char,
        triple: bool,
        fields: bool,
        raw: bool,
        bytes: bool,
    },
    /// Code inside an f-string's `{...}`, with its own bracket depth.
    Field { depth: u32 },
    /// The format spec after `:` in a replacement field, up to its `}`.
    Spec,
}

#[derive(Default)]
struct Lexer {
    i: usize,
    stack: Vec<Ctx>,
    /// Open brackets in plain code.
    depth: u32,
    /// Where the current statement started.
    statement_start: usize,
    /// The last character that is neither whitespace nor part of a
    /// comment, in plain code (a string counts as its closing quote).
    last: Option<char>,
    /// `lambda`s outside brackets in this statement still waiting for
    /// their colon.
    lambdas: u32,
    /// Colons outside brackets in this statement that are not a lambda's
    /// or a walrus's. Only the first can end a header.
    colons: u32,
    /// Whether the last of those colons was the statement's first.
    first_colon: bool,
    /// Inside a `#` comment, up to the line break. Read a character at a
    /// time like everything else, so reading never runs past a cursor that
    /// sits in the comment.
    comment: bool,
}

impl Lexer {
    /// Reads `chars` up to `p`, or past it when an escape or a word
    /// straddles it.
    fn run_to(&mut self, chars: &[char], p: usize) {
        while self.i < p.min(chars.len()) {
            self.step(chars);
        }
    }

    /// The statement's start when the text read so far, ending exactly at
    /// `p`, ends a block header: a `:` in plain code outside any bracket
    /// (ignoring a trailing comment and whitespace), the statement's first
    /// such colon, after a header keyword. The keyword check leaves out
    /// `value: int`; the colon count leaves out `if a: b: int`; lambda
    /// colons are not counted at all.
    fn header_at(&self, chars: &[char], p: usize) -> Option<usize> {
        if self.i != p
            || !self.stack.is_empty()
            || self.depth != 0
            || self.last != Some(':')
            || !self.first_colon
        {
            return None;
        }
        let rest = &chars[self.statement_start..p];
        let mut it = rest.iter().skip_while(|c| c.is_whitespace()).peekable();
        let mut word = String::new();
        while let Some(&&c) = it.peek() {
            if !is_ident(c) {
                break;
            }
            word.push(c);
            it.next();
        }
        if !HEADER_KEYWORDS.contains(&word.as_str()) {
            return None;
        }
        // `match` and `case` are keywords only in a match statement. As
        // names they are followed by `:` (an annotation), `=` or `,`, or
        // right away by `.` or `[` (`match.x: int`, `case["k"]: str`);
        // a subject or pattern is `match .5:` or `case [a, b]:`.
        if matches!(word.as_str(), "match" | "case") {
            if matches!(it.peek(), Some('.' | '[')) {
                return None;
            }
            let next = it.find(|c| !c.is_whitespace());
            if matches!(next, None | Some(':' | '=' | ',' | ')' | ']' | '}')) {
                return None;
            }
        }
        Some(self.statement_start)
    }

    fn step(&mut self, chars: &[char]) {
        let i = self.i;
        let c = chars[i];
        if self.comment {
            if c != '\n' && c != '\r' {
                self.i += 1;
                return;
            }
            // The line break ends the comment and is read as usual.
            self.comment = false;
        }
        match self.stack.last().copied() {
            Some(Ctx::Str {
                quote,
                triple,
                fields,
                raw,
                bytes,
            }) => {
                if c == '\\' {
                    self.i += escape_len(chars, i, fields, raw || bytes);
                    return;
                }
                if fields && c == '{' {
                    if chars.get(i + 1) == Some(&'{') {
                        self.i += 2;
                        return;
                    }
                    self.stack.push(Ctx::Field { depth: 0 });
                } else if c == quote {
                    if !triple {
                        self.stack.pop();
                    } else if chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote) {
                        self.stack.pop();
                        self.i += 2;
                    }
                    if self.stack.is_empty() {
                        self.last = Some(quote);
                    }
                } else if (c == '\n' || c == '\r') && !triple {
                    // A one-line string the line left open: Python rejects
                    // it, so end it here and read the break as code.
                    self.stack.pop();
                    return;
                }
                self.i += 1;
            }
            Some(Ctx::Spec) => {
                match c {
                    '{' => self.stack.push(Ctx::Field { depth: 0 }),
                    '}' => {
                        self.stack.pop();
                    }
                    _ => {}
                }
                self.i += 1;
            }
            Some(Ctx::Field { depth: d }) => {
                match c {
                    '}' if d == 0 => {
                        self.stack.pop();
                    }
                    // `{x:>10}`: the rest of the field is its format spec.
                    // (A walrus inside a field needs parentheses, so
                    // `{x:=5}` is a spec too.)
                    ':' if d == 0 => {
                        self.stack.pop();
                        self.stack.push(Ctx::Spec);
                    }
                    '(' | '[' | '{' => self.set_field_depth(d + 1),
                    ')' | ']' | '}' => self.set_field_depth(d.saturating_sub(1)),
                    '#' => self.comment = true,
                    '"' | '\'' => {
                        self.i = open_string(chars, i, &mut self.stack);
                        return;
                    }
                    _ => {}
                }
                self.i += 1;
            }
            None => self.code(chars, c),
        }
    }

    fn code(&mut self, chars: &[char], c: char) {
        let i = self.i;
        match c {
            '#' => {
                self.comment = true;
                self.i += 1;
                return;
            }
            '\\' if matches!(chars.get(i + 1), Some('\n' | '\r')) => {
                // A line continuation: the statement goes on.
                let crlf = chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n');
                self.i += if crlf { 3 } else { 2 };
                return;
            }
            '\n' | '\r' if self.depth == 0 => {
                self.statement_start = i + 1;
                self.last = None;
                self.lambdas = 0;
                self.colons = 0;
                self.first_colon = false;
            }
            '(' | '[' | '{' => self.depth += 1,
            ')' | ']' | '}' => self.depth = self.depth.saturating_sub(1),
            '"' | '\'' => {
                self.i = open_string(chars, i, &mut self.stack);
                return;
            }
            ':' if self.depth == 0 && chars.get(i + 1) != Some(&'=') => {
                if self.lambdas > 0 {
                    self.lambdas -= 1;
                    self.first_colon = false;
                } else {
                    self.colons += 1;
                    self.first_colon = self.colons == 1;
                }
            }
            c if is_ident(c) && (i == 0 || !is_ident(chars[i - 1])) => {
                let mut j = i;
                while j < chars.len() && is_ident(chars[j]) {
                    j += 1;
                }
                // A string prefix is read with its quote by `open_string`.
                if !matches!(chars.get(j), Some('"' | '\'')) {
                    if self.depth == 0 && chars[i..j] == ['l', 'a', 'm', 'b', 'd', 'a'] {
                        self.lambdas += 1;
                    }
                    self.last = Some(chars[j - 1]);
                    self.i = j;
                    return;
                }
            }
            _ => {}
        }
        if !c.is_whitespace() {
            self.last = Some(c);
        }
        self.i += 1;
    }

    fn set_field_depth(&mut self, depth: u32) {
        if let Some(Ctx::Field { depth: d }) = self.stack.last_mut() {
            *d = depth;
        }
    }
}

/// How many characters the backslash at `i` in a string takes with it.
/// It hides the next character from the lexer, so an escaped quote does
/// not end the string; an escaped CRLF counts as one line break. In an
/// f-string it never hides a brace (Python reads `\{` there as a backslash
/// and then a field, raw or not), and in a string that is neither raw nor
/// bytes `\N{...}` names a character, so its braces are not a field.
fn escape_len(chars: &[char], i: usize, fields: bool, plain: bool) -> usize {
    match (chars.get(i + 1), chars.get(i + 2)) {
        (Some('{' | '}'), _) if fields => 1,
        (Some('N'), Some('{')) if !plain => {
            let mut j = i + 3;
            while j < chars.len() && !matches!(chars[j], '}' | '\n' | '\r') {
                j += 1;
            }
            if chars.get(j) == Some(&'}') {
                j + 1 - i
            } else {
                j - i
            }
        }
        (Some('\r'), Some('\n')) => 3,
        _ => 2,
    }
}

/// Pushes the string whose opening quote is at `i` and returns the index
/// after its opening quote or quotes. Its prefix (`r`, `b`, `f`, `rb`,
/// `fr`, `t` and so on, any case) decides how its escapes and braces read.
fn open_string(chars: &[char], i: usize, stack: &mut Vec<Ctx>) -> usize {
    let quote = chars[i];
    let mut start = i;
    while start > 0 && i - start < 2 && chars[start - 1].is_ascii_alphabetic() {
        start -= 1;
    }
    let prefix: String = chars[start..i]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    let is_prefix = matches!(
        prefix.as_str(),
        "" | "r" | "u" | "b" | "f" | "t" | "br" | "rb" | "fr" | "rf" | "tr" | "rt"
    ) && (start == 0 || !is_ident(chars[start - 1]));
    let has = |c: char| is_prefix && prefix.contains(c);
    let triple = chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote);
    stack.push(Ctx::Str {
        quote,
        triple,
        fields: has('f') || has('t'),
        raw: has('r'),
        bytes: has('b'),
    });
    if triple { i + 3 } else { i + 1 }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(src: &str) -> Option<usize> {
        let chars: Vec<char> = src.chars().collect();
        let mut lexer = Lexer::default();
        lexer.run_to(&chars, chars.len());
        lexer.header_at(&chars, chars.len())
    }

    #[test]
    fn simple_headers() {
        for src in [
            "def blah(x):",
            "if x:",
            "else:",
            "class A(B):",
            "for i in range(3):",
            "with open(p) as f:",
            "match x:",
            "case [a, b]:",
            "async def f() -> dict[str, int]:",
            "if x:  # note",
            "if x:   ",
            "try:\t# why",
        ] {
            assert_eq!(header(src), Some(0), "{src}");
        }
    }

    #[test]
    fn colons_that_are_not_headers() {
        for src in [
            "x = {\"a\":",
            "y = s[1:",
            "s = \"a:",
            "s = 'a:",
            "s = \"\"\"Summary\nTitle:",
            "s = '''x\n  y:",
            "# todo:",
            "z = 1  # note:",
            "x: int",
            "x = 1",
            "",
            "values[\n    1:",
            "d = {\n    \"a\":",
            "f(a,\n  b=lambda: 1,\n  c:",
            "f = lambda x:",
            "if lambda x:",
            "if ready: value:",
            "match:",
            "match = 1\ncase:",
            "match.x:",
            "case[\"name\"]:",
            "match[0]:",
            "value:",
            "self.x:",
            "s = \"a\\\r\nTitle:",
            "pattern = rf\"\\{{\"\ns = 'a:",
            "s = f\"{x:",
            "s = f\"{x!r:>{w}:",
        ] {
            assert_eq!(header(src), None, "{src:?}");
        }
    }

    #[test]
    fn strings_that_end_before_the_colon() {
        for src in [
            "if message == '''It's ready''':",
            "if f\"{mapping[\"a#b\"]}\":",
            "if s == \"a:\\\"b\":",
            "if s == r'\\'':",
            "if s == f\"{{literal}}\":",
            "if s == f\"{x:>10}\":",
            "if s == f'{x!r:{w}}':",
            "if s == f\"{x:=5}\":",
            "if s == b'#':",
            "if rf\"{a}\" in xs:",
            "if s == \"\\N{EM DASH}\":",
            "if f\"\\N{BULLET} {x}\":",
            "if s == rf\"\\{x}\":",
            "if \"a\\\r\nb\":",
            "if data == b\"\\N{\":",
            "while chunk := f.read():",
            "if f(lambda: 1):",
            "match (x):",
            "match point:",
            "match .5:",
            "match ...:",
            "case [a, b]:",
            "case .5:",
        ] {
            assert_eq!(header(src), Some(0), "{src}");
        }
        let src = "x = 1\nif text == \"\"\"first\nsecond\"\"\":";
        assert_eq!(header(src), Some(6));
        // `\{{` in a raw f-string is a backslash and a literal brace; the
        // string ends at its quote and later headers still count.
        let src = "pattern = rf\"\\{{\"\nif ready:";
        assert_eq!(header(src), Some(18));
    }

    #[test]
    fn a_header_continued_over_lines_starts_at_its_first_line() {
        let src = "x = 1\nif (ready\n    and enabled):";
        assert_eq!(header(src), Some(6));
        let src = "def f(\n    a,\n    b,\n) -> int:";
        assert_eq!(header(src), Some(0));
        let src = "if a and \\\n   b:";
        assert_eq!(header(src), Some(0));
        let src = "if a and \\\r\n   b:";
        assert_eq!(header(src), Some(0));
    }

    #[test]
    fn identifiers_ending_in_prefix_letters_are_not_prefixes() {
        // `elif` ends in `f`, but `elif'x'` is not an f-string; neither is
        // `buf"..."`. The `{` inside must not open a field.
        assert_eq!(header("if buf\"{\" == x:"), Some(0));
    }

    #[test]
    fn a_cursor_in_a_comment_does_not_hide_later_cursors_or_its_own_header() {
        // Cursors after "# first" and after "if b:".
        let text = maestro_core::Rope::from_str("if a: # first second\n    pass\nif b:\n");
        let s = text.slice(..);
        let first = "if a: # first".chars().count();
        let second = "if a: # first second\n    pass\nif b:".chars().count();
        assert_eq!(
            header_start_lines(s, &[first, second]),
            vec![Some(0), Some(2)]
        );
        assert_eq!(header_start_lines(s, &[first]), vec![Some(0)]);
    }

    #[test]
    fn one_pass_answers_every_cursor() {
        let text = maestro_core::Rope::from_str("if a:\nx = 1\ndef f(\n    b):\ny: int\n");
        let s = text.slice(..);
        let ends: Vec<usize> = (0..5)
            .map(|l| maestro_core::text::line_end_excluding_terminator(s, l))
            .collect();
        assert_eq!(
            header_start_lines(s, &ends),
            vec![Some(0), None, None, Some(2), None]
        );
    }
}
