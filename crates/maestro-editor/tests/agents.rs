//! Agent panel behavior tests: keys in through `Editor::handle_key`, real
//! Git repositories and worktrees, real agent processes.
//!
//! `harness = false`: started with `--maestro-fake-acp`, this binary is the
//! fake ACP agent instead (see `maestro_agent::fake`). Terminal agents are
//! `sh` scripts configured as providers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_agent::fake::FAKE_ARG;
use maestro_config::{AgentProviderConfig, AgentProviderKind, Config};
use maestro_editor::agents::panel::{ConfirmAction, Overlay};
use maestro_editor::agents::review::Row;
use maestro_editor::agents::{AgentId, Status, TranscriptEntry};
use maestro_editor::git::GitState;
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};
use maestro_git::Git;

fn main() {
    maestro_agent::fake::run_if_requested();
    let tests: &[(&str, fn())] = &[
        (
            "acp_agent_permission_feedback_and_review",
            acp_agent_permission_feedback_and_review,
        ),
        (
            "terminal_agent_takes_keys_and_paste_through_ctrl_backslash",
            terminal_agent_takes_keys_and_paste_through_ctrl_backslash,
        ),
        (
            "terminal_agent_gets_the_task_typed_in",
            terminal_agent_gets_the_task_typed_in,
        ),
        (
            "m2_acceptance_two_agents_reject_one_integrate_the_other",
            m2_acceptance_two_agents_reject_one_integrate_the_other,
        ),
        (
            "quitting_with_running_agents_asks_and_stops_them",
            quitting_with_running_agents_asks_and_stops_them,
        ),
        ("no_repository_is_reported", no_repository_is_reported),
        (
            "rejecting_during_creation_never_launches_and_quit_waits_for_git",
            rejecting_during_creation_never_launches_and_quit_waits_for_git,
        ),
        (
            "integrate_refuses_a_base_that_is_not_on_the_target",
            integrate_refuses_a_base_that_is_not_on_the_target,
        ),
        (
            "integrate_refuses_when_the_worktree_is_gone",
            integrate_refuses_when_the_worktree_is_gone,
        ),
        (
            "a_dropped_file_leaves_the_review_and_the_integration",
            a_dropped_file_leaves_the_review_and_the_integration,
        ),
        (
            "an_agent_that_changes_nothing_says_so",
            an_agent_that_changes_nothing_says_so,
        ),
    ];
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let selected: Vec<_> = tests
        .iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|f| name.contains(f)))
        .collect();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in &selected {
        let ok = std::panic::catch_unwind(test).is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failed.push(*name);
        }
    }
    let passed = selected.len() - failed.len();
    println!(
        "\ntest result: {}. {passed} passed; {} failed\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}

const WAIT: Duration = Duration::from_secs(20);

static NEXT: AtomicUsize = AtomicUsize::new(0);

const GIT_CONFIG: &[&str] = &[
    "user.name=Maestro Test",
    "user.email=test@example.invalid",
    "init.defaultBranch=main",
    "commit.gpgSign=false",
];

struct Fixture {
    tmp: PathBuf,
    repo: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

impl Fixture {
    /// A repository `repo` with one commit holding `README.md` and
    /// `src/lib.rs`. Agent worktrees go next to it, in `repo-worktrees`.
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-agents-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let f = Fixture { tmp, repo };
        f.git(&f.repo, &["init", "-q"]);
        std::fs::write(f.repo.join("README.md"), "# demo\n").unwrap();
        std::fs::write(f.repo.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        f.git(&f.repo, &["add", "-A"]);
        f.git(&f.repo, &["commit", "-q", "-m", "initial"]);
        f
    }

    fn cmd(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new("git");
        for c in GIT_CONFIG {
            cmd.arg("-c").arg(c);
        }
        cmd.args(args)
            .current_dir(dir)
            .env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.cmd(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn api(&self) -> Git {
        let mut g = Git::new()
            .env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env("GIT_CEILING_DIRECTORIES", &self.tmp);
        for c in GIT_CONFIG {
            g = g.config(*c);
        }
        g
    }

    fn editor(&self, config: Config) -> Editor {
        let mut ed = Editor::new(config, Rect::new(0, 1, 120, 38));
        ed.git = GitState::new(self.api());
        ed.cwd = self.repo.clone();
        ed
    }
}

fn provider(kind: AgentProviderKind, command: &str, args: &[&str]) -> AgentProviderConfig {
    AgentProviderConfig {
        kind,
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Default::default(),
        enabled: true,
    }
}

/// A config with these providers and the built-in ones hidden, so the
/// tests never start a real agent that happens to be installed.
fn config(providers: &[(&str, AgentProviderConfig)]) -> Config {
    let mut c = Config::default();
    for (name, p) in providers {
        c.agents.provider.insert(name.to_string(), p.clone());
    }
    for builtin in ["gemini", "opencode", "claude", "codex"] {
        c.agents
            .provider
            .entry(builtin.into())
            .or_insert_with(|| AgentProviderConfig {
                enabled: false,
                ..provider(AgentProviderKind::Terminal, builtin, &[])
            });
    }
    c
}

fn keys(script: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
            out.push(name.parse().unwrap_or_else(|_| panic!("bad key {name}")));
        } else if c == '\n' {
            out.push(KeyEvent::plain(KeyCode::Enter));
        } else {
            out.push(KeyEvent::char(c));
        }
    }
    out
}

fn run(ed: &mut Editor, script: &str) {
    ed.feed_keys(&keys(script));
}

/// Polls background work until `done` holds, as the terminal loop does.
fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        if Instant::now() > deadline {
            let agents: Vec<String> = ed
                .agents
                .agents
                .iter()
                .map(|a| format!("{} {:?} {:?} {:?}", a.name, a.status(), a.errors, a.history))
                .collect();
            panic!(
                "timed out waiting for {what}; status {:?}; agents {agents:#?}",
                ed.status
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn agent(ed: &Editor, i: usize) -> &maestro_editor::agents::Agent {
    &ed.agents.agents[i]
}

fn id(ed: &Editor, i: usize) -> AgentId {
    agent(ed, i).id
}

fn status(ed: &Editor, i: usize) -> Status {
    agent(ed, i).status()
}

fn worktree(ed: &Editor, i: usize) -> PathBuf {
    agent(ed, i).worktree.as_ref().unwrap().path.clone()
}

fn screen_text(ed: &Editor, i: usize) -> String {
    let t = agent(ed, i).runtime.terminal().expect("a terminal agent");
    let s = t.snapshot();
    (0..s.rows)
        .map(|r| s.row_text(r))
        .collect::<Vec<_>>()
        .join("\n")
}

fn review_paths(ed: &Editor, i: usize) -> Vec<String> {
    agent(ed, i)
        .review
        .files
        .iter()
        .filter_map(|f| f.path().map(|p| p.display().to_string()))
        .collect()
}

/// Starts a task through the panel: `n`, the task, then the provider at
/// `provider_index` in the list.
fn start_task(ed: &mut Editor, task: &str, provider_index: usize) {
    run(ed, "n");
    ed.paste_text(task);
    run(ed, "\n");
    assert!(
        matches!(ed.agents.panel.overlay, Some(Overlay::Choice { .. })),
        "provider list after the task; status {:?}",
        ed.status
    );
    for _ in 0..provider_index {
        run(ed, "j");
    }
    run(ed, "\n");
}

fn acp_agent_permission_feedback_and_review() {
    let f = Fixture::new();
    let exe = std::env::current_exe().unwrap();
    let mut ed = f.editor(config(&[(
        "fake",
        provider(AgentProviderKind::Acp, exe.to_str().unwrap(), &[FAKE_ARG]),
    )]));
    run(&mut ed, " a");
    assert!(ed.agents.panel_open());
    // The task is the fake agent's script.
    start_task(
        &mut ed,
        "plan look at lib | edit lib\n\
         tool t1 edit src/lib.rs Edit lib.rs\n\
         ask Edit lib.rs\n\
         write src/lib.rs pub fn one() {}\\npub fn two() {}\\n\n\
         say done",
        0,
    );
    poll_until(&mut ed, "the permission request", |ed| {
        status(ed, 0) == Status::AwaitingApproval
    });
    let a = agent(&ed, 0);
    assert_eq!(a.permissions.len(), 1);
    assert_eq!(a.kind(), maestro_editor::agents::ProviderKind::Acp);
    assert!(
        a.transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Plan(p) if p.len() == 2)),
        "{:?}",
        a.transcript
    );
    assert!(a.history.iter().any(|h| h.text.contains("sent a plan")));
    assert!(a.history.iter().any(|h| h.text.contains("tool t1 (edit)")));
    // The worktree is a sibling of the repository, on a maestro/ branch.
    let wt = worktree(&ed, 0);
    assert!(wt.starts_with(f.tmp.join("repo-worktrees")), "{wt:?}");
    let branch = &a.worktree.as_ref().unwrap().branch;
    assert!(branch.starts_with("maestro/plan-look-at-lib"), "{branch}");

    // Answer it from the panel: p lists the choices, allowing asks first.
    run(&mut ed, "p");
    assert!(matches!(
        ed.agents.panel.overlay,
        Some(Overlay::Choice { .. })
    ));
    run(&mut ed, "\n");
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => {
            assert!(matches!(c.action, ConfirmAction::Grant { .. }));
            assert!(c.lines.iter().any(|l| l.contains("Edit lib.rs")), "{c:?}");
        }
        other => panic!("expected a grant confirmation, got {other:?}"),
    }
    // Nothing is sent before the confirmation.
    assert!(!agent(&ed, 0).permissions.is_empty());
    run(&mut ed, "y");
    assert!(agent(&ed, 0).permissions.is_empty());
    poll_until(&mut ed, "the end of the turn", |ed| {
        status(ed, 0) == Status::ReadyForReview
    });
    let said: String = agent(&ed, 0)
        .transcript
        .iter()
        .filter_map(|e| match e {
            TranscriptEntry::Agent(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(said.contains("permission: allow"), "{said}");
    assert!(said.contains("wrote src/lib.rs"), "{said}");
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.rs")).unwrap(),
        "pub fn one() {}\npub fn two() {}\n"
    );
    // The main worktree is untouched.
    assert_eq!(
        std::fs::read_to_string(f.repo.join("src/lib.rs")).unwrap(),
        "pub fn one() {}\n"
    );

    // Review: the written file shows in the diff.
    run(&mut ed, "\n");
    assert!(ed.agents.panel.review.is_some());
    poll_until(&mut ed, "the review diff", |ed| {
        review_paths(ed, 0) == ["src/lib.rs"]
    });
    let rows = maestro_editor::agents::review::current_rows(&ed);
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::Line(l) if l.text == "pub fn two() {}")),
        "{rows:?}"
    );
    assert_eq!(agent(&ed, 0).review.totals(), (1, 1, 0));
    // A comment on the added line, then feedback with typed text. The fake
    // agent runs the prompt as a script, so the text is a `say` command.
    let added = rows
        .iter()
        .position(|r| matches!(r, Row::Line(l) if l.text == "pub fn two() {}"))
        .unwrap();
    for _ in 0..added {
        run(&mut ed, "j");
    }
    run(&mut ed, "c");
    ed.paste_text("name it better");
    run(&mut ed, "\n");
    let c = &agent(&ed, 0).comments;
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].line, c[0].old), (2, false));
    assert!(
        maestro_editor::agents::review::current_rows(&ed)
            .iter()
            .any(|r| matches!(r, Row::Comment { text, .. } if text == "name it better"))
    );
    run(&mut ed, "f");
    ed.paste_text("say feedback received");
    run(&mut ed, "\n");
    assert!(agent(&ed, 0).comments[0].sent);
    poll_until(&mut ed, "the feedback turn", |ed| {
        status(ed, 0) == Status::ReadyForReview
            && agent(ed, 0)
                .transcript
                .iter()
                .any(|e| matches!(e, TranscriptEntry::Agent(t) if t.contains("feedback received")))
    });
    let prompt = agent(&ed, 0)
        .transcript
        .iter()
        .rev()
        .find_map(|e| match e {
            TranscriptEntry::User(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        prompt.contains("- src/lib.rs:2: name it better"),
        "{prompt}"
    );
    // Side-by-side layout keeps the line pairs.
    run(&mut ed, "s");
    assert!(
        maestro_editor::agents::review::current_rows(&ed)
            .iter()
            .any(|r| matches!(r, Row::Pair(None, Some(l)) if l.text == "pub fn two() {}"))
    );
    // Enter opens the worktree's file at that line in a normal buffer.
    run(&mut ed, "\n");
    assert!(!ed.agents.panel_open());
    assert_eq!(ed.doc().path(), Some(wt.join("src/lib.rs").as_path()));
    assert_eq!(ed.doc().text().char_to_line(ed.primary().head), 1);
    let owner = ed.agents.owner_of(&wt.join("src/lib.rs")).unwrap();
    assert_eq!(owner.id, id(&ed, 0));
}

fn terminal_agent_takes_keys_and_paste_through_ctrl_backslash() {
    let f = Fixture::new();
    // `{task}` is an argument, so nothing is typed in for it.
    let script =
        "printf 'ready\\n'; read line; printf '%s\\n' \"$line\" > typed.txt; printf 'got it\\n'";
    let mut ed = f.editor(config(&[(
        "shell",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", script, "sh", "{task}"],
        ),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "type a line", 0);
    poll_until(&mut ed, "the script's prompt", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.runtime.terminal().is_some())
            && screen_text(ed, 0).contains("ready")
    });
    assert_eq!(status(&ed, 0), Status::Running);
    // Keys belong to the panel until C-\.
    assert!(ed.agents.panel.terminal_focus.is_none());
    ed.handle_key(KeyEvent::ctrl('\\'));
    assert_eq!(ed.agents.panel.terminal_focus, Some(id(&ed, 0)));
    run(&mut ed, "hel");
    ed.paste_text("lo wor");
    run(&mut ed, "ld");
    poll_until(&mut ed, "the echo of the typed keys", |ed| {
        screen_text(ed, 0).contains("hello world")
    });
    // C-\ arrives from terminals as C-4; it takes the keys back too.
    ed.handle_key(KeyEvent::ctrl('4'));
    assert!(ed.agents.panel.terminal_focus.is_none());
    // Panel keys now: `v` changes the view and reaches no program.
    run(&mut ed, "v");
    assert_eq!(
        ed.agents.panel.view,
        maestro_editor::agents::panel::DetailView::History
    );
    run(&mut ed, "vv");
    ed.handle_key(KeyEvent::ctrl('\\'));
    run(&mut ed, "\n");
    poll_until(&mut ed, "the program's exit", |ed| {
        status(ed, 0) == Status::ReadyForReview
    });
    assert!(
        ed.agents.panel.terminal_focus.is_none(),
        "keys return to the panel when the program exits"
    );
    let wt = worktree(&ed, 0);
    assert_eq!(
        std::fs::read_to_string(wt.join("typed.txt")).unwrap(),
        "hello world\n"
    );
    poll_until(&mut ed, "the review diff", |ed| {
        review_paths(ed, 0) == ["typed.txt"]
    });
    assert!(agent(&ed, 0).exited.as_deref().unwrap().contains("0"));
}

fn terminal_agent_gets_the_task_typed_in() {
    let f = Fixture::new();
    let script = "read line; printf '%s\\n' \"$line\" > task.txt";
    let mut ed = f.editor(config(&[(
        "reader",
        provider(AgentProviderKind::Terminal, "sh", &["-c", script]),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "write the notes", 0);
    poll_until(&mut ed, "the program's exit", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    assert_eq!(status(&ed, 0), Status::ReadyForReview);
    assert_eq!(
        std::fs::read_to_string(worktree(&ed, 0).join("task.txt")).unwrap(),
        "write the notes\n"
    );
}

fn m2_acceptance_two_agents_reject_one_integrate_the_other() {
    let f = Fixture::new();
    let alpha = "printf 'alpha\\n' > notes.txt";
    let beta = "printf 'beta was here\\n' >> README.md";
    let mut ed = f.editor(config(&[
        (
            "alpha",
            provider(
                AgentProviderKind::Terminal,
                "sh",
                &["-c", alpha, "sh", "{task}"],
            ),
        ),
        (
            "beta",
            provider(
                AgentProviderKind::Terminal,
                "sh",
                &["-c", beta, "sh", "{task}"],
            ),
        ),
    ]));
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    run(&mut ed, " a");
    start_task(&mut ed, "write release notes", 0);
    poll_until(&mut ed, "the first worktree", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.worktree.is_some())
    });
    // A competing implementation of the same task: N offers the other
    // provider only.
    run(&mut ed, "N");
    match &ed.agents.panel.overlay {
        Some(Overlay::Choice { items, .. }) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].0, "beta");
        }
        other => panic!("expected the provider list, got {other:?}"),
    }
    run(&mut ed, "\n");
    assert_eq!(ed.agents.agents.len(), 2);
    assert_eq!(agent(&ed, 0).group, agent(&ed, 1).group);
    assert_eq!(agent(&ed, 1).task, "write release notes");
    poll_until(&mut ed, "both programs to finish and both diffs", |ed| {
        ed.agents.agents.iter().all(|a| {
            a.exited.is_some() && a.review.computed.is_some() && !a.review.files.is_empty()
        })
    });
    let (wt_a, wt_b) = (worktree(&ed, 0), worktree(&ed, 1));
    assert_ne!(wt_a, wt_b);
    assert_eq!(
        agent(&ed, 0).worktree.as_ref().unwrap().base,
        agent(&ed, 1).worktree.as_ref().unwrap().base
    );
    // Side by side in the list: files, lines, and status.
    assert_eq!(agent(&ed, 0).review.totals(), (1, 1, 0));
    assert_eq!(agent(&ed, 1).review.totals(), (1, 1, 0));

    // Review both.
    run(&mut ed, "k\n");
    assert_eq!(ed.agents.panel.review.as_ref().unwrap().agent, id(&ed, 0));
    poll_until(&mut ed, "alpha's review", |ed| {
        maestro_editor::agents::review::current_rows(ed)
            .iter()
            .any(|r| matches!(r, Row::Line(l) if l.text == "alpha"))
    });
    run(&mut ed, "<esc>j\n");
    assert_eq!(ed.agents.panel.review.as_ref().unwrap().agent, id(&ed, 1));
    poll_until(&mut ed, "beta's review", |ed| {
        maestro_editor::agents::review::current_rows(ed)
            .iter()
            .any(|r| matches!(r, Row::Line(l) if l.text == "beta was here"))
    });
    run(&mut ed, "<esc>");

    // Reject beta and archive its worktree. Its change is uncommitted, so
    // the plain removal is refused and forcing asks again, naming the file.
    run(&mut ed, "X");
    assert!(matches!(
        &ed.agents.panel.overlay,
        Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Reject(_))
    ));
    run(&mut ed, "a");
    assert_eq!(status(&ed, 1), Status::Rejected);
    poll_until(&mut ed, "the force confirmation", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::ForceArchive(_)))
    });
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => assert!(
            c.lines.iter().any(|l| l.contains("README.md")),
            "{:?}",
            c.lines
        ),
        _ => unreachable!(),
    }
    assert!(
        wt_b.exists(),
        "nothing removed before the second confirmation"
    );
    // `y` is not the key for forcing.
    run(&mut ed, "y");
    assert!(ed.agents.panel.overlay.is_some());
    run(&mut ed, "F");
    poll_until(&mut ed, "the archive", |ed| agent(ed, 1).archived);
    assert!(!wt_b.exists());
    let list = f.git(&f.repo, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(wt_b.to_str().unwrap()), "{list}");
    assert!(list.contains(wt_a.to_str().unwrap()), "{list}");

    // Integrate alpha. First with the main worktree dirty: refused before
    // anything is committed anywhere.
    std::fs::write(f.repo.join("src/lib.rs"), "pub fn one() {} // local edit\n").unwrap();
    run(&mut ed, "k");
    assert_eq!(ed.agents.selected().unwrap().id, id(&ed, 0));
    run(&mut ed, "I");
    poll_until(&mut ed, "the refusal", |ed| {
        ed.status
            .as_ref()
            .is_some_and(|(_, m)| m.contains("not integrated"))
    });
    let msg = ed.status.clone().unwrap().1;
    assert!(
        msg.contains("uncommitted changes in src/lib.rs") && msg.contains("nothing changed"),
        "{msg}"
    );
    assert!(ed.agents.panel.overlay.is_none());
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    assert_eq!(
        f.git(&wt_a, &["status", "--porcelain"]),
        "?? notes.txt\n",
        "the agent's work is still uncommitted"
    );
    f.git(&f.repo, &["checkout", "--", "src/lib.rs"]);

    // Open the agent's file from the review and leave an unsaved edit:
    // integration refuses until it is saved.
    run(&mut ed, "\n");
    poll_until(&mut ed, "alpha's review", |ed| {
        !maestro_editor::agents::review::current_rows(ed).is_empty()
    });
    run(&mut ed, "jj\n");
    assert_eq!(ed.doc().path(), Some(wt_a.join("notes.txt").as_path()));
    run(&mut ed, "oreviewed<esc>");
    run(&mut ed, " aI");
    let msg = ed.status.clone().unwrap().1;
    assert!(msg.contains("unsaved edits in"), "{msg}");
    run(&mut ed, ":w\n");
    assert!(!ed.doc().is_modified());

    run(&mut ed, "I");
    poll_until(&mut ed, "the integrate dialog", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Integrate { .. }))
    });
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => {
            let text = c.lines.join("\n");
            assert!(text.contains("When checked: notes.txt."), "{text}");
            assert!(text.contains("Merge maestro/write-release-notes"), "{text}");
            assert!(text.contains("into main"), "{text}");
        }
        _ => unreachable!(),
    }
    run(&mut ed, "y");
    poll_until(&mut ed, "the integration", |ed| {
        status(ed, 0) == Status::Integrated
    });
    assert_eq!(
        std::fs::read_to_string(f.repo.join("notes.txt")).unwrap(),
        "alpha\nreviewed\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("README.md")).unwrap(),
        "# demo\n",
        "the rejected agent's change is not on main"
    );
    assert_eq!(f.git(&f.repo, &["status", "--porcelain"]), "");
    let log = f.git(&f.repo, &["log", "--topo-order", "--format=%s", "main"]);
    let subjects: Vec<&str> = log.lines().collect();
    assert_eq!(subjects.len(), 3, "{log}");
    assert!(
        subjects[0].starts_with("Merge branch 'maestro/write-release-notes"),
        "{log}"
    );
    assert_eq!(subjects[1], "write release notes");
    assert_eq!(subjects[2], "initial");
    // The rejected branch is kept, still at the base: its uncommitted
    // work was deleted with the worktree after the second confirmation.
    let beta_branch = agent(&ed, 1).worktree.as_ref().unwrap().branch.clone();
    assert_eq!(f.git(&f.repo, &["rev-parse", &beta_branch]), main_head);
}

