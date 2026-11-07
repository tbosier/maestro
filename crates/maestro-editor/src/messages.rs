//! The message log. Every status message set through
//! [`Editor::set_status`] (and `info`, `error` and `add_status`, which call
//! it or share its logging) is kept here with its severity and time, so a
//! message the status line cut short, or a later one replaced, can be read
//! again with `:messages`.

use std::collections::VecDeque;

use maestro_core::{Document, Selection};

use crate::editor::{DocumentId, Editor, Severity};

/// How many messages the log keeps. Older ones are dropped first.
pub const LOG_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub severity: Severity,
    /// Seconds since the Unix epoch when it was last shown.
    pub at: i64,
    pub text: String,
    /// How many times in a row it was shown (1 unless it repeated).
    pub count: usize,
}

/// A bounded ring of status messages, oldest first.
#[derive(Debug, Default)]
pub struct MessageLog {
    entries: VecDeque<Message>,
    /// The buffer the last `:messages` opened; the next one replaces it.
    doc: Option<DocumentId>,
}

impl MessageLog {
    /// Records a message shown now.
    pub fn push(&mut self, severity: Severity, text: &str) {
        self.push_at(severity, text, now_unix());
    }

    /// Records a message shown at `at` (Unix seconds). The same message
    /// shown again right after itself bumps a count instead of taking
    /// another slot.
    pub fn push_at(&mut self, severity: Severity, text: &str, at: i64) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.entries.back_mut()
            && last.severity == severity
            && last.text == text
        {
            last.count += 1;
            last.at = at;
            return;
        }
        if self.entries.len() == LOG_LEN {
            self.entries.pop_front();
        }
        self.entries.push_back(Message {
            severity,
            at,
            text: text.to_string(),
            count: 1,
        });
    }

    /// The kept messages, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Message> + ExactSizeIterator {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `HH:MM:SS` in UTC for a Unix time.
fn utc_clock(secs: i64) -> String {
    let day = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
}

/// Splits `line` into pieces at most `width` columns wide, breaking after
/// a space where one is in reach and inside a word otherwise. Columns are
/// counted as the editor draws text (wide characters take two).
fn wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    // Byte offset in `cur` just after its last space, and the width there.
    let mut last_space: Option<(usize, usize)> = None;
    for g in maestro_core::text::graphemes(maestro_core::RopeSlice::from(line)) {
        let w = maestro_core::text::grapheme_width(&g, cur_w, 4);
        if cur_w + w > width && !cur.is_empty() {
            if g == " " {
                // The line ends exactly here; the space goes with the break.
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                last_space = None;
                continue;
            }
            match last_space {
                Some((at, at_w)) if at < cur.len() => {
                    let rest = cur.split_off(at);
                    out.push(cur.trim_end().to_string());
                    cur = rest;
                    cur_w -= at_w;
                }
                _ => {
                    out.push(cur.trim_end().to_string());
                    cur = String::new();
                    cur_w = 0;
                }
            }
            last_space = None;
        }
        cur.push_str(&g);
        cur_w += w;
        if g == " " {
            last_space = Some((cur.len(), cur_w));
        }
    }
    out.push(cur);
    out
}

