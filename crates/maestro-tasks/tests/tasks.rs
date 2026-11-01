//! Runs real processes through `start` and parses real `cargo test` output.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use maestro_tasks::{TaskEvent, TaskSpec, TestSummary, parse_cargo_test, start};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn spec(program: &str, args: &[&str]) -> TaskSpec {
    TaskSpec {
        program: program.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: std::env::temp_dir(),
    }
}

fn sh(script: &str) -> TaskSpec {
    spec("sh", &["-c", script])
}

fn run(spec: &TaskSpec) -> (maestro_tasks::TaskHandle, Receiver<TaskEvent>) {
    let (tx, rx) = mpsc::channel();
    let h = start(spec, move |e| {
        let _ = tx.send(e);
    })
    .expect("start");
    (h, rx)
}

/// Every event until `Finished`, which is returned separately.
fn collect(rx: &Receiver<TaskEvent>, limit: Duration) -> (Vec<String>, Option<i32>, Duration) {
    let deadline = Instant::now() + limit;
    let mut lines = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("task did not finish in time") {
            TaskEvent::Output(l) => lines.push(l),
            TaskEvent::Finished { code, duration } => {
                // Nothing after Finished.
                assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
                return (lines, code, duration);
            }
        }
    }
}

#[test]
fn reports_lines_in_order_then_the_exit_code() {
    let (_h, rx) = run(&sh("echo one; echo two; printf 'no newline'"));
    let (lines, code, _) = collect(&rx, Duration::from_secs(10));
    assert_eq!(lines, vec!["one", "two", "no newline"]);
    assert_eq!(code, Some(0));

    // Both pipes, interleaved as they arrive.
    let (_h, rx) = run(&sh(
        "echo out; sleep 0.2; echo err >&2; sleep 0.2; echo 'out again'; exit 3",
    ));
    let (lines, code, duration) = collect(&rx, Duration::from_secs(10));
    assert_eq!(lines, vec!["out", "err", "out again"]);
    assert_eq!(code, Some(3));
    assert!(duration >= Duration::from_millis(400), "{duration:?}");
}

#[test]
fn runs_in_cwd_with_stdin_closed() {
    let dir = std::fs::canonicalize(fixtures()).unwrap();
    let (_h, rx) = run(&TaskSpec {
        program: "pwd".into(),
        args: vec![],
        cwd: dir.clone(),
    });
    let (lines, code, _) = collect(&rx, Duration::from_secs(10));
    assert_eq!(lines, vec![dir.display().to_string()]);
    assert_eq!(code, Some(0));

    // `cat` with a closed stdin ends at once instead of waiting for input.
    let (_h, rx) = run(&spec("cat", &[]));
    let (lines, code, _) = collect(&rx, Duration::from_secs(10));
    assert!(lines.is_empty());
    assert_eq!(code, Some(0));
}

#[test]
fn a_missing_program_fails_to_start() {
    let err = start(&spec("maestro-no-such-program-xyz", &[]), |_| {})
        .err()
        .expect("should fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let mut bad_dir = spec("sh", &["-c", "true"]);
    bad_dir.cwd = PathBuf::from("/no/such/directory/for/maestro");
    assert!(start(&bad_dir, |_| {}).is_err());
}

#[test]
fn events_arrive_while_the_task_runs() {
    let (_h, rx) = run(&sh("echo started; sleep 3; echo done"));
    let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first, TaskEvent::Output("started".into()));
    // Still running: nothing else yet.
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    let (lines, code, _) = collect(&rx, Duration::from_secs(10));
    assert_eq!(lines, vec!["done"]);
    assert_eq!(code, Some(0));
}