fn quitting_with_running_agents_asks_and_stops_them() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "sleeper",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "sleep 30", "sh", "{task}"],
        ),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "wait", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.is_running())
    });
    run(&mut ed, ":qa\n");
    assert!(!ed.should_quit);
    assert!(
        ed.pending_char_label()
            .is_some_and(|l| l.contains("1 agent(s) running")),
        "{:?}",
        ed.pending_char_label()
    );
    run(&mut ed, "n");
    assert!(!ed.should_quit);
    assert!(agent(&ed, 0).is_running());
    run(&mut ed, ":qa\ny");
    assert!(ed.should_quit);
    // Quitting waits for the program to exit before the editor returns,
    // so the front end can end the process right after.
    assert!(
        !agent(&ed, 0).is_running(),
        "the program still runs when quit returns"
    );
}

fn no_repository_is_reported() {
    let f = Fixture::new();
    let outside = f.tmp.join("plain");
    std::fs::create_dir_all(&outside).unwrap();
    let mut ed = f.editor(config(&[(
        "shell",
        provider(AgentProviderKind::Terminal, "sh", &["-c", "true"]),
    )]));
    ed.cwd = outside;
    run(&mut ed, " a");
    start_task(&mut ed, "anything", 0);
    poll_until(&mut ed, "the failure", |ed| status(ed, 0) == Status::Failed);
    let a = agent(&ed, 0);
    assert!(a.worktree.is_none());
    assert!(
        a.errors
            .iter()
            .any(|e| e.contains("not in a Git repository")),
        "{:?}",
        a.errors
    );
}

