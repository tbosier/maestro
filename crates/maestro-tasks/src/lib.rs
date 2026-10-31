//! Verification commands and their results. See `docs/decisions/0006-agent-runtime.md`.
//!
//! [`start`] runs a command (a test suite, a build) in a directory on plain
//! threads and reports each output line and the exit status through a
//! callback. [`parse_cargo_test`] summarizes `cargo test` output.

use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// A command to run: no shell is involved, `args` go to the program as is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

/// What a running task reports, in order: any number of `Output` lines,
/// then exactly one `Finished`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// One line from stdout or stderr, without its line ending. Lines longer
    /// than [`MAX_LINE`] bytes arrive in pieces. Invalid UTF-8 is replaced.
    Output(String),
    /// The program exited. `code` is `None` when a signal ended it (for
    /// example after [`TaskHandle::kill`]).
    Finished {
        code: Option<i32>,
        duration: Duration,
    },
}

/// Longest piece of output delivered as one line.
pub const MAX_LINE: usize = 64 * 1024;

/// After the program exits, how long to wait for its output pipes to close.
/// A background process it started can hold them open indefinitely; output
/// that arrives after this is dropped so `Finished` is not held back.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// How often the event thread checks whether the program has exited when
/// no output is arriving.
const POLL: Duration = Duration::from_millis(20);

struct Shared {
    child: Child,
    /// Set once the child has been reaped. After that its process id (and
    /// process group id) may belong to someone else, so `kill` must not use
    /// it. Both are only read and written under the lock.
    reaped: bool,
}

/// A running task. Dropping the handle does not stop the task.
pub struct TaskHandle {
    pid: u32,
    shared: Arc<Mutex<Shared>>,
}

impl TaskHandle {
    /// Stops the task and everything it started that stayed in its process
    /// group (on Unix, the task runs in a group of its own; `cargo test`
    /// and the test binaries it spawns are all in it). Does nothing once the
    /// task has finished. `Finished` still arrives, with `code: None`.
    pub fn kill(&self) {
        let mut s = lock(&self.shared);
        if s.reaped {
            return;
        }
        // The child has not been reaped, so its pid, which is also the
        // group id, cannot have been reused while we hold the lock.
        #[cfg(unix)]
        kill_group(self.pid);
        let _ = s.child.kill();
    }

    /// The operating system's process id (on Unix, also the process group).
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// Sends SIGKILL to the process group `pgid`.
///
/// The standard library can put a child in its own group
/// (`CommandExt::process_group`) but has no safe call to signal a whole
/// group, and the workspace forbids `unsafe` (so no `libc::killpg`). The
/// `kill` program does it: `kill -KILL -- -<pgid>`. If `kill` is missing
/// or fails, the caller still kills the direct child, so at worst
/// grandchildren survive. SIGKILL rather than SIGTERM because a test that
/// ignores SIGTERM must not keep the task alive.
#[cfg(unix)]
fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // A panic while holding the lock leaves the data consistent enough:
    // at worst a kill is skipped or repeated.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Starts `spec` with stdin closed and stdout and stderr captured. Each
/// line goes to `on_event` as [`TaskEvent::Output`] as soon as it arrives,
/// in arrival order (lines from the two pipes are interleaved as they are
/// read, which can differ slightly from the order the program wrote them),
/// then [`TaskEvent::Finished`] once the program has exited. `on_event` runs
/// on a thread of the task's own. Fails when the program cannot be started
/// (not found, `cwd` missing, not executable).
pub fn start(
    spec: &TaskSpec,
    on_event: impl Fn(TaskEvent) + Send + 'static,
) -> io::Result<TaskHandle> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A new process group with the child as leader, so `kill` can stop
        // everything it starts.
        cmd.process_group(0);
    }
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let shared = Arc::new(Mutex::new(Shared {
        child,
        reaped: false,
    }));

    let handle = TaskHandle { pid, shared };
    // If a thread cannot be started, do not leave the program running
    // with nobody reading its output.
    if let Err(e) = spawn_threads(&handle, stdout, stderr, started, on_event) {
        handle.kill();
        let _ = lock(&handle.shared).child.wait();
        return Err(e);
    }
    Ok(handle)
}

fn spawn_threads(
    handle: &TaskHandle,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    started: Instant,
    on_event: impl Fn(TaskEvent) + Send + 'static,
) -> io::Result<()> {
    let pid = handle.pid;
    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [
        stdout.map(|p| Box::new(p) as Box<dyn Read + Send>),
        stderr.map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        thread::Builder::new()
            .name(format!("task-{pid}-pipe"))
            .spawn(move || read_lines(pipe, |line| tx.send(line).is_ok()))?;
    }
    drop(tx);

    let waiter = Arc::clone(&handle.shared);
    thread::Builder::new()
        .name(format!("task-{pid}"))
        .spawn(move || {
            let mut exit: Option<(Option<ExitStatus>, Instant)> = None;
            let mut pipes_open = true;
            let mut polled: Option<Instant> = None;
            loop {
                // At most once per POLL while output streams in.
                if exit.is_none() && polled.is_none_or(|p| p.elapsed() >= POLL) {
                    exit = poll_exit(&waiter).map(|st| (st, Instant::now()));
                    polled = Some(Instant::now());
                }
                if let Some((_, at)) = exit
                    && (!pipes_open || at.elapsed() >= DRAIN_GRACE)
                {
                    break;
                }
                let wait = match exit {
                    Some((_, at)) => DRAIN_GRACE.saturating_sub(at.elapsed()).min(POLL),
                    None => POLL,
                };
                if !pipes_open {
                    thread::sleep(wait);
                    continue;
                }
                match rx.recv_timeout(wait) {
                    Ok(line) => on_event(TaskEvent::Output(line)),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => pipes_open = false,
                }
            }
            let (status, at) = exit.expect("loop ends only after exit");
            on_event(TaskEvent::Finished {
                code: status.and_then(|s| s.code()),
                duration: at.duration_since(started),
            });
        })?;
    Ok(())
}

