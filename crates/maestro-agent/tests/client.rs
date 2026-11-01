//! `AcpClient` against the fake agent (`fake-acp-agent`), run as a real
//! child process. The fake's command language is documented in
//! `maestro_agent::fake`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use maestro_agent::fake::{
    FAIL_SESSION_ARG, LOAD_SESSION_ARG, PROTOCOL_ARG, SESSION_ID, SLOW_SESSION_ARG,
};
use maestro_agent::{AcpClient, AcpEvent, ProviderKind, ProviderSpec, TranscriptEntry};
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    client: AcpClient,
    events: Receiver<AcpEvent>,
    root: PathBuf,
    outside: PathBuf,
    base: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.client.shutdown();
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn spec(extra: &[&str]) -> ProviderSpec {
    ProviderSpec {
        name: "fake".to_string(),
        kind: ProviderKind::Acp,
        program: env!("CARGO_BIN_EXE_fake-acp-agent").to_string(),
        args: extra.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
    }
}

fn dirs(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    static N: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!(
        "maestro-agent-client-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&base);
    let root = base.join("root");
    let outside = base.join("outside");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();
    fs::write(root.join(".git/config"), "[core]\n").unwrap();
    fs::write(outside.join("secret.txt"), "secret\n").unwrap();
    let root = fs::canonicalize(root).unwrap();
    let outside = fs::canonicalize(outside).unwrap();
    (base, root, outside)
}

fn start(name: &str, extra: &[&str]) -> Harness {
    let (base, root, outside) = dirs(name);
    let (tx, events) = mpsc::channel();
    let client = AcpClient::start(&spec(extra), &root, move |e| drop(tx.send(e))).unwrap();
    Harness {
        client,
        events,
        root,
        outside,
        base,
    }
}

/// Started, with the session up.
fn ready(name: &str) -> Harness {
    let h = start(name, &[]);
    h.wait_for(|e| matches!(e, AcpEvent::SessionStarted { .. }));
    h
}

impl Harness {
    /// The next event other than stderr, within the deadline.
    fn next(&self, deadline: Instant, seen: &[AcpEvent]) -> AcpEvent {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(AcpEvent::Stderr(_)) => continue,
                Ok(e) => return e,
                Err(e) => panic!("no event before the deadline ({e:?}); saw {seen:?}"),
            }
        }
    }

    /// Events up to and including the first that matches.
    fn wait_for(&self, pred: impl Fn(&AcpEvent) -> bool) -> Vec<AcpEvent> {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            let e = self.next(deadline, &seen);
            let done = pred(&e);
            seen.push(e);
            if done {
                return seen;
            }
        }
    }

    fn until_turn_end(&self) -> Vec<AcpEvent> {
        self.wait_for(|e| matches!(e, AcpEvent::TurnEnded { .. }))
    }

    /// Runs a script and returns its events, ending with `TurnEnded`.
    fn turn(&self, script: &str) -> Vec<AcpEvent> {
        self.client.prompt(script).unwrap();
        self.until_turn_end()
    }
}

