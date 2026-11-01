//! Verification commands and their results. See `docs/decisions/0006-agent-runtime.md`.
//!
//! [`start`] runs a command (a test suite, a build) in a directory on plain
//! threads and reports each output line and the exit status through a
//! callback. [`parse_cargo_test`] summarizes `cargo test` output.
//!
//! On Unix the command runs in a process group of its own, and when it
//! exits, whatever it left running in that group is ended before
//! `Finished` is reported: SIGHUP, then SIGKILL once nothing else is left
//! in the group or 200 ms have passed, whichever comes first. A process that
//! moved itself to another group or session (`setsid`, a daemon) is not
//! reached.
//!
//! A group id is the pid of the process that made the group, and it cannot
//! be reused while any process of the group still exists, a zombie
//! included. So the command does not lead its group: a placeholder does
//! (`cat` reading a pipe that only this crate holds), and the placeholder is
//! reaped only after the last signal to the group. The command can then be
//! reaped as soon as it exits, which is how its exit is noticed, without
//! freeing the group id while something may still signal it.
//!
//! Two cases end nothing beyond the program itself. Signals go through the
//! `kill` program; if it is missing or fails, what the program left keeps
//! running. And when Maestro was started with SIGCHLD ignored (a
//! disposition a parent can pass on), the kernel reaps children by itself,
//! so no pid of a child is safe to signal once it may have exited: on
//! Linux this is read from `/proc/self/status`, and then there is no
//! placeholder and no group is ever signaled. Elsewhere it cannot be told.

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
    /// than [`MAX_LINE`] bytes of raw output arrive in pieces. Invalid UTF-8
    /// is replaced with U+FFFD, which can make a piece longer than
    /// `MAX_LINE` bytes as text (up to three times).
    Output(String),
    /// The program exited, and on Unix what it left in its process group
    /// was ended. `code` is the program's own exit code, `None` when a
    /// signal ended it (for example after [`TaskHandle::kill`]).
    /// `duration` runs from the start to the program's exit.
    Finished {
        code: Option<i32>,
        duration: Duration,
    },
}

/// Longest piece of raw output (in bytes) delivered as one line.
pub const MAX_LINE: usize = 64 * 1024;

/// Lines read but not yet passed to `on_event`.
const QUEUE: usize = 1024;

/// Once the program has exited and its process group has been ended,
/// `Finished` waits for the output pipes to close, but no longer than this
/// with no new line, and no longer than [`DRAIN_LIMIT`] after the exit in
/// all. By then only a process outside the group can hold the pipes open;
/// its later output is dropped, and the reader threads stay blocked on the
/// pipes until it closes them.
const DRAIN_GRACE: Duration = Duration::from_millis(500);
const DRAIN_LIMIT: Duration = Duration::from_secs(5);

/// How long what the program left in its group gets between SIGHUP and
/// SIGKILL, at most.
#[cfg(unix)]
const END_GRACE: Duration = Duration::from_millis(200);

/// How often the event thread checks whether the program has exited when
/// no output is arriving.
const POLL: Duration = Duration::from_millis(20);

struct Shared {
    child: Child,
    /// Set once the child has been reaped. After that its process id may
    /// belong to someone else, so `child.kill()` must not be used. Only
    /// read and written under the lock.
    reaped: bool,
    /// The placeholder leading the program's process group, until it is
    /// reaped. While it is here, unreaped, its pid (the group id) cannot
    /// belong to anyone else. Only the event thread takes it out and reaps
    /// it, under the lock, after the group's last signal. `None` from the
    /// start when SIGCHLD is ignored (see the crate docs): then nothing is
    /// ever signaled by group.
    #[cfg(unix)]
    leader: Option<Child>,
}

/// A running task. Dropping the handle does not stop the task.
pub struct TaskHandle {
    pid: u32,
    shared: Arc<Mutex<Shared>>,
}

impl TaskHandle {
    /// Stops the task and everything it started that stayed in its process
    /// group (on Unix, the task runs in a group of its own; `cargo test`
    /// and the test binaries it spawns are all in it). Does nothing once
    /// `Finished` has been reported. `Finished` still arrives, with
    /// `code: None` if the program itself was still running.
    pub fn kill(&self) {
        let mut s = lock(&self.shared);
        // The group's leader is not reaped while it is held here, and it is
        // only taken out under this lock, so the group id is still ours.
        #[cfg(unix)]
        if let Some(leader) = &s.leader {
            signal_group("KILL", leader.id());
        }
        if !s.reaped {
            let _ = s.child.kill();
        }
    }