fn rejecting_during_creation_never_launches_and_quit_waits_for_git() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "writer",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf x > late.txt", "sh", "{task}"],
        ),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "never start", 0);
    // The worktree is still being made on a background thread.
    assert!(agent(&ed, 0).worktree.is_none());
    run(&mut ed, ":qa\n");
    assert!(!ed.should_quit);
    let msg = ed.status.clone().unwrap().1;
    assert!(msg.contains("Git work is running"), "{msg}");
    run(&mut ed, "Xy");
    assert_eq!(status(&ed, 0), Status::Rejected);
    poll_until(&mut ed, "the worktree", |ed| {
        agent(ed, 0).worktree.is_some()
    });
    let a = agent(&ed, 0);
    assert!(
        a.runtime.terminal().is_none(),
        "a rejected agent was launched"
    );
    assert!(a.history.iter().any(|h| h.text.contains("not launched")));
    assert!(!worktree(&ed, 0).join("late.txt").exists());
    run(&mut ed, ":qa\n");
    assert!(ed.should_quit);
}

fn integrate_refuses_a_base_that_is_not_on_the_target() {
    let f = Fixture::new();
    // The editor runs in a linked worktree on a feature branch that is one
    // commit ahead of main.
    let feature = f.tmp.join("feature");
    f.git(
        &f.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().unwrap(),
        ],
    );
    std::fs::write(feature.join("feature.txt"), "unreviewed\n").unwrap();
    f.git(&feature, &["add", "-A"]);
    f.git(&feature, &["commit", "-q", "-m", "feature work"]);
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    let mut ed = f.editor(config(&[(
        "alpha",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf a > a.txt", "sh", "{task}"],
        ),
    )]));
    ed.cwd = feature.clone();
    run(&mut ed, " a");
    start_task(&mut ed, "add a", 0);
    poll_until(&mut ed, "the agent's exit", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    // The review shows only the agent's file.
    poll_until(&mut ed, "the review", |ed| review_paths(ed, 0) == ["a.txt"]);
    run(&mut ed, "I");
    poll_until(&mut ed, "the refusal", |ed| {
        ed.status
            .as_ref()
            .is_some_and(|(_, m)| m.contains("not integrated"))
    });
    let msg = ed.status.clone().unwrap().1;
    assert!(msg.contains("which is not on main"), "{msg}");
    assert!(ed.agents.panel.overlay.is_none());
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    assert_eq!(
        f.git(&worktree(&ed, 0), &["status", "--porcelain"]),
        "?? a.txt\n"
    );
}