fn said(events: &[AcpEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Update(TranscriptEntry::Agent(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn stop_reason(events: &[AcpEvent]) -> &str {
    match events.last() {
        Some(AcpEvent::TurnEnded { stop_reason }) => stop_reason,
        other => panic!("turn did not end: {other:?}"),
    }
}

fn errors(events: &[AcpEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AcpEvent::Error(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn initializes_and_starts_a_session_in_the_root() {
    let h = start("init", &[]);
    let events = h.wait_for(|e| matches!(e, AcpEvent::SessionStarted { .. }));
    assert_eq!(
        events,
        [
            AcpEvent::Initialized {
                agent_name: Some("fake-acp-agent".to_string()),
                load_session: false,
            },
            AcpEvent::SessionStarted {
                session_id: SESSION_ID.to_string()
            },
        ]
    );
    assert_eq!(h.client.session_id().as_deref(), Some(SESSION_ID));
    assert_eq!(h.client.root(), h.root);
    let events = h.turn("caps\ncwd");
    let said = said(&events);
    let caps: Value = serde_json::from_str(&said[0]).unwrap();
    assert_eq!(caps["fs"]["readTextFile"], true);
    assert_eq!(caps["fs"]["writeTextFile"], true);
    assert_ne!(caps["terminal"], true, "{caps}");
    assert_eq!(said[1], h.root.to_string_lossy());
    assert_eq!(stop_reason(&events), "end_turn");
}

#[test]
fn maps_messages_thoughts_plans_and_tools() {
    let h = ready("updates");
    let events = h.turn(
        "user echo\nthink hmm\nsay hello\nsay  world\nplan read | *write\n\
         tool t1 edit src/lib.rs Edit lib\ntool-update t1 completed\nstop max_tokens",
    );
    let lib = h.root.join("src/lib.rs");
    assert_eq!(
        events,
        [
            AcpEvent::Update(TranscriptEntry::User("echo".into())),
            AcpEvent::Update(TranscriptEntry::Thought("hmm".into())),
            AcpEvent::Update(TranscriptEntry::Agent("hello".into())),
            AcpEvent::Update(TranscriptEntry::Agent(" world".into())),
            AcpEvent::Update(TranscriptEntry::Plan(vec![
                ("read".into(), "pending".into()),
                ("write".into(), "completed".into()),
            ])),
            AcpEvent::Update(TranscriptEntry::Tool {
                id: "t1".into(),
                title: "Edit lib".into(),
                kind: "edit".into(),
                status: "pending".into(),
                locations: vec![lib],
            }),
            AcpEvent::ToolUpdate {
                id: "t1".into(),
                status: Some("completed".into()),
                title: None,
                locations: None,
            },
            AcpEvent::TurnEnded {
                stop_reason: "max_tokens".into()
            },
        ]
    );
    // A second turn works after the first ended.
    assert_eq!(said(&h.turn("say again")), ["again"]);
}

#[test]
fn ignores_command_mode_and_unknown_updates() {
    let h = ready("ignored");
    let events = h.turn("update-unknown\nsay after");
    assert_eq!(said(&events), ["after"]);
    assert!(errors(&events).is_empty(), "{events:?}");
}

#[test]
fn reports_malformed_updates_and_wrong_sessions() {
    let h = ready("badupdate");
    let events = h.turn("bad-update\nwrong-session\nsay after");
    let errs = errors(&events);
    assert_eq!(errs.len(), 2, "{events:?}");
    assert!(errs[0].contains("malformed tool_call"), "{errs:?}");
    assert!(errs[1].contains("unknown session"), "{errs:?}");
    // The stray message for the other session was not shown.
    assert_eq!(said(&events), ["after"]);
}

#[test]
fn writes_files_inside_the_root() {
    let h = ready("write");
    let events = h.turn("write notes/a.txt hello\\nworld\nwrite src/lib.rs replaced");
    let a = h.root.join("notes/a.txt");
    let lib = h.root.join("src/lib.rs");
    assert_eq!(
        events,
        [
            AcpEvent::FileWritten(a.clone()),
            AcpEvent::Update(TranscriptEntry::Agent("wrote notes/a.txt".into())),
            AcpEvent::FileWritten(lib.clone()),
            AcpEvent::Update(TranscriptEntry::Agent("wrote src/lib.rs".into())),
            AcpEvent::TurnEnded {
                stop_reason: "end_turn".into()
            },
        ]
    );
    assert_eq!(fs::read_to_string(a).unwrap(), "hello\nworld");
    assert_eq!(fs::read_to_string(lib).unwrap(), "replaced");
    let names: Vec<_> = fs::read_dir(h.root.join("src"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["lib.rs"], "temporary files left behind");
}

#[test]
fn reads_files_with_line_and_limit() {
    let h = ready("read");
    let events = h.turn("read src/lib.rs\nread src/lib.rs 2 1\nread missing.rs");
    let said = said(&events);
    assert_eq!(said[0], "read: one\ntwo\nthree\n");
    assert_eq!(said[1], "read: two\n");
    assert!(said[2].starts_with("read failed: -32002"), "{said:?}");
    // A missing file is not a refusal worth reporting.
    assert!(errors(&events).is_empty(), "{events:?}");
}

/// Runs one write and one read at `path` and checks both were refused
/// without touching anything outside the root.
fn assert_refused(h: &Harness, path: &str) {
    let events = h.turn(&format!("write {path} pwned\nread {path}"));
    let said = said(&events);
    assert!(
        said[0].starts_with("write failed: -32602"),
        "{path}: {said:?}"
    );
    assert!(said[1].starts_with("read failed:"), "{path}: {said:?}");
    assert!(
        !events.iter().any(|e| matches!(e, AcpEvent::FileWritten(_))),
        "{path}: {events:?}"
    );
    assert!(
        errors(&events).iter().any(|m| m.contains("refused")),
        "{path}: {events:?}"
    );
    assert_eq!(
        fs::read_to_string(h.outside.join("secret.txt")).unwrap(),
        "secret\n"
    );
    assert_eq!(
        fs::read_to_string(h.root.join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[test]
fn refuses_absolute_paths_outside_the_root() {
    let h = ready("abs");
    let secret = h.outside.join("secret.txt");
    assert_refused(&h, &secret.to_string_lossy());
    assert_refused(&h, &h.outside.join("new.txt").to_string_lossy());
    assert!(!h.outside.join("new.txt").exists());
}

#[test]
fn refuses_dot_dot_escapes() {
    let h = ready("dotdot");
    assert_refused(&h, "../outside/secret.txt");
    assert_refused(&h, "src/../../outside/new.txt");
    assert!(!h.outside.join("new.txt").exists());
}

#[cfg(unix)]
#[test]
fn refuses_symlinks_out_of_the_root() {
    use std::os::unix::fs::symlink;
    let h = ready("symlinks");
    symlink(&h.outside, h.root.join("linkdir")).unwrap();
    symlink(h.outside.join("secret.txt"), h.root.join("linkfile")).unwrap();
    symlink(h.outside.join("dangling.txt"), h.root.join("dangling")).unwrap();
    assert_refused(&h, "linkdir/secret.txt");
    assert_refused(&h, "linkdir/new.txt");
    assert_refused(&h, "linkfile");
    assert_refused(&h, "dangling");
    assert!(!h.outside.join("new.txt").exists());
    assert!(!h.outside.join("dangling.txt").exists());
}

#[test]
fn refuses_the_git_directory() {
    let h = ready("git");
    assert_refused(&h, ".git/config");
    assert_refused(&h, ".git/hooks/post-checkout");
    assert!(!h.root.join(".git/hooks").exists());
}

#[cfg(unix)]
#[test]
fn refuses_non_regular_files() {
    let h = ready("special");
    let fifo = h.root.join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    for path in ["src", "pipe"] {
        let events = h.turn(&format!("write {path} x\nread {path}"));
        let said = said(&events);
        assert!(said[0].contains("not a regular file"), "{said:?}");
        assert!(said[1].contains("not a regular file"), "{said:?}");
    }
    assert!(h.root.join("src").is_dir());
}

#[test]
fn relative_paths_are_refused() {
    let h = ready("relative");
    let events = h.turn("write-raw src/lib.rs pwned");
    let said = said(&events);
    assert!(
        said[0].starts_with("write failed: -32602") && said[0].contains("not an absolute path"),
        "{said:?}"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("src/lib.rs")).unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[test]
fn a_reused_permission_id_is_refused() {
    let h = ready("dup-permission");
    h.client.prompt("ask-dup First").unwrap();
    // Wait for the refusal of the second request before answering the
    // first: once answered, the id is free to be used again.
    let mut events = h.wait_for(
        |e| matches!(e, AcpEvent::Update(TranscriptEntry::Agent(t)) if t.starts_with("duplicate")),
    );
    let req = events
        .iter()
        .find_map(|e| match e {
            AcpEvent::PermissionRequested(r) => Some(r.clone()),
            _ => None,
        })
        .expect("the first request was shown");
    assert_eq!(req.tool_title, "First");
    h.client
        .answer_permission(req.request_id.clone(), Some("allow"))
        .unwrap();
    events.extend(h.until_turn_end());
    // The second request was refused, not shown, and did not take over
    // the first one's answer.
    let shown = events
        .iter()
        .filter(|e| matches!(e, AcpEvent::PermissionRequested(_)))
        .count();
    assert_eq!(shown, 1, "{events:?}");
    assert_eq!(said(&events), ["duplicate: -32600", "permission: allow"]);
}

#[test]
fn invalid_envelopes_are_not_acted_on() {
    let h = ready("envelope");
    let events = h.turn("fake-response\nunversioned-write planted.txt\nsay after");
    let errs = errors(&events);
    assert_eq!(errs.len(), 2, "{events:?}");
    assert!(
        errs.iter().all(|m| m.contains("not a JSON-RPC message")),
        "{errs:?}"
    );
    // The fake response did not end the turn, and the write did not run.
    assert_eq!(said(&events), ["after"]);
    assert_eq!(stop_reason(&events), "end_turn");
    assert!(!h.root.join("planted.txt").exists());
}

#[test]
fn permission_is_answered_with_a_choice() {
    let h = ready("permission");
    h.client.prompt("ask Write src/lib.rs\nsay done").unwrap();
    let events = h.wait_for(|e| matches!(e, AcpEvent::PermissionRequested(_)));
    let Some(AcpEvent::PermissionRequested(req)) = events.last() else {
        unreachable!()
    };
    assert_eq!(req.tool_title, "Write src/lib.rs");
    let ids: Vec<_> = req.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["allow", "reject"]);
    assert_eq!(req.options[0].kind, "allow_once");
    assert_eq!(req.options[1].label, "Reject");
    // A choice that was not offered is refused and nothing is sent.
    assert!(
        h.client
            .answer_permission(req.request_id.clone(), Some("always"))
            .is_err()
    );
    assert!(
        h.client
            .answer_permission(Value::String("nope".into()), Some("allow"))
            .is_err()
    );
    h.client
        .answer_permission(req.request_id.clone(), Some("reject"))
        .unwrap();
    // Answering twice is refused.
    assert!(
        h.client
            .answer_permission(req.request_id.clone(), Some("allow"))
            .is_err()
    );
    let events = h.until_turn_end();
    assert_eq!(said(&events), ["permission: reject", "done"]);
    assert_eq!(stop_reason(&events), "end_turn");
}

#[test]
fn permission_can_be_cancelled_by_the_user() {
    let h = ready("permission-none");
    h.client.prompt("ask Run tests").unwrap();
    let events = h.wait_for(|e| matches!(e, AcpEvent::PermissionRequested(_)));
    let Some(AcpEvent::PermissionRequested(req)) = events.last() else {
        unreachable!()
    };
    h.client
        .answer_permission(req.request_id.clone(), None)
        .unwrap();
    let events = h.until_turn_end();
    assert_eq!(said(&events), ["permission: cancelled"]);
    assert_eq!(stop_reason(&events), "cancelled");
}

#[test]
fn cancel_answers_a_pending_permission() {
    let h = ready("cancel-permission");
    h.client
        .prompt("ask Delete everything\nsay not reached")
        .unwrap();
    let events = h.wait_for(|e| matches!(e, AcpEvent::PermissionRequested(_)));
    let Some(AcpEvent::PermissionRequested(req)) = events.last() else {
        unreachable!()
    };
    h.client.cancel().unwrap();
    let events = h.until_turn_end();
    assert_eq!(
        said(&events),
        ["permission: cancelled after session/cancel"]
    );
    assert_eq!(stop_reason(&events), "cancelled");
    // The request was answered by the cancel.
    assert!(
        h.client
            .answer_permission(req.request_id.clone(), Some("allow"))
            .is_err()
    );
    // The session goes on.
    assert_eq!(said(&h.turn("say next")), ["next"]);
}

#[test]
fn cancel_ends_a_running_turn() {
    let h = ready("cancel");
    h.client
        .prompt("say working\nwait-cancel\nsay not reached")
        .unwrap();
    h.wait_for(|e| matches!(e, AcpEvent::Update(TranscriptEntry::Agent(_))));
    // Only one turn at a time.
    assert!(h.client.prompt("say second").is_err());
    h.client.cancel().unwrap();
    let events = h.until_turn_end();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(stop_reason(&events), "cancelled");
    // Cancelling with no turn running does nothing.
    h.client.cancel().unwrap();
    assert_eq!(said(&h.turn("say next")), ["next"]);
}

#[test]
fn a_prompt_sent_early_waits_for_the_session() {
    let h = start("early", &[SLOW_SESSION_ARG]);
    h.client.prompt("say early").unwrap();
    let events = h.until_turn_end();
    assert!(
        matches!(events[1], AcpEvent::SessionStarted { .. }),
        "{events:?}"
    );
    assert_eq!(said(&events), ["early"]);
}

#[test]
fn a_prompt_cancelled_before_the_session_is_never_sent() {
    let h = start("early-cancel", &[SLOW_SESSION_ARG]);
    h.client.prompt("say never").unwrap();
    h.client.cancel().unwrap();
    let events = h.until_turn_end();
    assert_eq!(stop_reason(&events), "cancelled");
    h.wait_for(|e| matches!(e, AcpEvent::SessionStarted { .. }));
    assert_eq!(said(&h.turn("say later")), ["later"]);
}

#[test]
fn crash_mid_turn_is_reported() {
    let h = ready("crash");
    h.client
        .prompt("say before\ncrash\nsay not reached")
        .unwrap();
    let deadline = Instant::now() + WAIT;
    let mut events = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match h.events.recv_timeout(left).expect("exit reported") {
            AcpEvent::Stderr(line) => stderr.push(line),
            AcpEvent::Exited(code) => {
                assert_eq!(code, Some(3));
                break;
            }
            e => events.push(e),
        }
    }
    assert_eq!(said(&events), ["before"]);
    // stderr before the exit arrives before the exit report.
    assert!(
        stderr
            .iter()
            .any(|l| l == "fake-acp-agent: crashing on request"),
        "{stderr:?}"
    );
    let err = h.client.prompt("say again").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    assert!(h.client.cancel().is_err());
}

#[test]
fn malformed_json_is_reported_and_skipped() {
    let h = ready("garbage");
    let events = h.turn("garbage\nnot-rpc\nsay still here");
    let errs = errors(&events);
    assert_eq!(errs.len(), 2, "{events:?}");
    assert!(errs[0].contains("malformed JSON"), "{errs:?}");
    assert!(errs[1].contains("not a JSON-RPC message"), "{errs:?}");
    assert_eq!(said(&events), ["still here"]);
    assert_eq!(stop_reason(&events), "end_turn");
}

#[test]
fn unknown_agent_requests_get_method_not_found() {
    let h = ready("unknown");
    assert_eq!(said(&h.turn("unknown-request")), ["unknown: -32601"]);
}

#[test]
fn a_failed_prompt_ends_the_turn_with_an_error() {
    let h = ready("fail");
    let events = h.turn("say partial\nfail");
    assert_eq!(said(&events), ["partial"]);
    assert!(errors(&events)[0].contains("fake failure"), "{events:?}");
    assert_eq!(stop_reason(&events), "error");
    assert_eq!(said(&h.turn("say recovered")), ["recovered"]);
}

#[test]
fn stderr_lines_are_forwarded() {
    let h = ready("stderr");
    h.client.prompt("stderr something odd").unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let AcpEvent::Stderr(line) = h.events.recv_timeout(left).expect("stderr line")
            && line == "something odd"
        {
            break;
        }
    }
}

#[test]
fn session_failure_is_reported_and_stops_the_agent() {
    // Slow, so the prompt is certainly queued before the failure arrives.
    let h = start("nosession", &[FAIL_SESSION_ARG, SLOW_SESSION_ARG]);
    h.client.prompt("say queued").unwrap();
    let events = h.wait_for(|e| matches!(e, AcpEvent::Exited(_)));
    let errs = errors(&events);
    assert!(errs[0].contains("session/new failed"), "{errs:?}");
    assert!(errs[0].contains("Authentication required"), "{errs:?}");
    assert!(
        events.contains(&AcpEvent::TurnEnded {
            stop_reason: "error".into()
        }),
        "{events:?}"
    );
    assert!(h.client.prompt("say again").is_err());
}

#[test]
fn other_protocol_versions_are_refused() {
    let h = start("protocol", &[PROTOCOL_ARG, "2"]);
    let events = h.wait_for(|e| matches!(e, AcpEvent::Exited(_)));
    assert!(errors(&events)[0].contains("ACP version 2"), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AcpEvent::SessionStarted { .. })),
        "{events:?}"
    );
}

#[test]
fn shutdown_kills_the_agent_and_reports_the_exit() {
    let h = ready("shutdown");
    h.client.prompt("wait-cancel").unwrap();
    h.client.shutdown();
    // Killed (None), or exited on its own when stdin closed first (Some(0)).
    let events = h.wait_for(|e| matches!(e, AcpEvent::Exited(_)));
    assert!(
        matches!(events.last(), Some(AcpEvent::Exited(None | Some(0)))),
        "{events:?}"
    );
    assert!(h.client.prompt("say x").is_err());
}

/// Whether `pid` is a live process (not a zombie).
#[cfg(target_os = "linux")]
fn running(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            !state.starts_with('Z')
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn assert_gone(pid: u32) {
    let deadline = Instant::now() + WAIT;
    while running(pid) {
        assert!(Instant::now() < deadline, "process {pid} still running");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child_pid(events: &[AcpEvent]) -> u32 {
    said(events)
        .iter()
        .find_map(|s| s.strip_prefix("child ")?.parse().ok())
        .unwrap_or_else(|| panic!("no child pid in {events:?}"))
}

#[cfg(target_os = "linux")]
#[test]
fn shutdown_stops_programs_the_agent_started() {
    let h = ready("tree");
    let pid = child_pid(&h.turn("spawn-child"));
    assert!(running(pid));
    h.client.shutdown();
    h.wait_for(|e| matches!(e, AcpEvent::Exited(_)));
    assert_gone(pid);
}

#[cfg(target_os = "linux")]
#[test]
fn exit_is_reported_while_a_child_holds_stdout() {
    let h = ready("orphan");
    h.client.prompt("orphan").unwrap();
    let events = h.wait_for(|e| matches!(e, AcpEvent::Exited(_)));
    assert_eq!(events.last(), Some(&AcpEvent::Exited(Some(4))));
    // The leftover child was stopped too, and nothing follows the exit.
    assert_gone(child_pid(&events));
    assert!(h.events.recv_timeout(Duration::from_millis(300)).is_err());
}

#[test]
fn the_callback_may_call_back_into_the_client() {
    use std::sync::{Arc, OnceLock};
    let (base, root, _) = dirs("reentrant");
    let cell: Arc<OnceLock<AcpClient>> = Arc::new(OnceLock::new());
    let (tx, events) = mpsc::channel();
    let inner = cell.clone();
    let client = AcpClient::start(&spec(&[]), &root, move |e| {
        if let AcpEvent::PermissionRequested(req) = &e {
            let client = inner.get().expect("set before any prompt");
            client
                .answer_permission(req.request_id.clone(), Some("allow"))
                .unwrap();
        }
        let _ = tx.send(e);
    })
    .unwrap();
    assert!(cell.set(client).is_ok());
    let client = cell.get().unwrap();
    client.prompt("ask Edit\nsay done").unwrap();
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left).expect("turn ends") {
            AcpEvent::TurnEnded { .. } => break,
            e => seen.push(e),
        }
    }
    assert_eq!(said(&seen), ["permission: allow", "done"]);
    client.shutdown();
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn start_fails_for_a_missing_program_or_root() {
    let (base, root, _) = dirs("missing");
    let mut s = spec(&[]);
    s.program = "maestro-definitely-not-installed".to_string();
    let err = AcpClient::start(&s, &root, |_| {}).err().unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let err = AcpClient::start(&spec(&[]), &root.join("nope"), |_| {})
        .err()
        .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(AcpClient::start(&spec(&[]), &root.join("src/lib.rs"), |_| {}).is_err());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn dropping_the_client_stops_the_agent() {
    let (base, root, _) = dirs("drop");
    let (tx, events) = mpsc::channel();
    let client = AcpClient::start(&spec(&[]), Path::new(&root), move |e| drop(tx.send(e))).unwrap();
    drop(client);
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let AcpEvent::Exited(_) = events.recv_timeout(left).expect("exit reported") {
            break;
        }
    }
    fs::remove_dir_all(base).unwrap();
}

/// Started with `load_session` for `session`.
fn loading(name: &str, extra: &[&str], session: &str) -> Harness {
    let (base, root, outside) = dirs(name);
    let (tx, events) = mpsc::channel();
    let client =
        AcpClient::load_session(&spec(extra), &root, session, move |e| drop(tx.send(e))).unwrap();
    Harness {
        client,
        events,
        root,
        outside,
        base,
    }
}

#[test]
fn a_saved_session_is_loaded_when_the_agent_supports_it() {
    let h = loading("load", &[LOAD_SESSION_ARG], SESSION_ID);
    // Sent before the load is answered: it waits for the session.
    h.client.prompt("say after the load").unwrap();
    let events = h.wait_for(|e| matches!(e, AcpEvent::SessionLoaded { .. }));
    assert_eq!(
        events,
        [
            AcpEvent::Initialized {
                agent_name: Some("fake-acp-agent".to_string()),
                load_session: true,
            },
            AcpEvent::Update(TranscriptEntry::User("earlier prompt".to_string())),
            AcpEvent::Update(TranscriptEntry::Agent(format!("replayed {SESSION_ID}"))),
            AcpEvent::SessionLoaded {
                session_id: SESSION_ID.to_string()
            },
        ]
    );
    assert_eq!(h.client.session_id().as_deref(), Some(SESSION_ID));
    let events = h.until_turn_end();
    assert_eq!(said(&events), ["after the load"]);
    assert_eq!(stop_reason(&events), "end_turn");
    // File requests naming the loaded session are served.
    let events = h.turn("write src/new.rs x");
    assert_eq!(said(&events), ["wrote src/new.rs"]);
}

#[test]
fn without_load_support_a_new_session_starts() {
    let h = loading("no-load", &[], SESSION_ID);
    let events = h.wait_for(|e| matches!(e, AcpEvent::SessionStarted { .. }));
    assert_eq!(
        events,
        [
            AcpEvent::Initialized {
                agent_name: Some("fake-acp-agent".to_string()),
                load_session: false,
            },
            AcpEvent::SessionStarted {
                session_id: SESSION_ID.to_string()
            },
        ]
    );
    assert_eq!(said(&h.turn("say fresh")), ["fresh"]);
}

#[test]
fn a_failed_load_is_reported_and_a_new_session_starts() {
    let h = loading("bad-load", &[LOAD_SESSION_ARG], "gone-session");
    let events = h.wait_for(|e| matches!(e, AcpEvent::SessionStarted { .. }));
    let errors = errors(&events);
    assert_eq!(errors.len(), 1, "{events:?}");
    assert!(
        errors[0].contains("session/load failed") && errors[0].contains("session not found"),
        "{errors:?}"
    );
    assert!(errors[0].contains("starting a new session instead"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AcpEvent::SessionLoaded { .. })),
        "{events:?}"
    );
    assert_eq!(h.client.session_id().as_deref(), Some(SESSION_ID));
    assert_eq!(said(&h.turn("say fresh")), ["fresh"]);
}