#[test]
fn a_slow_callback_still_gets_every_line_written_before_exit() {
    // 100 lines at once, then exit; the callback takes 10 ms per line, so
    // delivering them all takes about twice the drain grace.
    let (tx, rx) = mpsc::channel();
    let _h = start(&sh("seq 1 100"), move |e| {
        if matches!(e, TaskEvent::Output(_)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = tx.send(e);
    })
    .unwrap();
    let (lines, code, _) = collect(&rx, Duration::from_secs(20));
    assert_eq!(code, Some(0));
    let want: Vec<String> = (1..=100).map(|i| i.to_string()).collect();
    assert_eq!(lines, want);
}

#[test]
fn many_lines_arrive_complete_and_in_order() {
    let (_h, rx) = run(&sh("seq 1 50000"));
    let (lines, code, _) = collect(&rx, Duration::from_secs(30));
    assert_eq!(code, Some(0));
    assert_eq!(lines.len(), 50_000);
    assert!(
        lines
            .iter()
            .enumerate()
            .all(|(i, l)| *l == (i + 1).to_string())
    );
}

#[test]
fn long_output_lines_arrive_in_pieces() {
    let (_h, rx) = run(&sh(
        "head -c 200000 /dev/zero | tr '\\0' a; echo; echo after",
    ));
    let (lines, code, _) = collect(&rx, Duration::from_secs(10));
    assert_eq!(code, Some(0));
    assert_eq!(lines.last().map(String::as_str), Some("after"));
    let total: usize = lines[..lines.len() - 1].iter().map(String::len).sum();
    assert_eq!(total, 200_000);
    assert!(lines.iter().all(|l| l.len() <= maestro_tasks::MAX_LINE));
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // A zombie waiting for its new parent to reap it counts as dead.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => {
            // No procfs: ask `kill -0`.
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }
    }
}

#[cfg(unix)]
#[test]
fn kill_stops_the_child_and_what_it_started() {
    // The shell starts a grandchild that holds the output pipe open and
    // would outlive the shell by 30 seconds.
    let (h, rx) = run(&sh("sleep 30 & echo $!; wait"));
    let TaskEvent::Output(pid) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected the grandchild's pid");
    };
    let grandchild: u32 = pid.trim().parse().unwrap();
    assert!(alive(grandchild));
    assert!(alive(h.id()));

    let t = Instant::now();
    h.kill();
    let (_, code, _) = collect(&rx, Duration::from_secs(5));
    assert_eq!(code, None, "killed by a signal");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(grandchild) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(grandchild), "the grandchild survived the kill");

    // Killing a finished task does nothing.
    h.kill();
}