    /// The program's process id. On Unix the program is not the leader of
    /// its process group, so this is not the group id.
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// Sends `signal` to the process group `pgid`.
///
/// The standard library can put a child in a group
/// (`CommandExt::process_group`) but has no safe call to signal a whole
/// group, and the workspace forbids `unsafe` (so no `libc::killpg`). The
/// `kill` program does it: `kill -<signal> -- -<pgid>`. If `kill` is
/// missing or fails, the caller still kills the direct child, so at worst
/// what it started survives. A `kill` that has not finished after
/// [`KILL_PROGRAM_LIMIT`] is killed, so this never blocks for longer.
/// Callers make sure `pgid` is still the task's group: its leader is not
/// reaped yet.
#[cfg(unix)]
fn signal_group(signal: &str, pgid: u32) {
    let Ok(mut kill) = Command::new("kill")
        .args([format!("-{signal}"), "--".into(), format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = Instant::now() + KILL_PROGRAM_LIMIT;
    loop {
        match kill.try_wait() {
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
            Ok(Some(_)) => return,
            _ => {
                let _ = kill.kill();
                let _ = kill.wait();
                return;
            }
        }
    }
}

/// The longest `signal_group` waits for the `kill` program.
#[cfg(unix)]
const KILL_PROGRAM_LIMIT: Duration = Duration::from_secs(1);

/// Whether this process ignores SIGCHLD, from `/proc/self/status`. Then
/// the kernel reaps its children on its own, and a child's pid may belong
/// to someone else as soon as the child exits. `false` when it cannot be
/// told.
#[cfg(target_os = "linux")]
fn sigchld_ignored() -> bool {
    // SIGCHLD is signal 17 on Linux: bit 16 of the `SigIgn` mask.
    const SIGCHLD_BIT: u64 = 1 << 16;
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("SigIgn:"))
                .and_then(|m| u64::from_str_radix(m.trim(), 16).ok())
        })
        .is_some_and(|mask| mask & SIGCHLD_BIT != 0)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn sigchld_ignored() -> bool {
    false
}

/// Whether any live process other than the leader `pgid` is in process
/// group `pgid`, from `/proc`. `None` when that cannot be told (no `/proc`,
/// or one that is not this process's).
#[cfg(target_os = "linux")]
fn others_in_group(pgid: u32) -> Option<bool> {
    // A `/proc` from another pid namespace would name other processes.
    let me = std::fs::read_link("/proc/self").ok()?;
    if me.to_str()?.parse::<u32>().ok()? != std::process::id() {
        return None;
    }
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == pgid {
            continue;
        }
        // Gone in the meantime, or not readable: not ours either way.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // The command name is in parentheses and may hold spaces; the
        // fields after it are state, parent pid, process group.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next();
        let group = fields.nth(1).and_then(|g| g.parse::<u32>().ok());
        if group == Some(pgid) && !matches!(state, Some("Z" | "X")) {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn others_in_group(_pgid: u32) -> Option<bool> {
    None
}

/// One step of ending what the program left in its process group, run on
/// the event thread after the program was reaped. Returns true once the
/// group has had its last signal and the leader is reaped.
///
/// The first step sends SIGHUP unless nothing else is known to be in the
/// group. Later steps wait until nothing else is, or [`END_GRACE`] has
/// passed. The last sends SIGKILL to the group in any case (a process can
/// fork between the look at `/proc` and the signal), then reaps the leader.
#[cfg(unix)]
fn end_group_step(shared: &Mutex<Shared>, hup_at: &mut Option<Instant>) -> bool {
    let pgid = match &lock(shared).leader {
        Some(leader) => leader.id(),
        None => return true,
    };
    // Only this thread takes the leader out, so the group id stays ours
    // until the end of this function even without the lock.
    let others = others_in_group(pgid);
    match *hup_at {
        None if others == Some(false) => {}
        None => {
            signal_group("HUP", pgid);
            *hup_at = Some(Instant::now());
            return false;
        }
        Some(at) if others != Some(false) && at.elapsed() < END_GRACE => return false,
        Some(_) => {}
    }
    let mut s = lock(shared);
    if let Some(mut leader) = s.leader.take() {
        signal_group("KILL", pgid);
        let _ = leader.kill();
        let _ = leader.wait();
    }
    true
}

#[cfg(not(unix))]
fn end_group_step(_shared: &Mutex<Shared>, _hup_at: &mut Option<Instant>) -> bool {
    true
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // A panic while holding the lock leaves the data consistent enough:
    // at worst a kill is skipped or repeated.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Starts the placeholder that leads the task's process group: `cat`,
/// reading a pipe whose write end only this crate holds, so it lives until
/// it is killed or Maestro exits. It runs in `/` so it keeps no directory
/// busy.
#[cfg(unix)]
fn start_leader() -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    Command::new("cat")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("could not start `cat` to hold the task's process group: {e}"),
            )
        })
}

