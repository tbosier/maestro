//! Pseudo-terminals and terminal emulation. See `docs/decisions/0006-agent-runtime.md`.
//!
//! A [`Terminal`] runs one program in a PTY. Three plain threads serve it:
//!
//! - the reader reads the PTY master, feeds the bytes to a `vt100` parser and
//!   calls `on_output` after each chunk;
//! - the writer sends input (keys, pastes and the emulator's own replies to
//!   status queries) to the PTY, so a program that stops reading its input
//!   never blocks the caller;
//! - the waiter owns the child process, records how it exited, and kills it
//!   when asked or when the `Terminal` is dropped.
//!
//! No call on a `Terminal`, including drop, waits for the child or for IO.

mod keys;

pub use keys::{TermKey, TermMods, encode_key, encode_paste};

use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// portable-pty reports `anyhow` errors. This keeps the IO error kind when
/// there is one. A macro because the `anyhow` type is not nameable here.
macro_rules! pty_error {
    () => {
        |err| match err.downcast::<io::Error>() {
            Ok(io) => io,
            Err(other) => io::Error::other(format!("{other:#}")),
        }
    };
}

/// Lines of scrollback each terminal keeps.
pub const SCROLLBACK_LINES: usize = 3000;

/// How long the waiter lets the reader drain the last output after the child
/// exits before it reports the exit anyway. A background process that keeps
/// the PTY open would otherwise hold the exit back forever.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// The waiter checks on the child at this interval at first, backing off to
/// `MAX_POLL` while it keeps running.
const MIN_POLL: Duration = Duration::from_millis(2);
const MAX_POLL: Duration = Duration::from_millis(100);
/// The longest window title kept, in characters.
const MAX_TITLE: usize = 256;

/// A program to run in a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// The program, found on `PATH` when it has no slash.
    pub program: String,
    pub args: Vec<String>,
    /// The working directory. It must exist: `spawn` refuses a missing one
    /// rather than starting the program somewhere else.
    pub cwd: PathBuf,
    /// Added to the inherited environment, after `TERM` and `COLORTERM`, so
    /// these entries win.
    pub env: Vec<(String, String)>,
}

/// A terminal size in character cells. Zero is treated as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
}

/// A copy of what the terminal shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    pub rows: u16,
    pub cols: u16,
    /// `rows * cols` cells, row by row.
    pub cells: Vec<Cell>,
    /// `(row, col)` of the cursor, or None when the program hid it or the view
    /// is scrolled back.
    pub cursor: Option<(u16, u16)>,
    /// True when the view shows scrollback instead of the live screen.
    pub scrolled: bool,
}

impl Screen {
    /// The cell at `(row, col)`, or None outside the screen.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells
            .get(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }

    /// The text of one row with trailing blanks removed. Empty outside the screen.
    pub fn row_text(&self, row: u16) -> String {
        if row >= self.rows {
            return String::new();
        }
        let start = usize::from(row) * usize::from(self.cols);
        let cells = &self.cells[start..start + usize::from(self.cols)];
        let text: String = cells.iter().map(|c| c.text.as_str()).collect();
        text.trim_end().to_string()
    }
}

/// One character cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The cell's grapheme, `" "` for a blank cell, and empty for the right
    /// half of a wide character.
    pub text: String,
    pub fg: TermColor,
    pub bg: TermColor,
    pub bold: bool,
    /// Faint text (SGR 2).
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// True on the left half of a wide character. The right half is the next
    /// cell, with empty text.
    pub wide: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            text: " ".to_string(),
            fg: TermColor::Default,
            bg: TermColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            wide: false,
        }
    }
}

/// A cell color as the program set it. The UI maps `Default` and the 16 base
/// indexed colors to its theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TermColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// How the program ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    /// The exit code, or None when a signal ended the program.
    pub code: Option<i32>,
    /// For people: "exited with code 3", "terminated by signal: Killed".
    pub description: String,
}

/// A program running in a pseudo-terminal. Send + Sync, cheap to share behind Arc.
pub struct Terminal {
    shared: Arc<Shared>,
    input: Sender<Vec<u8>>,
}

