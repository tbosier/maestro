//! Real PTY tests: each one runs a program in a pseudo-terminal and checks
//! what the emulated screen shows. Waits are driven by the `on_output`
//! callback and bounded by a deadline; none sleeps for a fixed time.

#![cfg(unix)]

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use maestro_term::{
    CommandSpec, MAX_QUEUED_INPUT, Screen, TermColor, TermKey, TermMods, TermSize, Terminal,
    encode_key,
};

const TIMEOUT: Duration = Duration::from_secs(10);
const SIZE: TermSize = TermSize { rows: 24, cols: 80 };

/// Counts `on_output` calls so a test can sleep until the next one.
#[derive(Default)]
struct Ticks {
    count: Mutex<u64>,
    changed: Condvar,
}

struct Harness {
    term: Terminal,
    ticks: Arc<Ticks>,
}

fn spec(program: &str, args: &[&str]) -> CommandSpec {
    CommandSpec {
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        env: Vec::new(),
    }
}

fn start_spec(spec: &CommandSpec, size: TermSize) -> Harness {
    let ticks = Arc::new(Ticks::default());
    let notify = Arc::clone(&ticks);
    let term = Terminal::spawn(spec, size, move || {
        *notify.count.lock().unwrap() += 1;
        notify.changed.notify_all();
    })
    .expect("spawn in a PTY");
    Harness { term, ticks }
}

fn sh(script: &str, size: TermSize) -> Harness {
    start_spec(&spec("sh", &["-c", script]), size)
}

fn text(screen: &Screen) -> String {
    (0..screen.rows)
        .map(|r| screen.row_text(r))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Harness {
    /// Waits until `done` holds, checking after every `on_output` call.
    fn wait_for(&self, what: &str, done: impl Fn(&Terminal) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        let mut seen = *self.ticks.count.lock().unwrap();
        loop {
            if done(&self.term) {
                return;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {what}\nexit: {:?}\nscreen:\n{}",
                self.term.exit_status(),
                text(&self.term.snapshot())
            );
            let count = self.ticks.count.lock().unwrap();
            let (count, _) = self
                .ticks
                .changed
                .wait_timeout_while(count, deadline - now, |c| *c == seen)
                .unwrap();
            seen = *count;
        }
    }

    fn wait_for_text(&self, needle: &str) {
        self.wait_for(&format!("{needle:?} on screen"), |t| {
            text(&t.snapshot()).contains(needle)
        });
    }

    fn wait_for_exit(&self) -> maestro_term::ExitInfo {
        self.wait_for("the program to exit", |t| t.exit_status().is_some());
        self.term.exit_status().unwrap()
    }
}

#[test]
fn printed_text_reaches_the_screen() {
    let h = sh("printf 'hello\\nworld'", SIZE);
    let exit = h.wait_for_exit();
    assert_eq!(exit.code, Some(0));
    let screen = h.term.snapshot();
    assert_eq!((screen.rows, screen.cols), (24, 80));
    assert_eq!(screen.cells.len(), 24 * 80);
    assert_eq!(screen.row_text(0), "hello");
    assert_eq!(screen.row_text(1), "world");
    assert_eq!(screen.cursor, Some((1, 5)));
    assert!(!screen.scrolled);
}

#[test]
fn sgr_colors_and_attributes() {
    let h = sh(
        "printf '\\033[31mR\\033[38;5;200mI\\033[38;2;1;2;3mG\\033[0;1;3;4;7mB\\033[0;44mX\\033[0m'",
        SIZE,
    );
    h.wait_for_exit();
    let screen = h.term.snapshot();
    assert_eq!(screen.row_text(0), "RIGBX");
    let cell = |col| screen.cell(0, col).unwrap();
    assert_eq!(cell(0).fg, TermColor::Indexed(1));
    assert_eq!(cell(1).fg, TermColor::Indexed(200));
    assert_eq!(cell(2).fg, TermColor::Rgb(1, 2, 3));
    let b = cell(3);
    assert_eq!(b.fg, TermColor::Default);
    assert!(b.bold && b.italic && b.underline && b.inverse);
    let x = cell(4);
    assert_eq!((x.fg, x.bg), (TermColor::Default, TermColor::Indexed(4)));
    assert!(!x.bold && !x.italic && !x.underline && !x.inverse);
    assert_eq!(cell(5).bg, TermColor::Default);
}

#[test]
fn wide_characters_take_two_cells() {
    let h = sh("printf '中x'", SIZE);
    h.wait_for_exit();
    let screen = h.term.snapshot();
    let left = screen.cell(0, 0).unwrap();
    assert_eq!(left.text, "中");
    assert!(left.wide);
    let right = screen.cell(0, 1).unwrap();
    assert_eq!(right.text, "");
    assert!(!right.wide);
    assert_eq!(screen.cell(0, 2).unwrap().text, "x");
    assert_eq!(screen.cursor, Some((0, 3)));
}

#[test]
fn resize_reaches_the_program() {
    let h = sh("stty size; read line; stty size", SIZE);
    h.wait_for_text("24 80");
    h.term
        .resize(TermSize {
            rows: 30,
            cols: 100,
        })
        .unwrap();
    // Resizing to the same size again is a no-op.
    h.term
        .resize(TermSize {
            rows: 30,
            cols: 100,
        })
        .unwrap();
    h.term.write(b"\r").unwrap();
    h.wait_for_text("30 100");
    let screen = h.term.snapshot();
    assert_eq!((screen.rows, screen.cols), (30, 100));
    assert_eq!(screen.cells.len(), 30 * 100);
    assert_eq!(h.wait_for_exit().code, Some(0));
}

#[test]
fn exit_codes_are_reported() {
    let h = sh("exit 3", SIZE);
    let exit = h.wait_for_exit();
    assert_eq!(exit.code, Some(3));
    assert_eq!(exit.description, "exited with code 3");
    assert_eq!(sh("true", SIZE).wait_for_exit().code, Some(0));
}

#[test]
fn kill_ends_a_running_program() {
    let h = start_spec(&spec("sleep", &["30"]), SIZE);
    assert!(h.term.exit_status().is_none());
    let started = Instant::now();
    h.term.kill().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "kill blocked for {:?}",
        started.elapsed()
    );
    let exit = h.wait_for_exit();
    assert_eq!(exit.code, None, "{exit:?}");
    assert!(
        exit.description.starts_with("terminated by signal"),
        "{exit:?}"
    );
}