fn integrate_refuses_when_the_worktree_is_gone() {
    let f = Fixture::new();
    // Worktrees inside the repository: if the agent's directory vanishes,
    // Git discovery from it finds the main repository instead.
    let mut c = config(&[(
        "alpha",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf a > a.txt", "sh", "{task}"],
        ),
    )]);
    c.agents.worktree_dir = Some(".agents".into());
    let mut ed = f.editor(c);
    run(&mut ed, " a");
    start_task(&mut ed, "add a", 0);
    poll_until(&mut ed, "the agent's exit", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    let wt = worktree(&ed, 0);
    assert!(wt.starts_with(f.repo.join(".agents")), "{wt:?}");
    std::fs::write(f.repo.join("untracked-in-main.txt"), "mine\n").unwrap();
    run(&mut ed, "I");
    poll_until(&mut ed, "the integrate dialog", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Integrate { .. }))
    });
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    std::fs::remove_dir_all(&wt).unwrap();
    run(&mut ed, "y");
    poll_until(&mut ed, "the refusal", |ed| {
        ed.status
            .as_ref()
            .is_some_and(|(_, m)| m.contains("not integrated"))
    });
    let msg = ed.status.clone().unwrap().1;
    assert!(msg.contains("no longer a worktree"), "{msg}");
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    assert!(
        f.git(&f.repo, &["status", "--porcelain"])
            .contains("?? untracked-in-main.txt"),
        "the main worktree's own files were not committed"
    );
}