struct Shared {
    parser: Mutex<vt100::Parser<Events>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    exit: Mutex<Option<ExitInfo>>,
    control: Mutex<Control>,
    control_changed: Condvar,
    on_output: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct Control {
    kill: bool,
    reader_done: bool,
    dropped: bool,
}

impl Shared {
    /// Tells the UI something changed, unless the `Terminal` is gone.
    fn changed(&self) {
        let dropped = lock(&self.control).dropped;
        if !dropped {
            (self.on_output)();
        }
    }
}

impl Terminal {
    /// Spawns `spec` in a new PTY. `on_output` runs on the reader thread after
    /// each chunk is parsed, and once more on the waiter thread after the
    /// program exits. It is not called after the `Terminal` is dropped.
    pub fn spawn(
        spec: &CommandSpec,
        size: TermSize,
        on_output: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Terminal> {
        if !spec.cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "working directory {} is not a directory",
                    spec.cwd.display()
                ),
            ));
        }
        let (rows, cols) = dimensions(size);
        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .map_err(pty_error!())?;
        let reader = pair.master.try_clone_reader().map_err(pty_error!())?;
        let writer = pair.master.take_writer().map_err(pty_error!())?;

        let shared = Arc::new(Shared {
            parser: Mutex::new(vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_LINES,
                Events::default(),
            )),
            master: Mutex::new(pair.master),
            exit: Mutex::new(None),
            control: Mutex::new(Control::default()),
            control_changed: Condvar::new(),
            on_output: Box::new(on_output),
        });

        // The reader and writer start before the child so that a failure here
        // leaves no process behind. If the spawn below fails, dropping the
        // slave ends the reader (EIO) and dropping `input` ends the writer.
        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("maestro-term-writer".into())
            .spawn(move || write_loop(writer, input_rx))?;
        let replies = input.clone();
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("maestro-term-reader".into())
            .spawn(move || read_loop(&reader_shared, reader, &replies))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        let child = pair.slave.spawn_command(cmd).map_err(pty_error!())?;
        // The parent's copy of the slave must close, or the reader never sees
        // the end of the output when the child exits.
        drop(pair.slave);

        let mut killer = child.clone_killer();
        let waiter_shared = Arc::clone(&shared);
        if let Err(err) = thread::Builder::new()
            .name("maestro-term-wait".into())
            .spawn(move || wait_loop(&waiter_shared, child))
        {
            let _ = killer.kill();
            return Err(err);
        }

        Ok(Terminal { shared, input })
    }

    /// Queues bytes for the program's input. Never blocks on the PTY. Fails
    /// with `BrokenPipe` once the program has exited (Linux would otherwise
    /// accept and discard the bytes) or the PTY stopped accepting input.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        if self.exit_status().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the program has exited",
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.input.send(bytes.to_vec()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the terminal no longer accepts input",
            )
        })
    }

    /// Resizes the PTY (the program gets SIGWINCH) and the emulated screen.
    /// Does nothing when the size is unchanged.
    pub fn resize(&self, size: TermSize) -> io::Result<()> {
        let (rows, cols) = dimensions(size);
        // Hold the parser while the PTY changes size, so output the program
        // draws for the new size is never parsed at the old one.
        let mut parser = lock(&self.shared.parser);
        if parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        lock(&self.shared.master)
            .resize(pty_size(rows, cols))
            .map_err(pty_error!())?;
        parser.set_size(rows, cols);
        Ok(())
    }

    /// The visible screen, or a scrolled-back view.
    pub fn snapshot(&self) -> Screen {
        screen_from(lock(&self.shared.parser).screen())
    }

    /// Moves the view `lines` further into the scrollback (negative moves back
    /// toward the live screen), clamped to what exists. 0 returns to the live
    /// screen. While scrolled back the view stays on the same lines as new
    /// output arrives.
    pub fn scroll(&self, lines: isize) {
        let mut parser = lock(&self.shared.parser);
        let screen = parser.screen_mut();
        let offset = match lines {
            0 => 0,
            1.. => screen.scrollback().saturating_add(lines.unsigned_abs()),
            _ => screen.scrollback().saturating_sub(lines.unsigned_abs()),
        };
        screen.set_scrollback(offset);
    }

    /// None while the program runs.
    pub fn exit_status(&self) -> Option<ExitInfo> {
        lock(&self.shared.exit).clone()
    }

    /// Asks the waiter thread to end the program: SIGHUP, then SIGKILL if it
    /// is still running a moment later. Returns at once; `exit_status` and
    /// `on_output` report the exit.
    pub fn kill(&self) -> io::Result<()> {
        lock(&self.shared.control).kill = true;
        self.shared.control_changed.notify_all();
        Ok(())
    }

    /// Whether the program turned on application cursor mode (DECCKM), for
    /// [`encode_key`].
    pub fn application_cursor(&self) -> bool {
        lock(&self.shared.parser).screen().application_cursor()
    }

    /// Whether the program turned on bracketed paste, for [`encode_paste`].
    pub fn bracketed_paste(&self) -> bool {
        lock(&self.shared.parser).screen().bracketed_paste()
    }

    /// The window title the program set with OSC 0 or OSC 2, without control
    /// characters. Empty until it sets one.
    pub fn title(&self) -> String {
        lock(&self.shared.parser).callbacks().title.clone()
    }
}