#[test]
fn kill_escalates_when_hangup_is_ignored() {
    let h = sh("trap '' HUP; echo ready; while :; do sleep 1; done", SIZE);
    h.wait_for_text("ready");
    h.term.kill().unwrap();
    let exit = h.wait_for_exit();
    assert_eq!(exit.code, None, "{exit:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn drop_returns_at_once_and_kills_the_program() {
    // The pid is printed after the trap, so the shell ignores SIGHUP by then.
    let h = sh("trap '' HUP; echo pid=$$; while :; do sleep 1; done", SIZE);
    h.wait_for("the pid", |t| t.snapshot().row_text(0).starts_with("pid="));
    let pid: u32 = h.term.snapshot().row_text(0)["pid=".len()..]
        .parse()
        .unwrap();
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    assert!(proc_dir.exists());

    let started = Instant::now();
    drop(h);
    // The shell ignores SIGHUP, so a drop that waited for the kill would take
    // at least portable-pty's 200 ms grace period. The 150 ms bound leaves
    // room for a loaded machine.
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "drop blocked for {:?}",
        started.elapsed()
    );

    // Nothing reports on a dropped terminal, so poll the process table.
    let deadline = Instant::now() + TIMEOUT;
    while proc_dir.exists() {
        assert!(
            Instant::now() < deadline,
            "process {pid} still running after drop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn input_echoes_through_cat() {
    let h = start_spec(&spec("cat", &[]), SIZE);
    h.term.write(b"hello").unwrap();
    h.term
        .write(&encode_key(TermKey::Enter, TermMods::default(), false))
        .unwrap();
    // The line discipline echoes the input, then cat prints it back.
    h.wait_for("the echo and cat's copy", |t| {
        let s = t.snapshot();
        s.row_text(0) == "hello" && s.row_text(1) == "hello"
    });
    let ctrl_d = encode_key(
        TermKey::Char('d'),
        TermMods {
            ctrl: true,
            ..TermMods::default()
        },
        false,
    );
    h.term.write(&ctrl_d).unwrap();
    assert_eq!(h.wait_for_exit().code, Some(0));
}

#[test]
fn cwd_and_environment() {
    let mut spec = spec(
        "sh",
        &[
            "-c",
            "pwd -P; printf '%s|%s|%s\\n' \"$MAESTRO_TEST\" \"$TERM\" \"$COLORTERM\"",
        ],
    );
    spec.env.push(("MAESTRO_TEST".into(), "yes".into()));
    let h = start_spec(
        &spec,
        TermSize {
            rows: 24,
            cols: 500,
        },
    );
    h.wait_for_exit();
    let screen = h.term.snapshot();
    let cwd = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    assert_eq!(screen.row_text(0), cwd.to_string_lossy());
    assert_eq!(screen.row_text(1), "yes|xterm-256color|truecolor");
}

#[test]
fn missing_working_directory_is_refused() {
    let mut spec = spec("true", &[]);
    spec.cwd = PathBuf::from("/maestro/no/such/dir");
    let err = Terminal::spawn(&spec, SIZE, || {})
        .err()
        .expect("spawn must fail");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn missing_program_is_an_error() {
    let result = Terminal::spawn(&spec("maestro-no-such-program", &[]), SIZE, || {});
    assert!(result.is_err());
}

#[test]
fn modes_title_and_hidden_cursor() {
    let h = sh(
        "printf '\\033]0;agent one\\007\\033[?1h\\033[?2004h\\033[?25lx'",
        SIZE,
    );
    h.wait_for_exit();
    assert_eq!(h.term.title(), "agent one");
    assert!(h.term.application_cursor());
    assert!(h.term.bracketed_paste());
    assert_eq!(h.term.snapshot().cursor, None);
}

#[test]
fn modes_start_off() {
    let h = sh("printf x", SIZE);
    h.wait_for_exit();
    assert_eq!(h.term.title(), "");
    assert!(!h.term.application_cursor());
    assert!(!h.term.bracketed_paste());
}

#[test]
fn scrollback_view() {
    let h = sh(
        "i=1; while [ $i -le 100 ]; do echo $i; i=$((i+1)); done",
        TermSize { rows: 10, cols: 20 },
    );
    h.wait_for_exit();
    let live = h.term.snapshot();
    assert_eq!(live.row_text(0), "92");
    assert_eq!(live.row_text(8), "100");
    assert!(live.cursor.is_some());

    h.term.scroll(5);
    let back = h.term.snapshot();
    assert!(back.scrolled);
    assert_eq!(back.cursor, None);
    assert_eq!(back.row_text(0), "87");

    h.term.scroll(-2);
    assert_eq!(h.term.snapshot().row_text(0), "89");

    h.term.scroll(isize::MAX);
    assert_eq!(h.term.snapshot().row_text(0), "1");

    h.term.scroll(isize::MIN);
    let bottom = h.term.snapshot();
    assert!(!bottom.scrolled);
    assert_eq!(bottom.row_text(0), "92");

    h.term.scroll(3);
    h.term.scroll(0);
    assert_eq!(h.term.snapshot(), live);
}

#[test]
fn cursor_position_queries_are_answered() {
    // The program turns off echo, asks where the cursor is, and prints the
    // reply with ESC shown as E.
    let script = "stty -echo -icanon min 1; printf 'ab\\033[6n'; \
                  r=$(dd bs=1 count=6 2>/dev/null); printf '\\n%s' \"$r\" | tr '\\033' E";
    let h = sh(script, SIZE);
    h.wait_for_text("E[1;3R");
    assert_eq!(h.wait_for_exit().code, Some(0));
}

#[test]
fn write_does_not_block_when_the_program_stops_reading() {
    // Non-canonical mode: the line discipline keeps every byte, so its 4 KiB
    // input buffer fills and the writer thread blocks on the PTY.
    let h = sh("stty -icanon -echo; echo ready; sleep 30", SIZE);
    h.wait_for_text("ready");
    let chunk = vec![b'a'; 1 << 20];
    let started = Instant::now();
    let mut accepted = 0usize;
    let err = loop {
        match h.term.write(&chunk) {
            Ok(()) => accepted += chunk.len(),
            Err(err) => break err,
        }
        assert!(
            accepted <= 2 * MAX_QUEUED_INPUT,
            "the input queue has no bound"
        );
    };
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "write blocked for {:?}",
        started.elapsed()
    );
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    // Only what the PTY buffered left the queue: a few KiB, far under 1 MiB.
    assert!(
        accepted >= MAX_QUEUED_INPUT - chunk.len(),
        "accepted {accepted}"
    );
    assert!(
        accepted <= MAX_QUEUED_INPUT + chunk.len(),
        "accepted {accepted}"
    );
    h.term.kill().unwrap();
    assert_eq!(h.wait_for_exit().code, None);
}

#[test]
fn writing_after_exit_fails() {
    let h = sh("true", SIZE);
    h.wait_for_exit();
    assert_eq!(
        h.term.write(b"x").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}