/// Waits up to `limit` for `pid` to be gone; whether it is.
#[cfg(unix)]
fn gone_within(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

/// The pid a script printed as its first line.
#[cfg(unix)]
fn first_pid(lines: &[String]) -> u32 {
    lines
        .first()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or_else(|| panic!("expected a pid first: {lines:?}"))
}

#[cfg(unix)]
#[test]
fn a_background_process_left_behind_ends_when_the_command_finishes() {
    // The shell exits at once; its background child would run for 1000
    // seconds and keeps stdout open.
    let t = Instant::now();
    let (_h, rx) = run(&sh("sleep 1000 & echo $!; echo done"));
    let (lines, code, _) = collect(&rx, Duration::from_secs(5));
    let elapsed = t.elapsed();
    let leftover = first_pid(&lines);
    let gone = gone_within(leftover, Duration::from_secs(1));
    if !gone {
        kill_pid(leftover);
    }
    assert_eq!(lines[1..], ["done"]);
    assert_eq!(code, Some(0), "the command's own status, not the cleanup's");
    assert!(gone, "the background sleep {leftover} outlived the task");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

#[cfg(unix)]
#[test]
fn a_leftover_that_ignores_sighup_is_killed_after_the_grace_period() {
    let t = Instant::now();
    let (_h, rx) = run(&sh(
        "(trap '' HUP; exec sleep 1000) & echo $!; sleep 0.1; exit 4",
    ));
    let (lines, code, _) = collect(&rx, Duration::from_secs(5));
    let elapsed = t.elapsed();
    let leftover = first_pid(&lines);
    let gone = gone_within(leftover, Duration::from_secs(1));
    if !gone {
        kill_pid(leftover);
    }
    assert_eq!(code, Some(4));
    assert!(
        gone,
        "the sleep that ignores SIGHUP ({leftover}) outlived the task"
    );
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_process_that_left_the_group_is_not_signaled_and_does_not_hold_back_finished() {
    if std::process::Command::new("setsid")
        .arg("true")
        .status()
        .is_err()
    {
        eprintln!("no setsid program; skipped");
        return;
    }
    // `setsid` makes the sleep a session (and group) of its own. It keeps
    // stdout open, so Finished waits for the pipe no longer than its
    // bounds. The shell exits only once the sleep has left the group.
    let ready = std::env::temp_dir().join(format!("maestro-setsid-{}", std::process::id()));
    let _ = std::fs::remove_file(&ready);
    let t = Instant::now();
    let (_h, rx) = run(&spec(
        "sh",
        &[
            "-c",
            "setsid sh -c ': > \"$1\"; exec sleep 1000' sh \"$1\" & \
             while [ ! -e \"$1\" ]; do sleep 0.01; done; echo $!; echo done",
            "sh",
            &ready.display().to_string(),
        ],
    ));
    let (lines, code, _) = collect(&rx, Duration::from_secs(8));
    let elapsed = t.elapsed();
    let outside = first_pid(&lines);
    let _ = std::fs::remove_file(&ready);
    let survived = alive(outside);
    kill_pid(outside);
    assert_eq!(code, Some(0));
    assert!(elapsed < Duration::from_secs(6), "{elapsed:?}");
    assert!(survived, "a process outside the task's group was signaled");
}

fn expected_summary() -> TestSummary {
    TestSummary {
        passed: 3,
        failed: 2,
        ignored: 1,
        failures: vec!["tests::fails".into(), "src/lib.rs - add (line 9)".into()],
    }
}

#[test]
fn parses_captured_cargo_test_output() {
    // Captured with `cargo test --no-fail-fast` (and `-q`) in
    // tests/fixtures/tiny, local paths replaced.
    for file in ["cargo-test-output.txt", "cargo-test-quiet.txt"] {
        let text = std::fs::read_to_string(fixtures().join(file)).unwrap();
        assert_eq!(parse_cargo_test(&text), Some(expected_summary()), "{file}");
    }
}

#[test]
fn parses_partial_and_passing_output() {
    let ok = "running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(
        parse_cargo_test(ok),
        Some(TestSummary {
            passed: 2,
            ..TestSummary::default()
        })
    );
    // Captured output with an indented line under `failures:` that is not
    // a name, then no closing list: only the result line's counts count.
    let odd = "failures:\n\n---- x stdout ----\n    not a name\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
    let s = parse_cargo_test(odd).unwrap();
    assert_eq!(s.failed, 1);
    assert!(s.failures.is_empty(), "{:?}", s.failures);
}

/// The real path: `cargo test` on the fixture crate, run through `start`,
/// its output parsed.
#[test]
fn runs_cargo_test_and_summarizes_it() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let target = std::env::temp_dir().join(format!("maestro-tasks-tiny-{}", std::process::id()));
    let spec = TaskSpec {
        program: cargo,
        args: vec![
            "test".into(),
            "--no-fail-fast".into(),
            "--offline".into(),
            "--locked".into(),
            "--target-dir".into(),
            target.display().to_string(),
        ],
        cwd: fixtures().join("tiny"),
    };
    let (_h, rx) = run(&spec);
    let (lines, code, _) = collect(&rx, Duration::from_secs(300));
    let _ = std::fs::remove_dir_all(&target);
    let text = lines.join("\n");
    assert_eq!(code, Some(101), "{text}");
    assert_eq!(parse_cargo_test(&text), Some(expected_summary()), "{text}");
}
