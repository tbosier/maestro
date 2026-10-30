//! Line-ending detection. Documents keep the terminators they were loaded
//! with; the detected style only decides what a newly inserted line uses.

use ropey::Rope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }

    /// Detects the style from the first line terminator in the first 64 KiB
    /// worth of lines. Defaults to the platform convention when none exists.
    pub fn detect(text: &Rope) -> Self {
        for line in text.lines().take(10_000) {
            let n = line.len_chars();
            if n == 0 {
                continue;
            }
            if line.char(n - 1) == '\n' {
                return if n >= 2 && line.char(n - 2) == '\r' {
                    LineEnding::Crlf
                } else {
                    LineEnding::Lf
                };
            }
        }
        Self::native()
    }

    pub fn native() -> Self {
        if cfg!(windows) {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_first_terminator() {
        assert_eq!(
            LineEnding::detect(&Rope::from_str("a\r\nb\n")),
            LineEnding::Crlf
        );
        assert_eq!(
            LineEnding::detect(&Rope::from_str("a\nb\r\n")),
            LineEnding::Lf
        );
        assert_eq!(
            LineEnding::detect(&Rope::from_str("abc")),
            LineEnding::native()
        );
    }
}