/// Starts `spec` with stdin closed and stdout and stderr captured. Each
/// line goes to `on_event` as [`TaskEvent::Output`] as soon as it arrives,
/// in arrival order (lines from the two pipes are interleaved as they are
/// read, which can differ slightly from the order the program wrote them),
/// then [`TaskEvent::Finished`] once the program has exited and, on Unix,
/// what it left in its process group has been ended. `on_event` runs on a
/// thread of the task's own. Fails when the program cannot be started (not
/// found, `cwd` missing, not executable).
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
    let mut leader = {
        use std::os::unix::process::CommandExt;
        if sigchld_ignored() {
            // A group of its own still, but nothing will signal it.
            cmd.process_group(0);
            None
        } else {
            let mut leader = start_leader()?;
            match i32::try_from(leader.id()) {
                // The placeholder's group, so `kill` can stop everything the
                // program starts.
                Ok(pgid) => {
                    cmd.process_group(pgid);
                }
                Err(_) => {
                    let _ = leader.kill();
                    let _ = leader.wait();
                    return Err(io::Error::other("process id out of range"));
                }
            }
            Some(leader)
        }
    };
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            #[cfg(unix)]
            if let Some(leader) = leader.as_mut() {
                let _ = leader.kill();
                let _ = leader.wait();
            }
            return Err(e);
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let shared = Arc::new(Mutex::new(Shared {
        child,
        reaped: false,
        #[cfg(unix)]
        leader,
    }));

    let handle = TaskHandle { pid, shared };
    // If a thread cannot be started, do not leave the program running
    // with nobody reading its output. The event thread, the only other
    // place that reaps the leader, was not started.
    if let Err(e) = spawn_threads(&handle, stdout, stderr, started, on_event) {
        handle.kill();
        let mut s = lock(&handle.shared);
        let _ = s.child.wait();
        #[cfg(unix)]
        if let Some(mut leader) = s.leader.take() {
            let _ = leader.kill();
            let _ = leader.wait();
        }
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
    // Bounded: when `on_event` is slower than the output, the readers wait,
    // the pipes fill and the program blocks on its writes, instead of lines
    // piling up in memory.
    let (tx, rx) = mpsc::sync_channel::<String>(QUEUE);
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
            // When the program's group was ended, and when SIGHUP went out.
            let mut ended: Option<Instant> = None;
            let mut hup_at: Option<Instant> = None;
            let mut pipes_open = true;
            let mut polled: Option<Instant> = None;
            let mut last_line = Instant::now();
            loop {
                // At most once per POLL while output streams in.
                if exit.is_none() && polled.is_none_or(|p| p.elapsed() >= POLL) {
                    exit = poll_exit(&waiter).map(|st| (st, Instant::now()));
                    polled = Some(Instant::now());
                }
                if exit.is_some() && ended.is_none() && end_group_step(&waiter, &mut hup_at) {
                    ended = Some(Instant::now());
                }
                // Silence counts from the end of the group at the earliest:
                // output written just before the exit may not have been read
                // yet.
                if let (Some((_, at)), Some(end)) = (exit, ended)
                    && (!pipes_open
                        || last_line.max(end).elapsed() >= DRAIN_GRACE
                        || at.elapsed() >= DRAIN_LIMIT)
                {
                    break;
                }
                if !pipes_open {
                    thread::sleep(POLL);
                    continue;
                }
                match rx.recv_timeout(POLL) {
                    Ok(line) => {
                        last_line = Instant::now();
                        on_event(TaskEvent::Output(line));
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => pipes_open = false,
                }
            }
            // Lines already read and queued are delivered; at most one
            // queue's worth, so a process that keeps writing cannot hold
            // `Finished` back.
            for line in rx.try_iter().take(QUEUE) {
                on_event(TaskEvent::Output(line));
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
    #[cfg(unix)]
    use std::path::Path;

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

    #[cfg(target_os = "linux")]
    #[test]
    fn others_in_group_sees_members_but_not_the_leader_or_zombies() {
        use std::os::unix::process::CommandExt;
        let mut leader = start_leader().unwrap();
        let pgid = leader.id();
        assert_eq!(others_in_group(pgid), Some(false));
        let mut member = Command::new("sleep")
            .arg("30")
            .process_group(i32::try_from(pgid).unwrap())
            .spawn()
            .unwrap();
        assert_eq!(others_in_group(pgid), Some(true));
        member.kill().unwrap();
        // Dead but not reaped: a zombie does not count.
        let deadline = Instant::now() + Duration::from_secs(5);
        while others_in_group(pgid) != Some(false) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(others_in_group(pgid), Some(false));
        member.wait().unwrap();
        leader.kill().unwrap();
        leader.wait().unwrap();
    }

    /// A path for a ready flag no other test uses.
    #[cfg(unix)]
    fn flag(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "maestro-tasks-{name}-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Runs `script` (with `$1` set to `arg`) and collects its lines and
    /// exit code.
    #[cfg(unix)]
    fn run_script(script: &str, arg: &str) -> (TaskHandle, Vec<String>, Option<i32>) {
        let (tx, rx) = mpsc::channel();
        let spec = TaskSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into(), arg.into()],
            cwd: std::env::temp_dir(),
        };
        let h = start(&spec, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        let mut lines = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TaskEvent::Output(l) => lines.push(l),
                TaskEvent::Finished { code, .. } => return (h, lines, code),
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_leader_is_reaped_before_finished_and_a_hangup_handler_gets_to_run() {
        let ready = flag("hup");
        // The background loop cleans up on SIGHUP: the grace period lets
        // its handler run before SIGKILL. The shell exits only once the
        // handler is in place.
        let script = "(trap 'echo cleaned; exit 0' HUP; : > \"$1\"; while :; do sleep 0.05; done) & \
                      while [ ! -e \"$1\" ]; do sleep 0.01; done; echo started";
        let (tx, rx) = mpsc::channel();
        let spec = TaskSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                ready.display().to_string(),
            ],
            cwd: std::env::temp_dir(),
        };
        let h = start(&spec, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        // The command waits for the flag, so the leader is still here.
        let leader = lock(&h.shared).leader.as_ref().map(Child::id).unwrap();
        let mut lines = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TaskEvent::Output(l) => lines.push(l),
                TaskEvent::Finished { code, .. } => {
                    assert_eq!(code, Some(0));
                    break;
                }
            }
        }
        let _ = std::fs::remove_file(&ready);
        assert_eq!(lines, ["started", "cleaned"]);
        // Reaped, not just let go of: a zombie would still be in /proc.
        assert!(lock(&h.shared).leader.is_none());
        assert!(
            !Path::new(&format!("/proc/{leader}")).exists(),
            "the leader {leader} was not reaped"
        );
        // Nothing is left to signal.
        h.kill();
    }

    /// Set in the environment of the copy of this test binary that
    /// `with_sigchld_ignored_no_group_is_signaled` starts.
    #[cfg(target_os = "linux")]
    const SIGCHLD_CHILD: &str = "MAESTRO_TASKS_SIGCHLD_IGNORED_CHILD";

    #[cfg(target_os = "linux")]
    #[test]
    fn with_sigchld_ignored_no_group_is_signaled() {
        if std::env::var_os(SIGCHLD_CHILD).is_some() {
            return sigchld_ignored_child();
        }
        assert!(!sigchld_ignored());
        // The same test binary, run with SIGCHLD ignored (which survives
        // exec), running only the part below.
        let exe = std::env::current_exe().unwrap();
        let out = Command::new("sh")
            .args([
                "-c",
                "trap '' CHLD; exec \"$0\" \"$@\"",
                &exe.display().to_string(),
                "--exact",
                "tests::with_sigchld_ignored_no_group_is_signaled",
                "--nocapture",
            ])
            .env(SIGCHLD_CHILD, "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("sigchld child ran"), "{text}");
    }

    #[cfg(target_os = "linux")]
    fn sigchld_ignored_child() {
        assert!(sigchld_ignored(), "the parent did not ignore SIGCHLD");
        let (h, lines, _code) = run_script("sleep 1000 & echo $!", "");
        assert!(lock(&h.shared).leader.is_none(), "no placeholder");
        let pid: u32 = lines[0].trim().parse().unwrap();
        // Nothing signaled the group, so the leftover still runs.
        let alive = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            !s.rsplit_once(')')
                .is_some_and(|(_, r)| r.trim_start().starts_with('Z'))
        });
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        assert!(alive, "the leftover {pid} was signaled");
        println!("sigchld child ran");
    }
}