impl Drop for Terminal {
    /// Kills the program without waiting for it. The threads end on their own
    /// once it is gone.
    fn drop(&mut self) {
        let mut control = lock(&self.shared.control);
        control.dropped = true;
        control.kill = true;
        drop(control);
        self.shared.control_changed.notify_all();
    }
}

fn read_loop(shared: &Shared, mut reader: Box<dyn Read + Send>, replies: &Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let answer = {
                    let mut parser = lock(&shared.parser);
                    parser.process(&buf[..n]);
                    std::mem::take(&mut parser.callbacks_mut().replies)
                };
                if !answer.is_empty() {
                    let _ = replies.send(answer);
                }
                shared.changed();
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            // Linux reports EIO once every slave descriptor is closed.
            Err(_) => break,
        }
    }
    lock(&shared.control).reader_done = true;
    shared.control_changed.notify_all();
}

fn write_loop(mut writer: Box<dyn Write + Send>, input: Receiver<Vec<u8>>) {
    while let Ok(bytes) = input.recv() {
        if writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

fn wait_loop(shared: &Shared, mut child: Box<dyn Child + Send + Sync>) {
    let info = match wait_for_child(shared, child.as_mut()) {
        Ok(status) => exit_info(&status),
        Err(err) => ExitInfo {
            code: None,
            description: format!("could not wait for the program: {err}"),
        },
    };
    // Give the reader a moment to parse the last output, so the screen is
    // complete when the exit is reported.
    let control = lock(&shared.control);
    let _ = shared
        .control_changed
        .wait_timeout_while(control, DRAIN_TIMEOUT, |c| !c.reader_done)
        .unwrap_or_else(PoisonError::into_inner);
    *lock(&shared.exit) = Some(info);
    shared.changed();
}

/// Polls the child with backoff and wakes early when `kill` is asked for or
/// the reader sees the PTY close, which usually means the child just exited.
fn wait_for_child(
    shared: &Shared,
    child: &mut (dyn Child + Send + Sync),
) -> io::Result<portable_pty::ExitStatus> {
    let mut delay = MIN_POLL;
    let mut kill_sent = false;
    let mut saw_reader_done = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let control = lock(&shared.control);
        if control.kill && !kill_sent {
            drop(control);
            kill_sent = true;
            // portable-pty sends SIGHUP, gives the child up to 200 ms, then
            // sends SIGKILL. This blocks only this thread.
            let _ = child.kill();
            delay = MIN_POLL;
            continue;
        }
        if control.reader_done && !saw_reader_done {
            saw_reader_done = true;
            delay = MIN_POLL;
        }
        let (control, _) = shared
            .control_changed
            .wait_timeout(control, delay)
            .unwrap_or_else(PoisonError::into_inner);
        drop(control);
        delay = (delay * 2).min(MAX_POLL);
    }
}

fn exit_info(status: &portable_pty::ExitStatus) -> ExitInfo {
    match status.signal() {
        Some(signal) => ExitInfo {
            code: None,
            description: format!("terminated by signal: {signal}"),
        },
        None => {
            let code = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
            ExitInfo {
                code: Some(code),
                description: format!("exited with code {code}"),
            }
        }
    }
}

/// vt100 callbacks: the window title and replies to status queries.
#[derive(Default)]
struct Events {
    title: String,
    /// Bytes to send back to the program, taken after each chunk.
    replies: Vec<u8>,
}

impl vt100::Callbacks for Events {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = clean_title(title);
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // vt100 splits OSC parameters on ';', so a title containing one
        // arrives here instead of in `set_window_title`.
        if let [b"0" | b"2", rest @ ..] = params
            && !rest.is_empty()
        {
            self.title = clean_title(&rest.join(&b';'));
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Programs that ask where the cursor is or what the terminal is often
        // wait for the answer, some for seconds, before drawing anything.
        let param = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (first, second, c, param) {
            // DSR: device status.
            (None, None, 'n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            // DSR: cursor position report, 1-based.
            (None, None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                let (_, cols) = screen.size();
                let col = col.min(cols.get() - 1);
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // Primary device attributes: a VT100 with advanced video.
            (None, None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            // Secondary device attributes.
            (Some(b'>'), None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            _ => {}
        }
    }
}

fn clean_title(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE)
        .collect()
}

fn screen_from(screen: &vt100::Screen) -> Screen {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let scrolled = screen.scrollback() > 0;
    let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    for row in 0..rows {
        for col in 0..cols {
            cells.push(
                screen
                    .cell(row, col)
                    .map_or_else(Cell::default, convert_cell),
            );
        }
    }
    let cursor = if scrolled || screen.hide_cursor() {
        None
    } else {
        let (row, col) = screen.cursor_position();
        Some((row.min(rows - 1), col.min(cols - 1)))
    };
    Screen {
        rows,
        cols,
        cells,
        cursor,
        scrolled,
    }
}

fn convert_cell(cell: &vt100::Cell) -> Cell {
    let text = if cell.is_wide_continuation() {
        String::new()
    } else if cell.has_contents() {
        cell.contents().to_string()
    } else {
        " ".to_string()
    };
    Cell {
        text,
        fg: convert_color(cell.fgcolor()),
        bg: convert_color(cell.bgcolor()),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
        wide: cell.is_wide(),
    }
}

fn convert_color(color: vt100::Color) -> TermColor {
    match color {
        vt100::Color::Default => TermColor::Default,
        vt100::Color::Idx(i) => TermColor::Indexed(i),
        vt100::Color::Rgb(r, g, b) => TermColor::Rgb(r, g, b),
    }
}

fn dimensions(size: TermSize) -> (NonZeroU16, NonZeroU16) {
    (
        NonZeroU16::new(size.rows).unwrap_or(NonZeroU16::MIN),
        NonZeroU16::new(size.cols).unwrap_or(NonZeroU16::MIN),
    )
}

fn pty_size(rows: NonZeroU16, cols: NonZeroU16) -> PtySize {
    PtySize {
        rows: rows.get(),
        cols: cols.get(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser<Events> {
        let (rows, cols) = dimensions(TermSize { rows, cols });
        let mut parser = vt100::Parser::new_with_callbacks(rows, cols, 100, Events::default());
        parser.process(bytes);
        parser
    }

    #[test]
    fn terminal_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Terminal>();
    }

    #[test]
    fn zero_sizes_become_one() {
        let (rows, cols) = dimensions(TermSize { rows: 0, cols: 0 });
        assert_eq!((rows.get(), cols.get()), (1, 1));
    }

    #[test]
    fn titles_with_semicolons_and_control_characters() {
        let parser = parse(5, 20, b"\x1b]2;build; test\x07");
        assert_eq!(parser.callbacks().title, "build; test");
        let parser = parse(5, 20, b"\x1b]0;a\x01b\x07");
        assert_eq!(parser.callbacks().title, "ab");
    }

    #[test]
    fn status_queries_are_answered() {
        let mut parser = parse(5, 20, b"ab\r\nc\x1b[6n\x1b[5n\x1b[c\x1b[>c");
        assert_eq!(
            std::mem::take(&mut parser.callbacks_mut().replies),
            b"\x1b[2;2R\x1b[0n\x1b[?1;2c\x1b[>0;0;0c"
        );
    }

    #[test]
    fn blank_wide_and_colored_cells() {
        let parser = parse(2, 6, "中\x1b[31;2mx".as_bytes());
        let screen = screen_from(parser.screen());
        assert_eq!(screen.cells.len(), 12);
        let wide = screen.cell(0, 0).unwrap();
        assert!(wide.wide);
        assert_eq!(wide.text, "中");
        assert_eq!(screen.cell(0, 1).unwrap().text, "");
        let x = screen.cell(0, 2).unwrap();
        assert_eq!(
            (x.text.as_str(), x.fg, x.dim),
            ("x", TermColor::Indexed(1), true)
        );
        assert_eq!(screen.cell(1, 5).unwrap(), &Cell::default());
        assert_eq!(screen.row_text(0), "中x");
        assert_eq!(screen.cursor, Some((0, 3)));
    }

    /// Times `snapshot` on a full 200x50 screen. Run with
    /// `cargo test --release -p maestro-term -- --ignored --nocapture snapshot_timing`.
    #[test]
    #[ignore = "timing measurement, not a check"]
    fn snapshot_timing() {
        let mut bytes = Vec::new();
        for row in 0..50 {
            for col in 0..100 {
                bytes.extend_from_slice(
                    format!("\x1b[38;5;{}m", (row * 100 + col) % 256).as_bytes(),
                );
                bytes.extend_from_slice(if col % 10 == 0 {
                    "中".as_bytes()
                } else {
                    b"ab"
                });
            }
            if row < 49 {
                bytes.extend_from_slice(b"\r\n");
            }
        }
        let parser = parse(50, 200, &bytes);
        let runs = 2000;
        let start = std::time::Instant::now();
        let mut cells = 0;
        for _ in 0..runs {
            cells += std::hint::black_box(screen_from(parser.screen()))
                .cells
                .len();
        }
        let per = start.elapsed() / runs;
        assert_eq!(cells, 10_000 * runs as usize);
        println!("snapshot of 200x50: {per:?} per call over {runs} calls");
    }
}
