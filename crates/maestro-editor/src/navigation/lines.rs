//! Splits raw file bytes into lines the way the editor's rope does, so a
//! line number found on disk names the same line once the file is open.
//! Ropey (with its default `unicode_lines` feature) ends a line at LF, CRLF,
//! a lone CR, VT, FF, NEL, LINE SEPARATOR and PARAGRAPH SEPARATOR; see
//! `maestro_core::text::is_line_ending_str`.

/// Byte length of the line terminator starting at `i`, if one does.
pub fn terminator_len(data: &[u8], i: usize) -> Option<usize> {
    match data[i] {
        b'\n' | 0x0B | 0x0C => Some(1),
        b'\r' => Some(if data.get(i + 1) == Some(&b'\n') {
            2
        } else {
            1
        }),
        0xC2 if data.get(i + 1) == Some(&0x85) => Some(2),
        0xE2 if data.get(i + 1) == Some(&0x80) && matches!(data.get(i + 2), Some(0xA8 | 0xA9)) => {
            Some(3)
        }
        _ => None,
    }
}

/// The end of the line content that starts at or before `from`: the index
/// of the next terminator, or the data length.
pub fn content_end(data: &[u8], from: usize) -> usize {
    (from..data.len())
        .find(|&i| terminator_len(data, i).is_some())
        .unwrap_or(data.len())
}

/// Lines as `(start, content_end)` byte ranges, terminators excluded.
pub fn lines(data: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut pos = 0;
    let mut done = data.is_empty();
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let start = pos;
        let end = content_end(data, start);
        match (end < data.len())
            .then(|| terminator_len(data, end))
            .flatten()
        {
            Some(len) => pos = end + len,
            None => done = true,
        }
        Some((start, end))
    })
}

/// Where a scan through the lines has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// Zero-based line number.
    pub line: usize,
    /// Byte where that line starts.
    pub start: usize,
    /// Byte where the line before it starts.
    pub prev_start: usize,
    /// Where the scan stopped; past the requested end when a terminator
    /// straddles it.
    pub scanned: usize,
}

/// Counts the terminators that begin in `data[c.scanned..to]`.
pub fn advance(data: &[u8], mut c: Cursor, to: usize) -> Cursor {
    let mut i = c.scanned;
    while i < to {
        match terminator_len(data, i) {
            Some(len) => {
                c.line += 1;
                i += len;
                c.prev_start = c.start;
                c.start = i;
            }
            None => i += 1,
        }
    }
    c.scanned = i;
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_numbers_agree_with_the_rope() {
        let samples = [
            "a\nb\r\nc\rd\u{000B}e\u{000C}f\u{0085}g\u{2028}h\u{2029}i",
            "\n\n\r\r\n",
            "no break",
            "ends with break\n",
            "é\u{2028}ü",
        ];
        for s in samples {
            let rope = maestro_core::Rope::from_str(s);
            let ours: Vec<String> = lines(s.as_bytes())
                .map(|(a, b)| s[a..b].to_string())
                .collect();
            let theirs: Vec<String> = (0..rope.len_lines())
                .map(|i| {
                    let l = rope.line(i).to_string();
                    l.trim_end_matches(maestro_core::text::is_line_ending_char)
                        .to_string()
                })
                .collect();
            // The rope has an extra empty line after a final terminator.
            let theirs = if s.is_empty() || ours.len() == theirs.len() {
                theirs
            } else {
                theirs[..theirs.len() - 1].to_vec()
            };
            assert_eq!(ours, theirs, "{s:?}");
        }
    }

    #[test]
    fn advance_counts_each_terminator_once() {
        let d = b"a\r\nb\rc\nd";
        let all = advance(d, Cursor::default(), d.len());
        assert_eq!(
            (all.line, all.start, all.prev_start, all.scanned),
            (3, 7, 5, 8)
        );
        // A CRLF that starts before `to` counts once, and the scan resumes
        // after it.
        let c = advance(d, Cursor::default(), 2);
        assert_eq!((c.line, c.start, c.prev_start, c.scanned), (1, 3, 0, 3));
        let c = advance(d, c, 6);
        assert_eq!((c.line, c.start, c.prev_start, c.scanned), (2, 5, 3, 6));
    }
}