/// The log as text, oldest first, and the char offset of the newest
/// message's first line (where the cursor goes). Message text is wrapped
/// to `width` columns (pass `usize::MAX` for none), continuation lines
/// indented under it.
pub fn render(log: &MessageLog, width: usize) -> (String, usize) {
    let mut text =
        format!("Status messages, oldest first. Times are UTC. The last {LOG_LEN} are kept.\n\n");
    if log.is_empty() {
        text.push_str("No messages yet.\n");
        return (text, 0);
    }
    // Width of the widest severity word, so the text column lines up.
    let word_w = "warning".len();
    let indent = " ".repeat("00:00:00".len() + 2 + word_w + 2);
    let text_w = width.saturating_sub(indent.len()).max(20);
    let mut newest = 0;
    for m in log.iter() {
        newest = text.len();
        let mut body = m.text.clone();
        if m.count > 1 {
            body.push_str(&format!(" (shown {} times in a row)", m.count));
        }
        let pieces = body.lines().flat_map(|l| wrap(l, text_w));
        for (i, piece) in pieces.enumerate() {
            let line = if i == 0 {
                format!(
                    "{}  {:<word_w$}  {piece}",
                    utc_clock(m.at),
                    m.severity.label()
                )
            } else {
                format!("{indent}{piece}")
            };
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    let newest = text[..newest].chars().count();
    (text, newest)
}

/// `:messages`: opens the log in a scratch buffer with the cursor on the
/// newest message. The buffer an earlier `:messages` opened is dropped when
/// nothing else shows it and it was not edited.
pub fn show(ed: &mut Editor) {
    // Room for text in the focused view, less a gutter wide enough for
    // five-digit line numbers, so wrapped lines need no sideways scrolling.
    let width = (ed.view().area.width as usize).saturating_sub(8);
    let (text, newest) = render(&ed.messages, width);
    let old = ed.messages.doc.take();
    let id = ed.add_document(Document::from_text(&text));
    if let Some(b) = ed.buffers.get_mut(&id) {
        b.last_selection = Selection::point(newest);
    }
    ed.switch_to(id);
    ed.messages.doc = Some(id);
    if let Some(old) = old
        && old != id
        && ed
            .buffers
            .get(&old)
            .is_some_and(|b| !b.doc.is_modified() && b.doc.path().is_none())
        && !ed.views.values().any(|v| v.doc == old)
    {
        // Unmodified, so closing cannot fail or lose anything.
        let _ = ed.close_buffer(old, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_utc_hours_minutes_seconds() {
        assert_eq!(utc_clock(0), "00:00:00");
        // 2026-09-08T23:21:26Z
        assert_eq!(utc_clock(1_788_909_686), "23:21:26");
    }

    #[test]
    fn repeats_are_counted_and_the_ring_is_bounded() {
        let mut log = MessageLog::default();
        log.push_at(Severity::Info, "a", 1);
        log.push_at(Severity::Info, "a", 2);
        log.push_at(Severity::Error, "a", 3);
        let got: Vec<_> = log.iter().map(|m| (m.severity, m.count, m.at)).collect();
        assert_eq!(got, [(Severity::Info, 2, 2), (Severity::Error, 1, 3)]);
        for i in 0..LOG_LEN + 5 {
            log.push_at(Severity::Info, &format!("m{i}"), 10);
        }
        assert_eq!(log.len(), LOG_LEN);
        assert_eq!(log.iter().next().unwrap().text, "m5");
    }

    #[test]
    fn render_lines_up_continuation_lines_and_marks_repeats() {
        let mut log = MessageLog::default();
        log.push_at(Severity::Warning, "one\ntwo", 3661);
        log.push_at(Severity::Info, "x", 3662);
        log.push_at(Severity::Info, "x", 3663);
        let (text, newest) = render(&log, usize::MAX);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[2], "01:01:01  warning  one");
        assert_eq!(lines[3], "                   two");
        assert_eq!(lines[4], "01:01:03  info     x (shown 2 times in a row)");
        assert!(text[newest..].starts_with("01:01:03"));
    }

    #[test]
    fn long_messages_wrap_at_spaces_under_their_text_column() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("漢字漢字", 5), ["漢字", "漢字"]);
        assert_eq!(wrap("", 5), [""]);
        let mut log = MessageLog::default();
        let long = format!("{} tail", "word ".repeat(10).trim_end());
        log.push_at(Severity::Error, &long, 0);
        // 19 columns of time and severity, 20 of text.
        let (text, _) = render(&log, 39);
        let lines: Vec<&str> = text.lines().skip(2).collect();
        assert_eq!(lines[0], "00:00:00  error    word word word word");
        assert_eq!(lines[1], "                   word word word word");
        assert_eq!(lines[2], "                   word word tail");
    }
}