/// `D` in the review discards one file's change in the agent's worktree,
/// after a confirmation, and integration then leaves it out.
fn a_dropped_file_leaves_the_review_and_the_integration() {
    let f = Fixture::new();
    let script = "printf 'wanted\\n' > notes.txt; printf 'cache' > junk.pyc";
    let mut ed = f.editor(config(&[(
        "writer",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", script, "sh", "{task}"],
        ),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "write notes", 0);
    poll_until(&mut ed, "the program and its diff", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.exited.is_some() && a.review.files.len() == 2)
    });
    let wt = worktree(&ed, 0);
    run(&mut ed, "\n");
    // Files are listed by path: junk.pyc comes before notes.txt.
    assert_eq!(review_paths(&ed, 0), ["junk.pyc", "notes.txt"]);
    run(&mut ed, "D");
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => {
            let text = c.lines.join(" ");
            assert!(
                text.contains("junk.pyc is deleted; it did not exist at"),
                "{text}"
            );
        }
        other => panic!("expected a confirmation, got {other:?}"),
    }
    run(&mut ed, "n");
    assert!(wt.join("junk.pyc").exists(), "cancel must change nothing");
    run(&mut ed, "Dy");
    poll_until(&mut ed, "the file to be dropped", |ed| {
        ed.agents.agents[0].review.files.len() == 1 && !ed.agents.agents[0].review.running
    });
    assert!(!wt.join("junk.pyc").exists());
    assert_eq!(review_paths(&ed, 0), ["notes.txt"]);

    // The integrate dialog names what will be committed.
    run(&mut ed, "I");
    poll_until(&mut ed, "the integrate dialog", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Integrate { .. }))
    });
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => {
            let text = c.lines.join(" ");
            assert!(text.contains("When checked: notes.txt."), "{text}");
        }
        other => panic!("expected the integrate dialog, got {other:?}"),
    }
    run(&mut ed, "y");
    poll_until(&mut ed, "the integration", |ed| {
        ed.agents.agents[0].status() == Status::Integrated
    });
    let tree = f.git(&f.repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.lines().any(|l| l == "notes.txt"), "{tree}");
    assert!(!tree.lines().any(|l| l == "junk.pyc"), "{tree}");
}

