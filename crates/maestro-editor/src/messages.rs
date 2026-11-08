//! The message log. Every status message set through
//! [`Editor::set_status`] (and `info`, `error` and `add_status`, which call
//! it or share its logging) is kept here with its severity and time, so a
//! message the status line cut short, or a later one replaced, can be read
//! again with `:messages`.

use std::collections::VecDeque;

use jiff::Timestamp;
use jiff::tz::TimeZone;
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
    /// Buffers `:messages` opened that are still open. The next `:messages`
    /// closes the ones nothing shows and nobody edited.
    docs: Vec<DocumentId>,
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

    /// Takes back the newest record of `text`, for a message that was set
    /// only to be captured and shown as part of a later one.
    pub(crate) fn unrecord(&mut self, severity: Severity, text: &str) {
        if let Some(last) = self.entries.back_mut()
            && last.severity == severity
            && last.text == text
        {
            if last.count > 1 {
                last.count -= 1;
            } else {
                self.entries.pop_back();
            }
        }
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

/// The time zone `:messages` shows times in.
#[derive(Debug, Clone)]
pub struct Zone {
    tz: TimeZone,
    /// False when the local zone could not be read and this is UTC in its
    /// place.
    read: bool,
}

impl Zone {
    /// The local zone: `TZ` when set, else `/etc/localtime` (the system
    /// setting on Windows). UTC when neither can be read, for example an
    /// unknown `TZ` name.
    pub fn local() -> Zone {
        match TimeZone::try_system() {
            Ok(tz) => Zone { tz, read: true },
            Err(_) => Zone {
                tz: TimeZone::UTC,
                read: false,
            },
        }
    }

    /// Times in `tz`.
    pub fn new(tz: TimeZone) -> Zone {
        Zone { tz, read: true }
    }

    /// `HH:MM:SS` in this zone for a Unix time, with the offset in effect
    /// at that time (so a log that crosses a daylight saving change shows
    /// each message's own wall clock).
    fn clock(&self, secs: i64) -> String {
        let offset = i64::from(self.tz.to_offset(timestamp(secs)).seconds());
        utc_clock(secs.saturating_add(offset))
    }

    /// The zone in words for the header: `UTC`, the zone's name
    /// (`America/New_York`), or for a zone without a name (a POSIX `TZ`
    /// rule, a copied `/etc/localtime`) the abbreviation and offset in
    /// effect at each of `times`, each kind once (`EDT, UTC-04; EST,
    /// UTC-05` for a log that crosses a daylight saving change).
    fn describe(&self, times: impl Iterator<Item = i64>) -> String {
        if !self.read {
            return "UTC (the local time zone could not be read)".into();
        }
        match self.tz.iana_name() {
            _ if self.tz == TimeZone::UTC => return "UTC".into(),
            Some("Etc/UTC" | "Etc/UCT" | "Etc/Universal" | "Etc/Zulu") => return "UTC".into(),
            Some(name) => return name.to_string(),
            None => {}
        }
        let mut seen: Vec<String> = Vec::new();
        for at in times {
            let info = self.tz.to_offset_info(timestamp(at));
            let offset = format!("UTC{}", info.offset());
            let abbr = info.abbreviation();
            let shown = if abbr.is_empty() || abbr.starts_with(['+', '-']) {
                offset
            } else {
                format!("{abbr}, {offset}")
            };
            if !seen.contains(&shown) {
                seen.push(shown);
            }
        }
        seen.join("; ")
    }
}

/// `secs` as a jiff timestamp, held to the range jiff supports (years -9999
/// to 9999), so a time outside it still gets the zone's offset at the
/// nearest end instead of none.
fn timestamp(secs: i64) -> Timestamp {
    let secs = secs.clamp(Timestamp::MIN.as_second(), Timestamp::MAX.as_second());
    Timestamp::from_second(secs).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// `line` with each tab replaced by spaces up to the next multiple of
/// `tab_width`, so the log holds no tabs whose width depends on where they
/// are drawn.
fn expand_tabs(line: &str, tab_width: usize) -> String {
    let tab_width = tab_width.max(1);
    let mut out = String::new();
    let mut col = 0;
    for g in maestro_core::text::graphemes(maestro_core::RopeSlice::from(line)) {
        if g == "\t" {
            let n = tab_width - col % tab_width;
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            col += maestro_core::text::grapheme_width(&g, col, tab_width);
            out.push_str(&g);
        }
    }
    out
}

/// Splits `line` (with no tabs; see [`expand_tabs`]) into pieces at most
/// `width` columns wide, breaking after a space where one is in reach and
/// inside a word otherwise. Columns are counted as the editor draws text:
/// wide characters take two, control characters two (`^X`).
fn wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    // Byte offset in `cur` just after its last space, and the width there.
    let mut last_space: Option<(usize, usize)> = None;
    for g in maestro_core::text::graphemes(maestro_core::RopeSlice::from(line)) {
        let w = maestro_core::text::grapheme_width(&g, cur_w, 1);
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

/// Narrowest text column worth keeping beside the time and severity; below
/// it each message starts on the line after them.
const MIN_TEXT_W: usize = 20;

/// The log as text, oldest first, and the char offset of the newest
/// message's first line (where the cursor goes). Lines are wrapped to
/// `width` columns (pass `usize::MAX` for none) and tabs expanded to
/// `tab_width`. Where there is room the text sits right of the time and
/// severity, continuation lines indented under it; in a narrow view each
/// message starts on the line after them, indented two columns.
pub fn render(log: &MessageLog, width: usize, tab_width: usize, zone: &Zone) -> (String, usize) {
    let width = width.max(1);
    let mut text = String::new();
    let times: Vec<i64> = if log.is_empty() {
        vec![now_unix()]
    } else {
        log.iter().map(|m| m.at).collect()
    };
    let header = format!(
        "Status messages, oldest first. Times are {}. The last {LOG_LEN} are kept.",
        zone.describe(times.into_iter())
    );
    for piece in wrap(&header, width) {
        text.push_str(&piece);
        text.push('\n');
    }
    text.push('\n');
    if log.is_empty() {
        text.push_str("No messages yet.\n");
        return (text, 0);
    }
    // Width of the widest severity word, so the text column lines up.
    let word_w = "warning".len();
    let beside = "00:00:00".len() + 2 + word_w + 2;
    let narrow = width < beside + MIN_TEXT_W;
    let indent = if narrow { 2 } else { beside };
    let text_w = width.saturating_sub(indent).max(1);
    let pad = " ".repeat(indent);
    let mut newest = 0;
    for m in log.iter() {
        newest = text.len();
        let mut body = m.text.clone();
        if m.count > 1 {
            body.push_str(&format!(" (shown {} times in a row)", m.count));
        }
        let head = format!("{}  {:<word_w$}", zone.clock(m.at), m.severity.label());
        if narrow {
            text.push_str(head.trim_end());
            text.push('\n');
        }
        let pieces = body
            .lines()
            .flat_map(|l| wrap(&expand_tabs(l, tab_width), text_w));
        for (i, piece) in pieces.enumerate() {
            let line = if i == 0 && !narrow {
                format!("{head}  {piece}")
            } else {
                format!("{pad}{piece}")
            };
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    let newest = text[..newest].chars().count();
    (text, newest)
}

/// `:messages`: opens the log in a scratch buffer with the cursor on the
/// newest message. Buffers earlier `:messages` opened are closed when no
/// view shows them and nobody edited them.
pub fn show(ed: &mut Editor) {
    // Room for text in the focused view, less a gutter wide enough for
    // five-digit line numbers (the widest a log of 200 wrapped messages
    // plausibly needs), so wrapped lines need no sideways scrolling.
    let width = (ed.view().area.width as usize).saturating_sub(8);
    let (text, newest) = render(&ed.messages, width, ed.tab_width(), &Zone::local());
    let id = ed.add_document(Document::from_text(&text));
    if let Some(b) = ed.buffers.get_mut(&id) {
        b.last_selection = Selection::point(newest);
    }
    ed.switch_to(id);
    let old = std::mem::take(&mut ed.messages.docs);
    for doc in old {
        let Some(b) = ed.buffers.get(&doc) else {
            continue;
        };
        let untouched = !b.doc.is_modified() && b.doc.path().is_none();
        if untouched && !ed.views.values().any(|v| v.doc == doc) {
            // Unmodified, so closing cannot fail or lose anything.
            let _ = ed.close_buffer(doc, false);
        } else if b.doc.path().is_none() {
            // Still shown or edited: tried again next time.
            ed.messages.docs.push(doc);
        }
    }
    ed.messages.docs.push(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> Zone {
        Zone::new(TimeZone::UTC)
    }

    /// The header line and the first message line of a one-message log.
    fn rendered(zone: &Zone, at: i64) -> (String, String) {
        let mut log = MessageLog::default();
        log.push_at(Severity::Info, "m", at);
        let (text, _) = render(&log, usize::MAX, 4, zone);
        let lines: Vec<&str> = text.lines().collect();
        (lines[0].to_string(), lines[2].to_string())
    }

    // 2026-09-08T23:21:26Z, daylight saving time in New York.
    const SUMMER: i64 = 1_788_909_686;
    // 2026-01-15T12:00:00Z, standard time in New York.
    const WINTER: i64 = 1_768_478_400;
    // 2026-11-01T05:59:59Z and one second later: New York's clocks go from
    // 01:59:59 EDT back to 01:00:00 EST.
    const FALL_BACK: i64 = 1_793_512_799;

    /// A TZif file (RFC 8536, version 2) with no transitions, one local
    /// time type and `rule` as its footer, the way zones are stored on
    /// disk. Built here so the tests do not depend on the machine's
    /// zoneinfo files.
    pub(super) fn tzif(utoff: i32, abbr: &str, rule: &str) -> Vec<u8> {
        let chars = format!("{abbr}\0");
        let header = |out: &mut Vec<u8>| {
            out.extend_from_slice(b"TZif2");
            out.extend_from_slice(&[0; 15]);
            // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
            for n in [0u32, 0, 0, 0, 1, chars.len() as u32] {
                out.extend_from_slice(&n.to_be_bytes());
            }
        };
        let block = |out: &mut Vec<u8>| {
            out.extend_from_slice(&utoff.to_be_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(chars.as_bytes());
        };
        let mut out = Vec::new();
        header(&mut out);
        block(&mut out);
        header(&mut out);
        block(&mut out);
        out.extend_from_slice(format!("\n{rule}\n").as_bytes());
        out
    }

    fn new_york() -> TimeZone {
        let data = tzif(-5 * 3600, "EST", "EST5EDT,M3.2.0,M11.1.0");
        TimeZone::tzif("America/New_York", &data).unwrap()
    }

    #[test]
    fn a_named_zone_shows_local_times_and_its_name() {
        let zone = Zone::new(new_york());
        let (header, line) = rendered(&zone, SUMMER);
        assert_eq!(
            header,
            "Status messages, oldest first. Times are America/New_York. The last 200 are kept."
        );
        assert_eq!(line, "19:21:26  info     m");
        assert_eq!(rendered(&zone, WINTER).1, "07:00:00  info     m");
        // Across the change each message keeps its own wall clock.
        let mut log = MessageLog::default();
        log.push_at(Severity::Info, "before", FALL_BACK);
        log.push_at(Severity::Info, "after", FALL_BACK + 1);
        let (text, _) = render(&log, usize::MAX, 4, &zone);
        assert!(
            text.contains("01:59:59  info     before\n01:00:00  info     after"),
            "{text}"
        );
    }

    #[test]
    fn a_zone_without_a_name_shows_its_abbreviation_and_offset() {
        let zone = Zone::new(TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap());
        let (header, line) = rendered(&zone, SUMMER);
        assert!(header.contains("Times are EDT, UTC-04."), "{header}");
        assert_eq!(line, "19:21:26  info     m");
        // A log on both sides of a change names both.
        let mut log = MessageLog::default();
        log.push_at(Severity::Info, "before", FALL_BACK);
        log.push_at(Severity::Info, "after", FALL_BACK + 1);
        let (text, _) = render(&log, usize::MAX, 4, &zone);
        assert!(
            text.starts_with(
                "Status messages, oldest first. Times are EDT, UTC-04; EST, UTC-05. The last"
            ),
            "{text}"
        );
        // Times past the range jiff supports still get an offset.
        let (_, line) = rendered(&zone, i64::MAX);
        assert_ne!(line, rendered(&utc(), i64::MAX).1);
        let fixed = Zone::new(TimeZone::fixed(jiff::tz::offset(5)));
        let (header, line) = rendered(&fixed, SUMMER);
        assert!(header.contains("Times are UTC+05."), "{header}");
        assert_eq!(line, "04:21:26  info     m");
    }

    #[test]
    fn utc_says_so_and_an_unreadable_zone_says_why() {
        let (header, line) = rendered(&utc(), SUMMER);
        assert!(header.contains("Times are UTC. "), "{header}");
        assert_eq!(line, "23:21:26  info     m");
        let etc = Zone::new(TimeZone::tzif("Etc/UTC", &tzif(0, "UTC", "UTC0")).unwrap());
        assert!(rendered(&etc, SUMMER).0.contains("Times are UTC. "));
        let unread = Zone {
            tz: TimeZone::UTC,
            read: false,
        };
        let (header, line) = rendered(&unread, SUMMER);
        assert!(
            header.contains("Times are UTC (the local time zone could not be read)."),
            "{header}"
        );
        assert_eq!(line, "23:21:26  info     m");
    }

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
        let (text, newest) = render(&log, usize::MAX, 4, &utc());
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
        let (text, _) = render(&log, 39, 4, &utc());
        // The header wraps too; messages start after the blank line.
        let body = text.split("\n\n").nth(1).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[0], "00:00:00  error    word word word word");
        assert_eq!(lines[1], "                   word word word word");
        assert_eq!(lines[2], "                   word word tail");
    }

    #[test]
    fn unrecord_takes_back_only_the_matching_newest_message() {
        let mut log = MessageLog::default();
        log.push_at(Severity::Info, "keep", 1);
        log.push_at(Severity::Warning, "w", 2);
        log.push_at(Severity::Warning, "w", 3);
        log.unrecord(Severity::Warning, "w");
        assert_eq!(log.iter().next_back().unwrap().count, 1);
        log.unrecord(Severity::Warning, "w");
        log.unrecord(Severity::Warning, "keep");
        let left: Vec<_> = log.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(left, ["keep"]);
    }

    #[test]
    fn a_narrow_width_puts_the_text_under_the_time() {
        let mut log = MessageLog::default();
        log.push_at(Severity::Error, "one two three", 0);
        let (text, newest) = render(&log, 24, 4, &utc());
        assert!(text.lines().all(|l| l.chars().count() <= 24), "{text}");
        assert!(text[newest..].starts_with("00:00:00  error\n  one two three\n"));
    }
}