/// `Some(status)` once the child has exited (and is now reaped), `None`
/// while it runs. An error from the OS counts as exited with no status.
fn poll_exit(shared: &Mutex<Shared>) -> Option<Option<ExitStatus>> {
    let mut s = lock(shared);
    match s.child.try_wait() {
        Ok(None) => None,
        Ok(Some(st)) => {
            s.reaped = true;
            Some(Some(st))
        }
        Err(_) => {
            s.reaped = true;
            Some(None)
        }
    }
}

/// Reads `pipe` to the end, calling `emit` with each line (without `\n` or
/// `\r\n`) and splitting lines longer than [`MAX_LINE`] at a char boundary.
/// Stops early when `emit` returns false.
fn read_lines(pipe: impl Read, mut emit: impl FnMut(String) -> bool) {
    let mut reader = BufReader::new(pipe);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let room = (MAX_LINE - buf.len()) as u64;
        let n = match (&mut reader).take(room).read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        if n == 0 {
            if !buf.is_empty() {
                emit(String::from_utf8_lossy(&buf).into_owned());
            }
            return;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            if !emit(String::from_utf8_lossy(&buf).into_owned()) {
                return;
            }
            buf.clear();
        } else if buf.len() >= MAX_LINE {
            // Cut before an incomplete UTF-8 sequence at the end, if any.
            let cut = match std::str::from_utf8(&buf) {
                Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => e.valid_up_to(),
                _ => buf.len(),
            };
            let rest = buf.split_off(cut);
            if !emit(String::from_utf8_lossy(&buf).into_owned()) {
                return;
            }
            buf = rest;
        }
    }
}

fn add_failure(s: &mut TestSummary, name: &str) {
    if !name.is_empty() && !s.failures.iter().any(|f| f == name) {
        s.failures.push(name.to_string());
    }
}

/// Totals from `cargo test` output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestSummary {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    /// Names of failing tests (doc tests look like `src/lib.rs - add (line
    /// 9)`), each once, in the order they first appear.
    pub failures: Vec<String>,
}

/// Sums every `test result:` line of `cargo test` output (unit tests,
/// integration tests and doc tests each print one) and collects the names
/// of failing tests, from both the pretty (`test x ... FAILED`) and the
/// terse (`x --- FAILED`, `cargo test -q`) formats and from the closing
/// `failures:` list. `None` when there is no `test result:` line.
///
/// A test binary that crashes or fails to build prints no result line, so
/// the totals can miss tests. Use the exit status for pass or fail; this
/// is for showing what happened.
pub fn parse_cargo_test(output: &str) -> Option<TestSummary> {
    let mut summary = TestSummary::default();
    let mut seen = false;
    // The names under a `failures:` heading, kept only if the list runs
    // straight into a `test result:` line (the first `failures:` section
    // holds captured output instead).
    let mut pending: Option<Vec<String>> = None;
    for raw in output.lines() {
        let line = raw.trim_end();
        if let Some(rest) = line.trim_start().strip_prefix("test result:") {
            seen = true;
            let words: Vec<&str> = rest.split_whitespace().collect();
            for w in words.windows(2) {
                let Ok(n) = w[0].parse::<u64>() else {
                    continue;
                };
                match w[1].trim_end_matches([';', ',', '.']) {
                    "passed" => summary.passed += n,
                    "failed" => summary.failed += n,
                    "ignored" => summary.ignored += n,
                    _ => {}
                }
            }
            for name in pending.take().unwrap_or_default() {
                add_failure(&mut summary, &name);
            }
            continue;
        }
        if line == "failures:" {
            pending = Some(Vec::new());
            continue;
        }
        if let Some(list) = pending.as_mut() {
            if line.is_empty() {
                continue;
            }
            if let Some(name) = raw.strip_prefix("    ") {
                list.push(name.trim().to_string());
                continue;
            }
            pending = None;
        }
        if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|l| l.strip_suffix(" ... FAILED"))
        {
            add_failure(&mut summary, name);
        } else if let Some(name) = line.strip_suffix(" --- FAILED") {
            add_failure(&mut summary, name);
        }
    }
    seen.then_some(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_lines_on_char_boundaries() {
        // 'é' is two bytes; put one across the MAX_LINE boundary.
        let mut text = "a".repeat(MAX_LINE - 1);
        text.push('é');
        text.push_str("tail\nnext\r\nlast");
        let mut out = Vec::new();
        read_lines(text.as_bytes(), |l| {
            out.push(l);
            true
        });
        assert_eq!(
            out.len(),
            4,
            "{:?}",
            out.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(out[0].len(), MAX_LINE - 1);
        assert_eq!(out[1], "étail");
        assert_eq!(out[2], "next");
        assert_eq!(out[3], "last");
        assert!(!out.iter().any(|l| l.contains('\u{fffd}')));
    }

    #[test]
    fn no_result_line_is_none() {
        assert_eq!(parse_cargo_test(""), None);
        assert_eq!(
            parse_cargo_test("error[E0425]: cannot find value `x` in this scope\n"),
            None
        );
    }
}