/// Exit code 0 with an empty review diff is "finished, no changes", not
/// "ready for review"; a change made afterward flips it back.
fn an_agent_that_changes_nothing_says_so() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "idle",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "true", "sh", "{task}"],
        ),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "do nothing", 0);
    poll_until(&mut ed, "the empty review", |ed| {
        status(ed, 0) == Status::Completed
    });
    assert_eq!(Status::Completed.label(), "finished, no changes");
    std::fs::write(worktree(&ed, 0).join("later.txt"), "x\n").unwrap();
    run(&mut ed, "\nr");
    poll_until(&mut ed, "the changed review", |ed| {
        status(ed, 0) == Status::ReadyForReview
    });
    // Reject and archive from the list, as for any finished agent.
    std::fs::remove_file(worktree(&ed, 0).join("later.txt")).unwrap();
    run(&mut ed, "<esc>X");
    assert!(
        matches!(&ed.agents.panel.overlay, Some(Overlay::Confirm(c))
            if matches!(c.action, ConfirmAction::Reject(_))),
        "{:?}",
        ed.agents.panel.overlay
    );
    run(&mut ed, "a");
    poll_until(&mut ed, "the archive", |ed| agent(ed, 0).archived);
    assert_eq!(status(&ed, 0), Status::Rejected);
    assert!(!worktree(&ed, 0).exists());
}
