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
        (
            "dropping_refuses_when_the_worktree_changed_branch",
            dropping_refuses_when_the_worktree_changed_branch,
        ),
        (
            "records_survive_a_restart_and_loaded_agents_can_be_finished",
            records_survive_a_restart_and_loaded_agents_can_be_finished,
        ),
        (
            "found_worktrees_are_offered_archived_and_adopted",
            found_worktrees_are_offered_archived_and_adopted,
        ),
        (
            "an_unusable_records_file_is_reported_and_left_alone",
            an_unusable_records_file_is_reported_and_left_alone,
        ),
        (
            "without_sessions_no_records_are_read_or_written",
            without_sessions_no_records_are_read_or_written,
        ),
        (
            "a_loaded_terminal_agent_relaunches_without_its_task",
            a_loaded_terminal_agent_relaunches_without_its_task,
        ),
        (
            "a_loaded_acp_agent_relaunches_into_its_saved_session",
            a_loaded_acp_agent_relaunches_into_its_saved_session,
        ),
        (
            "a_loaded_record_whose_worktree_changed_is_not_archived",
            a_loaded_record_whose_worktree_changed_is_not_archived,
        ),
        (
            "a_worktree_switched_after_loading_is_not_touched",
            a_worktree_switched_after_loading_is_not_touched,
        ),
        (
            "records_written_by_another_maestro_are_kept",
            records_written_by_another_maestro_are_kept,
        ),
        (
            "contracts_typed_and_from_a_file_are_shown_and_sent",
            contracts_typed_and_from_a_file_are_shown_and_sent,
        ),
        (
            "acp_writes_outside_the_contract_are_refused",
            acp_writes_outside_the_contract_are_refused,
        ),
        (
            "out_of_scope_files_need_their_own_key_and_verification_is_checked",
            out_of_scope_files_need_their_own_key_and_verification_is_checked,
        ),
        (
            "files_outside_the_contract_written_after_the_dialog_stop_the_integration",
            files_outside_the_contract_written_after_the_dialog_stop_the_integration,
        ),
        (
            "history_outside_the_contract_is_listed_in_full",
            history_outside_the_contract_is_listed_in_full,
        ),
        (
            "overlapping_agents_show_conflicts_only_where_they_conflict",
            overlapping_agents_show_conflicts_only_where_they_conflict,
        ),
        (
            "contracts_and_verification_survive_a_restart",
            contracts_and_verification_survive_a_restart,
        ),
        (
            "a_verification_that_leaves_a_process_behind_ends_it",
            a_verification_that_leaves_a_process_behind_ends_it,
        ),
        (
            "a_branch_that_moved_after_the_dialog_is_not_integrated",
            a_branch_that_moved_after_the_dialog_is_not_integrated,
        ),
        (
            "a_target_that_changed_during_the_commit_is_not_integrated_into",
            a_target_that_changed_during_the_commit_is_not_integrated_into,
        ),
        (
            "marked_hunks_integrate_alone_and_the_rest_stays",
            marked_hunks_integrate_alone_and_the_rest_stays,
        ),
        (
            "a_marked_hunk_that_needs_an_unmarked_one_is_refused",
            a_marked_hunk_that_needs_an_unmarked_one_is_refused,
        ),
        (
            "composing_hunks_of_two_agents_tests_and_integrates_the_result",
            composing_hunks_of_two_agents_tests_and_integrates_the_result,
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

    /// The state directory editors of this fixture save into.
    fn state(&self) -> PathBuf {
        self.tmp.join("state")
    }

    /// An editor with sessions on, as the terminal front end configures
    /// it: agent records are read from and written to [`Self::state`].
    fn session_editor(&self, config: Config) -> Editor {
        let mut ed = self.editor(config);
        maestro_editor::navigation::configure(&mut ed, Some(self.state()), true);
        ed
    }

    fn records(&self) -> PathBuf {
        maestro_editor::agents::persist::records_path(&self.state(), &self.repo)
    }
}

/// Opens the panel and waits for the saved records and found worktrees.
fn open_and_load(ed: &mut Editor, expect: usize) {
    run(ed, " a");
    poll_until(ed, "the saved records", |ed| {
        !ed.agents.persist.loading && ed.agents.agents.len() == expect
    });
}

fn status_text(ed: &Editor) -> String {
    ed.status
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_default()
}

fn hint_keys(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    maestro_editor::agents::panel::hints(ed)
}

/// Moves the panel's selection to agent `i` with `j`/`k`.
fn select(ed: &mut Editor, i: usize) {
    while ed.agents.panel.selected > i {
        run(ed, "k");
    }
    while ed.agents.panel.selected < i {
        run(ed, "j");
    }
}

fn index_of(ed: &Editor, name: &str) -> usize {
    ed.agents
        .agents
        .iter()
        .position(|a| a.name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = ed.agents.agents.iter().map(|a| a.name.as_str()).collect();
            panic!("no agent {name} in {names:?}")
        })
}

fn quit(ed: &mut Editor) {
    run(ed, ":qa\n");
    if !ed.should_quit && ed.pending_char_label().is_some() {
        run(ed, "y");
    }
    assert!(ed.should_quit, "status {:?}", ed.status);
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
            assert!(
                text.contains("When checked (1 in all; the review lists every one): notes.txt."),
                "{text}"
            );
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
    // The drop is Git work: integrating waits for it, and so does quitting.
    assert_eq!(agent(&ed, 0).git_busy, Some("dropping a file"));
    assert_eq!(ed.agents.git_busy().len(), 1);
    run(&mut ed, "I");
    assert!(
        ed.status
            .as_ref()
            .is_some_and(|(_, m)| m.contains("busy (dropping a file)")),
        "{:?}",
        ed.status
    );
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
            assert!(
                text.contains("When checked (1 in all; the review lists every one): notes.txt."),
                "{text}"
            );
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

/// A drop is refused, changing nothing, when the worktree no longer has
/// the agent's branch checked out: the dialog promised that branch's
/// starting commit.
fn dropping_refuses_when_the_worktree_changed_branch() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "writer",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf 'x\\n' > new.txt", "sh", "{task}"],
        ),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "write", 0);
    poll_until(&mut ed, "the program and its diff", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.exited.is_some() && a.review.files.len() == 1)
    });
    let wt = worktree(&ed, 0);
    run(&mut ed, "\nD");
    // While the dialog is open, the worktree moves to another branch.
    f.git(&wt, &["checkout", "-q", "-b", "elsewhere"]);
    run(&mut ed, "y");
    poll_until(&mut ed, "the refusal", |ed| {
        ed.status
            .as_ref()
            .is_some_and(|(_, m)| m.contains("the agent's worktree is on"))
    });
    assert!(wt.join("new.txt").exists(), "nothing may change");
    assert_eq!(agent(&ed, 0).git_busy, None);
}

fn two_writers() -> Config {
    let alpha = "printf 'alpha\\n' > notes.txt";
    let beta = "printf 'beta was here\\n' >> README.md";
    let mut c = config(&[
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
    ]);
    c.agents.test_command = vec![
        "sh".into(),
        "-c".into(),
        "echo 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'".into(),
    ];
    c
}

/// Save on quit, load in a new editor: records, comments, test summaries
/// and status come back with no process and say so; review, tests,
/// integrate and archive work on them; new ids continue after theirs.
fn records_survive_a_restart_and_loaded_agents_can_be_finished() {
    let f = Fixture::new();
    let c = two_writers();
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "write release notes", 0);
    poll_until(&mut ed, "the first worktree", |ed| {
        agent(ed, 0).worktree.is_some()
    });
    start_task(&mut ed, "touch the readme", 1);
    poll_until(&mut ed, "both programs and diffs", |ed| {
        ed.agents.agents.len() == 2
            && ed.agents.agents.iter().all(|a| {
                a.exited.is_some() && a.review.computed.is_some() && !a.review.files.is_empty()
            })
    });
    // A comment on alpha's change and a test run.
    select(&mut ed, 0);
    run(&mut ed, "\n");
    poll_until(&mut ed, "alpha's review", |ed| {
        maestro_editor::agents::review::current_rows(ed)
            .iter()
            .any(|r| matches!(r, Row::Line(l) if l.text == "alpha"))
    });
    let at = maestro_editor::agents::review::current_rows(&ed)
        .iter()
        .position(|r| matches!(r, Row::Line(l) if l.text == "alpha"))
        .unwrap();
    for _ in 0..at {
        run(&mut ed, "j");
    }
    run(&mut ed, "c");
    ed.paste_text("keep this");
    run(&mut ed, "\n<esc>t");
    poll_until(&mut ed, "the test run", |ed| {
        agent(ed, 0)
            .latest_test()
            .is_some_and(|t| t.finished.is_some())
    });
    let (wt_a, wt_b) = (worktree(&ed, 0), worktree(&ed, 1));
    let names: Vec<String> = ed.agents.agents.iter().map(|a| a.name.clone()).collect();
    assert_eq!(names, ["alpha-1", "beta-2"]);
    quit(&mut ed);
    drop(ed);
    let path = f.records();
    assert!(path.is_file(), "{path:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("keep this") && saved.contains("3 passed"));
    assert!(!saved.contains("printf"), "no provider arguments are saved");

    // A new editor in the same directory: nothing is read until the
    // panel opens.
    let mut ed = f.session_editor(c.clone());
    assert!(ed.agents.agents.is_empty());
    open_and_load(&mut ed, 2);
    let msg = status_text(&ed);
    assert!(
        msg.contains("2 agent record(s) from an earlier session, not running"),
        "{msg}"
    );
    for a in &ed.agents.agents {
        assert_eq!(a.origin, maestro_editor::agents::Origin::Loaded);
        assert!(!a.is_running());
        assert!(a.from_earlier_session());
        assert_eq!(a.status(), Status::ReadyForReview);
        assert!(
            a.history
                .iter()
                .any(|h| h.text.contains("loaded from an earlier session"))
        );
    }
    let alpha = agent(&ed, 0);
    assert_eq!(alpha.name, "alpha-1");
    assert_eq!(alpha.worktree.as_ref().unwrap().path, wt_a);
    assert_eq!(alpha.comments.len(), 1);
    assert_eq!(alpha.comments[0].text, "keep this");
    assert!(
        alpha
            .latest_test()
            .unwrap()
            .describe()
            .contains("passed: 3 passed"),
        "{}",
        alpha.latest_test().unwrap().describe()
    );
    assert!(hint_keys(&ed).contains(&("R", "relaunch")));
    // Feedback cannot reach a program that is not running.
    run(&mut ed, "f");
    assert!(ed.agents.panel.overlay.is_none());
    let msg = status_text(&ed);
    assert!(
        msg.contains("from an earlier session") && msg.contains("R relaunches it"),
        "{msg}"
    );
    // Tests run on a loaded agent.
    run(&mut ed, "t");
    poll_until(&mut ed, "the second test run", |ed| {
        agent(ed, 0).tests.len() == 2 && agent(ed, 0).tests[1].finished.is_some()
    });

    // Review and integrate alpha, comment and all.
    run(&mut ed, "\n");
    poll_until(&mut ed, "alpha's review with its comment", |ed| {
        maestro_editor::agents::review::current_rows(ed)
            .iter()
            .any(|r| matches!(r, Row::Comment { text, .. } if text == "keep this"))
    });
    run(&mut ed, "<esc>I");
    poll_until(&mut ed, "the integrate dialog", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Integrate { .. }))
    });
    run(&mut ed, "y");
    poll_until(&mut ed, "the integration", |ed| {
        status(ed, 0) == Status::Integrated
    });
    assert_eq!(
        std::fs::read_to_string(f.repo.join("notes.txt")).unwrap(),
        "alpha\n"
    );

    // Archive beta: its change is uncommitted, so forcing asks again.
    select(&mut ed, 1);
    run(&mut ed, "A");
    assert!(matches!(&ed.agents.panel.overlay,
        Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Archive(_))));
    run(&mut ed, "y");
    poll_until(&mut ed, "the force confirmation", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::ForceArchive(_)))
    });
    assert!(wt_b.exists());
    run(&mut ed, "F");
    poll_until(&mut ed, "the archive", |ed| agent(ed, 1).archived);
    let list = f.git(&f.repo, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(wt_b.to_str().unwrap()), "{list}");
    assert!(list.contains(wt_a.to_str().unwrap()), "{list}");

    // A new agent continues after the loaded ids: no branch is reused.
    start_task(&mut ed, "third", 0);
    poll_until(&mut ed, "the third agent", |ed| {
        ed.agents.agents.len() == 3 && agent(ed, 2).exited.is_some()
    });
    assert_eq!(id(&ed, 2), AgentId(3));
    assert_eq!(agent(&ed, 2).name, "alpha-3");
    assert!(
        agent(&ed, 2)
            .worktree
            .as_ref()
            .unwrap()
            .branch
            .ends_with("third-3")
    );
    quit(&mut ed);
    drop(ed);

    // A third start shows the decisions.
    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 3);
    assert_eq!(status(&ed, 0), Status::Integrated);
    assert!(agent(&ed, 1).archived);
    assert_eq!(agent(&ed, 2).name, "alpha-3");
    assert_eq!(agent(&ed, 0).tests.len(), 2);
}

/// Worktrees on maestro/ branches with no record are offered; they can be
/// reviewed, archived (with the usual confirmation) or adopted, and
/// nothing else.
fn found_worktrees_are_offered_archived_and_adopted() {
    let f = Fixture::new();
    let wts = f.tmp.join("repo-worktrees");
    std::fs::create_dir_all(&wts).unwrap();
    let orphan = wts.join("orphan-7");
    let kept = wts.join("kept-8");
    let add = |path: &Path, branch: &str| {
        f.git(
            &f.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
    };
    add(&orphan, "maestro/orphan-7");
    std::fs::write(orphan.join("left.txt"), "left behind\n").unwrap();
    f.git(&orphan, &["add", "-A"]);
    f.git(&orphan, &["commit", "-q", "-m", "orphaned work"]);
    add(&kept, "maestro/kept-8");
    // Not offered: another branch, and a maestro/ branch elsewhere.
    add(&f.tmp.join("feature"), "feature");
    add(&f.tmp.join("elsewhere"), "maestro/elsewhere-1");

    let mut ed = f.session_editor(config(&[(
        "sh",
        provider(AgentProviderKind::Terminal, "sh", &["-c", "true"]),
    )]));
    open_and_load(&mut ed, 2);
    let msg = status_text(&ed);
    assert!(msg.contains("2 found worktree(s) with no record"), "{msg}");
    let o = index_of(&ed, "found-orphan-7");
    let k = index_of(&ed, "found-kept-8");
    assert_eq!(agent(&ed, o).origin, maestro_editor::agents::Origin::Found);
    assert!(!agent(&ed, o).is_running());
    select(&mut ed, o);
    let hints = hint_keys(&ed);
    assert!(hints.contains(&("a", "adopt")) && hints.contains(&("A", "archive")));
    assert!(!hints.iter().any(|(k, _)| *k == "I" || *k == "R"));
    for key in ["t", "I", "f", "X", "N"] {
        run(&mut ed, key);
        let msg = status_text(&ed);
        assert!(
            msg.contains("found worktree with no record"),
            "{key}: {msg}"
        );
        assert!(ed.agents.panel.overlay.is_none());
    }
    // The review shows its committed work against the fork point.
    run(&mut ed, "\n");
    poll_until(&mut ed, "the found worktree's review", |ed| {
        maestro_editor::agents::review::current_rows(ed)
            .iter()
            .any(|r| matches!(r, Row::Line(l) if l.text == "left behind"))
    });
    run(&mut ed, "c");
    assert!(
        ed.agents.panel.overlay.is_none(),
        "no comments on a found worktree"
    );
    // Archive it: the dialog says it has no record; the branch is kept.
    run(&mut ed, "<esc>A");
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => {
            assert!(matches!(c.action, ConfirmAction::Archive(_)));
            assert!(
                c.lines.iter().any(|l| l.contains("has no record")),
                "{:?}",
                c.lines
            );
        }
        other => panic!("expected the archive dialog, got {other:?}"),
    }
    assert!(orphan.exists(), "nothing removed before y");
    run(&mut ed, "y");
    // Its commits are on no other branch, so removal is refused and the
    // second dialog asks, saying the branch keeps them.
    poll_until(&mut ed, "the force confirmation", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::ForceArchive(_)))
    });
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => assert!(
            c.lines
                .iter()
                .any(|l| l.contains("only on branch maestro/orphan-7, which is kept")),
            "{:?}",
            c.lines
        ),
        _ => unreachable!(),
    }
    assert!(orphan.exists(), "nothing removed before F");
    run(&mut ed, "F");
    poll_until(&mut ed, "the archive", |ed| agent(ed, o).archived);
    assert!(!orphan.exists());
    let tip = f.git(&f.repo, &["log", "-1", "--format=%s", "maestro/orphan-7"]);
    assert_eq!(tip.trim(), "orphaned work", "the branch is kept");

    // Adopt the other: it becomes a saved record with a provider.
    select(&mut ed, k);
    run(&mut ed, "a");
    assert!(matches!(
        &ed.agents.panel.overlay,
        Some(Overlay::Choice { .. })
    ));
    run(&mut ed, "\n");
    let a = agent(&ed, k);
    assert_eq!(a.origin, maestro_editor::agents::Origin::Loaded);
    assert_eq!(a.provider.name, "sh");
    assert_eq!(a.status(), Status::Stopped);
    assert!(a.name.starts_with("sh-"), "{}", a.name);
    assert!(hint_keys(&ed).contains(&("R", "relaunch")));
    quit(&mut ed);
    drop(ed);
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(f.records()).unwrap()).unwrap();
    let agents = saved["agents"].as_array().unwrap();
    assert_eq!(
        agents.len(),
        1,
        "only the adopted worktree is saved: {saved}"
    );
    assert_eq!(agents[0]["worktree"]["branch"], "maestro/kept-8");
    assert_eq!(agents[0]["status"], "stopped");
}

/// A records file that cannot be read is reported, left as it is, and
/// never overwritten in that session.
fn an_unusable_records_file_is_reported_and_left_alone() {
    let f = Fixture::new();
    let path = f.records();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ this is not json").unwrap();
    let mut ed = f.session_editor(config(&[(
        "sh",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf x > x.txt", "sh", "{task}"],
        ),
    )]));
    open_and_load(&mut ed, 0);
    let msg = status_text(&ed);
    assert!(
        msg.contains("agent records not loaded")
            && msg.contains("not valid JSON")
            && msg.contains("left as it is"),
        "{msg}"
    );
    start_task(&mut ed, "write x", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    ed.tick(Instant::now());
    quit(&mut ed);
    drop(ed);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ this is not json"
    );
    let dir: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(dir.len(), 1, "no temp file or other file: {dir:?}");
}

/// `restore-session = false` and `--clean` (no state directory) read and
/// write no agent records.
fn without_sessions_no_records_are_read_or_written() {
    let f = Fixture::new();
    let c = config(&[(
        "sh",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf x > x.txt", "sh", "{task}"],
        ),
    )]);
    // One record saved with sessions on.
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "first", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    quit(&mut ed);
    drop(ed);
    let saved = std::fs::read_to_string(f.records()).unwrap();

    // restore-session = false: the state directory is set, sessions off.
    let mut ed = f.editor(c.clone());
    maestro_editor::navigation::configure(&mut ed, Some(f.state()), false);
    run(&mut ed, " a");
    assert!(!ed.agents.persist.loading, "no scan is started");
    for _ in 0..20 {
        ed.poll_background();
    }
    assert!(ed.agents.agents.is_empty(), "nothing is loaded");
    start_task(&mut ed, "second", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    assert_eq!(id(&ed, 0), AgentId(1));
    ed.tick(Instant::now());
    quit(&mut ed);
    drop(ed);
    assert_eq!(std::fs::read_to_string(f.records()).unwrap(), saved);

    // --clean: no state directory at all.
    let mut ed = f.editor(c);
    run(&mut ed, " a");
    assert!(ed.agents.agents.is_empty());
    start_task(&mut ed, "third", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    ed.tick(Instant::now());
    quit(&mut ed);
    drop(ed);
    assert_eq!(std::fs::read_to_string(f.records()).unwrap(), saved);
    let files = std::fs::read_dir(f.records().parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(files, 1);
}

/// `R` on a loaded terminal agent runs its provider command again in the
/// same worktree, with the `{task}` argument left out.
fn a_loaded_terminal_agent_relaunches_without_its_task() {
    let f = Fixture::new();
    let script = "printf '%s\\n' \"${1:-no task}\" >> runs.txt";
    let c = config(&[(
        "sh",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", script, "sh", "{task}"],
        ),
    )]);
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "first run", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    let wt = worktree(&ed, 0);
    quit(&mut ed);
    drop(ed);

    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 1);
    assert!(agent(&ed, 0).from_earlier_session());
    run(&mut ed, "R");
    let msg = status_text(&ed);
    assert!(msg.contains("without its task"), "{msg}");
    poll_until(&mut ed, "the relaunched program", |ed| {
        agent(ed, 0).exited.is_some()
    });
    let a = agent(&ed, 0);
    assert_eq!(a.origin, maestro_editor::agents::Origin::New);
    assert!(!a.from_earlier_session());
    assert_eq!(a.status(), Status::ReadyForReview);
    assert!(
        a.history.iter().any(|h| h
            .text
            .contains("relaunched sh in a terminal without the task")),
        "{:?}",
        a.history
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("runs.txt")).unwrap(),
        "first run\nno task\n"
    );
}

/// `R` on a loaded ACP agent loads its saved session when the agent
/// advertises `loadSession`, and starts a new one (saying so) when not.
fn a_loaded_acp_agent_relaunches_into_its_saved_session() {
    use maestro_agent::fake::LOAD_SESSION_ARG;
    let f = Fixture::new();
    let exe = std::env::current_exe().unwrap();
    let exe = exe.to_str().unwrap();
    let c = config(&[
        (
            "loader",
            provider(AgentProviderKind::Acp, exe, &[FAKE_ARG, LOAD_SESSION_ARG]),
        ),
        ("plain", provider(AgentProviderKind::Acp, exe, &[FAKE_ARG])),
    ]);
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "say first answer", 0);
    start_task(&mut ed, "say plain answer", 1);
    poll_until(&mut ed, "both turns", |ed| {
        ed.agents.agents.len() == 2
            && ed
                .agents
                .agents
                .iter()
                .all(|a| matches!(a.status(), Status::ReadyForReview | Status::Completed))
    });
    assert_eq!(
        agent(&ed, 0).session_id.as_deref(),
        Some(maestro_agent::fake::SESSION_ID)
    );
    assert!(agent(&ed, 0).load_session);
    assert!(!agent(&ed, 1).load_session);
    quit(&mut ed);
    drop(ed);

    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 2);
    assert!(
        agent(&ed, 0).transcript.is_empty(),
        "transcripts are not saved"
    );
    run(&mut ed, "f");
    let msg = status_text(&ed);
    assert!(
        msg.contains("R relaunches it; then f sends the feedback"),
        "{msg}"
    );
    run(&mut ed, "R");
    assert!(
        status_text(&ed).contains("loading its saved session"),
        "{}",
        status_text(&ed)
    );
    poll_until(&mut ed, "the loaded session", |ed| {
        status(ed, 0) == Status::Idle
    });
    let a = agent(&ed, 0);
    assert!(a.is_running());
    assert!(
        a.transcript
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Agent(t) if t == "replayed fake-session")),
        "{:?}",
        a.transcript
    );
    assert!(
        a.history
            .iter()
            .any(|h| h.text.contains("loaded the saved session fake-session"))
    );
    // A replayed tool call keeps its later update in the transcript, and
    // neither goes into the history or the status.
    assert!(
        a.transcript.iter().any(|e| matches!(e,
            TranscriptEntry::Tool { id, status, .. } if id == "old-tool" && status == "completed")),
        "{:?}",
        a.transcript
    );
    assert!(!a.history.iter().any(|h| h.text.contains("old-tool")));
    run(&mut ed, "f");
    ed.paste_text("say after the relaunch");
    run(&mut ed, "\n");
    poll_until(&mut ed, "the feedback turn", |ed| {
        matches!(status(ed, 0), Status::ReadyForReview | Status::Completed)
            && agent(ed, 0)
                .transcript
                .iter()
                .any(|e| matches!(e, TranscriptEntry::Agent(t) if t.contains("after the relaunch")))
    });

    // An agent that does not advertise loadSession gets a new session.
    select(&mut ed, 1);
    run(&mut ed, "R");
    assert!(
        status_text(&ed).contains("in a new session without the earlier conversation"),
        "{}",
        status_text(&ed)
    );
    poll_until(&mut ed, "the new session", |ed| {
        status(ed, 1) == Status::Idle
    });
    assert!(
        agent(&ed, 1).transcript.iter().any(|e| matches!(e,
            TranscriptEntry::System(t) if t.contains("did not advertise session loading"))),
        "{:?}",
        agent(&ed, 1).transcript
    );
    quit(&mut ed);
}

/// A loaded record whose worktree was switched to another branch is not
/// reviewed, tested, relaunched or archived as if it were the agent's.
fn a_loaded_record_whose_worktree_changed_is_not_archived() {
    let f = Fixture::new();
    let c = config(&[(
        "sh",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf x > x.txt", "sh", "{task}"],
        ),
    )]);
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "write x", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    let wt = worktree(&ed, 0);
    quit(&mut ed);
    drop(ed);
    // Someone else's work now lives in that directory.
    f.git(&wt, &["checkout", "-q", "-b", "other"]);

    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 1);
    let msg = status_text(&ed);
    assert!(msg.contains("1 record(s) whose worktree changed"), "{msg}");
    let note = agent(&ed, 0).worktree_note.clone().unwrap();
    assert!(note.contains("now on branch other"), "{note}");
    poll_until(&mut ed, "the refused review", |ed| {
        agent(ed, 0)
            .review
            .error
            .as_deref()
            .is_some_and(|e| e.contains("on other now"))
    });
    for key in ["A", "R", "t"] {
        run(&mut ed, key);
        assert!(ed.agents.panel.overlay.is_none(), "{key}");
        let msg = status_text(&ed);
        assert!(msg.contains("now on branch other"), "{key}: {msg}");
    }
    assert!(wt.join("x.txt").exists(), "nothing was removed");
}

fn x_writer() -> Config {
    config(&[(
        "sh",
        provider(
            AgentProviderKind::Terminal,
            "sh",
            &["-c", "printf x > x.txt", "sh", "{task}"],
        ),
    )])
}

/// The worktree is checked again when it is used, not only at load: a
/// switch to other work after loading stops tests, relaunch and drop file.
fn a_worktree_switched_after_loading_is_not_touched() {
    let f = Fixture::new();
    let mut ed = f.session_editor(x_writer());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "write x", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    let wt = worktree(&ed, 0);
    quit(&mut ed);
    drop(ed);

    let mut ed = f.session_editor(x_writer());
    open_and_load(&mut ed, 1);
    assert!(agent(&ed, 0).worktree_note.is_none());
    run(&mut ed, "\n");
    poll_until(&mut ed, "the review", |ed| review_paths(ed, 0) == ["x.txt"]);
    run(&mut ed, "<esc>");
    // Now someone puts other work in that directory, with an x.txt of its
    // own that the old review still lists.
    f.git(&wt, &["checkout", "-q", "-b", "other"]);
    f.git(&wt, &["add", "-A"]);
    f.git(&wt, &["commit", "-q", "-m", "not the agent's"]);
    for key in ["t", "R"] {
        run(&mut ed, key);
        let msg = status_text(&ed);
        assert!(msg.contains("on branch other now"), "{key}: {msg}");
        assert!(!agent(&ed, 0).is_running() && agent(&ed, 0).tests.is_empty());
    }
    // The review still lists the file; dropping it is refused by the
    // check on the background thread.
    run(&mut ed, "\nDy");
    poll_until(&mut ed, "the refused drop", |ed| {
        status_text(ed).contains("could not drop the change")
    });
    let msg = status_text(&ed);
    assert!(msg.contains("is on other, not maestro/write-x-1"), "{msg}");
    assert_eq!(std::fs::read_to_string(wt.join("x.txt")).unwrap(), "x");
}

/// Another Maestro saving in between: its new records are kept, and a
/// file it left unusable is not overwritten.
fn records_written_by_another_maestro_are_kept() {
    let f = Fixture::new();
    let mut ed = f.session_editor(x_writer());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "write x", 0);
    poll_until(&mut ed, "the program", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    quit(&mut ed);
    drop(ed);

    let mut ed = f.session_editor(x_writer());
    open_and_load(&mut ed, 1);
    start_task(&mut ed, "write y", 0);
    poll_until(&mut ed, "the second agent", |ed| {
        ed.agents.agents.len() == 2 && agent(ed, 1).exited.is_some()
    });
    assert_eq!(id(&ed, 1), AgentId(2));
    // Meanwhile another Maestro saved an agent 2 of its own.
    let path = f.records();
    let mut file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut other = file["agents"][0].clone();
    other["id"] = 2.into();
    other["name"] = "sh-2".into();
    other["task"] = "the other editor's task".into();
    let elsewhere = f.tmp.join("repo-worktrees/elsewhere-2");
    other["worktree"]["path"] = elsewhere.to_str().unwrap().into();
    other["worktree"]["branch"] = "maestro/elsewhere-2".into();
    file["agents"].as_array_mut().unwrap().push(other);
    std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
    ed.tick(Instant::now());
    let msg = status_text(&ed);
    assert!(
        msg.contains("1 agent record(s) saved by another Maestro"),
        "{msg}"
    );
    let theirs = ed
        .agents
        .agents
        .iter()
        .find(|a| a.task == "the other editor's task")
        .unwrap();
    assert_eq!(theirs.id, AgentId(3), "renumbered: 2 was taken here");
    assert_eq!(theirs.name, "sh-3");
    assert!(theirs.from_earlier_session());
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let tasks: Vec<&str> = saved["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["task"].as_str().unwrap())
        .collect();
    assert_eq!(tasks, ["write x", "write y", "the other editor's task"]);

    // Then it leaves the file unusable: this editor stops saving and does
    // not replace it.
    std::fs::write(&path, "{\"version\": 9}").unwrap();
    run(&mut ed, "\nc");
    ed.paste_text("a new comment");
    run(&mut ed, "\n<esc>");
    ed.tick(Instant::now());
    let msg = status_text(&ed);
    assert!(msg.contains("can no longer be used"), "{msg}");
    quit(&mut ed);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"version\": 9}");
}

// ----- task contracts and conflicts ------------------------------------------

/// A terminal provider running `script` in `sh`, with the task as `$1`.
fn sh_provider(script: &str) -> AgentProviderConfig {
    provider(
        AgentProviderKind::Terminal,
        "sh",
        &["-c", script, "sh", "{task}"],
    )
}

/// The text of the open confirmation dialog, or a panic.
fn dialog(ed: &Editor) -> (String, Vec<(String, String)>) {
    match &ed.agents.panel.overlay {
        Some(Overlay::Confirm(c)) => (c.lines.join("\n"), c.keys.clone()),
        other => panic!("expected a dialog, got {other:?}; status {:?}", ed.status),
    }
}

/// `n`, the task with its contract lines, the contract dialog, `y`, then
/// the provider at `provider_index`.
fn start_contract_task(ed: &mut Editor, task: &str, provider_index: usize) -> String {
    run(ed, "n");
    ed.paste_text(task);
    run(ed, "\n");
    let (text, keys) = dialog(ed);
    assert!(
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::StartTask { .. })),
        "{text}"
    );
    assert_eq!(keys[0].0, "y");
    run(ed, "y");
    match &ed.agents.panel.overlay {
        Some(Overlay::Choice { title, .. }) => {
            assert_eq!(title, "provider (task with a contract)")
        }
        other => panic!("expected the provider list, got {other:?}"),
    }
    for _ in 0..provider_index {
        run(ed, "j");
    }
    run(ed, "\n");
    text
}

/// Waits for agent `i`'s program to exit and its diff to show `files`.
fn wait_files(ed: &mut Editor, i: usize, files: &[&str]) {
    let want: Vec<String> = files.iter().map(|s| s.to_string()).collect();
    poll_until(ed, "the program's exit and its diff", |ed| {
        ed.agents.agents.len() > i
            && agent(ed, i).exited.is_some()
            && !agent(ed, i).review.running
            && {
                let mut got = review_paths(ed, i);
                got.sort();
                got == want
            }
    });
}

/// Opens the integrate dialog for the selected agent and waits for it.
fn open_integrate(ed: &mut Editor) {
    run(ed, "I");
    poll_until(ed, "the integrate dialog", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Integrate { .. }))
    });
}

/// Runs `t` on the selected agent and waits until the run finished and
/// the files it ran on were recorded.
fn verify(ed: &mut Editor, i: usize) {
    let before = agent(ed, i).tests.len();
    run(ed, "t");
    poll_until(ed, "the verification and its snapshot", |ed| {
        agent(ed, i).tests.len() == before + 1
            && agent(ed, i)
                .latest_test()
                .is_some_and(|t| t.finished.is_some() && t.tree.is_some())
    });
}

fn contracts_typed_and_from_a_file_are_shown_and_sent() {
    use maestro_editor::agents::contract::Contract;
    let f = Fixture::new();
    // The script saves what it was given, so the test sees what was sent.
    let mut ed = f.editor(config(&[(
        "echoer",
        sh_provider("printf '%s' \"$1\" > task.txt"),
    )]));
    run(&mut ed, " a");

    // A mistake keeps the text in the box and says what is wrong.
    run(&mut ed, "n");
    ed.paste_text("x\n@allow ../up");
    run(&mut ed, "\n");
    assert!(
        status_text(&ed).contains("task contract"),
        "{:?}",
        ed.status
    );
    match &ed.agents.panel.overlay {
        Some(Overlay::Input { text, .. }) => assert_eq!(text, "x\n@allow ../up"),
        other => panic!("the input stays open, got {other:?}"),
    }
    run(&mut ed, "<esc>");
    assert!(ed.agents.agents.is_empty());

    // Typed: the dialog shows all of it before anything starts.
    let shown = start_contract_task(
        &mut ed,
        "write the notes\n@allow task.txt notes.txt\n@forbid secret/**\n\
         @verify sh -c 'test -f notes.txt'\n@accept the notes exist",
        0,
    );
    assert!(shown.contains("Contract typed with the task."), "{shown}");
    assert!(
        shown.contains("Allowed paths: task.txt, notes.txt"),
        "{shown}"
    );
    assert!(shown.contains("Forbidden paths: secret/**"), "{shown}");
    assert!(shown.contains("sh -c 'test -f notes.txt'"), "{shown}");
    assert!(shown.contains("Acceptance: the notes exist"), "{shown}");
    assert!(
        shown.contains("the agent's own tools are not stopped"),
        "{shown}"
    );
    wait_files(&mut ed, 0, &["task.txt"]);
    let a = agent(&ed, 0);
    assert_eq!(a.task, "write the notes");
    let want = Contract {
        allowed: vec!["task.txt".into(), "notes.txt".into()],
        forbidden: vec!["secret/**".into()],
        verify: Some(vec!["sh".into(), "-c".into(), "test -f notes.txt".into()]),
        acceptance: vec!["the notes exist".into()],
        source: None,
    };
    assert_eq!(a.contract.as_ref(), Some(&want));
    let sent = std::fs::read_to_string(worktree(&ed, 0).join("task.txt")).unwrap();
    assert!(
        sent.starts_with("write the notes\n\nTask contract from the user"),
        "{sent}"
    );
    assert!(
        sent.contains("- Change only paths matching: task.txt, notes.txt"),
        "{sent}"
    );
    assert!(
        sent.contains("- Do not change paths matching: secret/**"),
        "{sent}"
    );
    assert!(sent.contains("  - the notes exist"), "{sent}");
    assert!(!sent.contains("@allow"), "{sent}");

    // From a file the user names, read only then.
    std::fs::create_dir_all(f.repo.join("contracts")).unwrap();
    std::fs::write(
        f.repo.join("contracts/notes.toml"),
        "allowed = [\"task.txt\"]\nverify = \"true\"\nacceptance = \"from the file\"\n",
    )
    .unwrap();
    let shown = start_contract_task(&mut ed, "write it again\n@contract contracts/notes.toml", 0);
    assert!(
        shown.contains("Contract from contracts/notes.toml."),
        "{shown}"
    );
    wait_files(&mut ed, 1, &["task.txt"]);
    let c = agent(&ed, 1).contract.clone().unwrap();
    assert_eq!(c.allowed, ["task.txt"]);
    assert_eq!(c.verify.as_deref().unwrap(), ["true"]);
    assert_eq!(c.acceptance, ["from the file"]);
    assert_eq!(c.source.as_deref(), Some("contracts/notes.toml"));
    let sent = std::fs::read_to_string(worktree(&ed, 1).join("task.txt")).unwrap();
    assert!(sent.contains("  - from the file"), "{sent}");

    // A file with a key the format does not have is refused whole.
    std::fs::write(
        f.repo.join("contracts/bad.toml"),
        "allowed = [\"x\"]\nrun = \"rm -rf .\"\n",
    )
    .unwrap();
    run(&mut ed, "n");
    ed.paste_text("bad\n@contract contracts/bad.toml");
    run(&mut ed, "\n");
    let msg = status_text(&ed);
    assert!(
        msg.contains("contracts/bad.toml") && msg.contains("run"),
        "{msg}"
    );
    run(&mut ed, "<esc>");
    // Combining a file and typed lines is refused too.
    run(&mut ed, "n");
    ed.paste_text("both\n@contract contracts/notes.toml\n@allow x");
    run(&mut ed, "\n");
    assert!(status_text(&ed).contains("not both"), "{:?}", ed.status);
    run(&mut ed, "<esc>");
    assert_eq!(ed.agents.agents.len(), 2);

    // A task without contract lines goes straight to the providers.
    run(&mut ed, "n");
    ed.paste_text("no contract here");
    run(&mut ed, "\n");
    match &ed.agents.panel.overlay {
        Some(Overlay::Choice { title, .. }) => assert_eq!(title, "provider (no task contract)"),
        other => panic!("{other:?}"),
    }
}

fn acp_writes_outside_the_contract_are_refused() {
    let f = Fixture::new();
    let exe = std::env::current_exe().unwrap();
    let mut ed = f.editor(config(&[(
        "fake",
        provider(AgentProviderKind::Acp, exe.to_str().unwrap(), &[FAKE_ARG]),
    )]));
    run(&mut ed, " a");
    // The task is the fake agent's script; the @ lines are the contract.
    start_contract_task(
        &mut ed,
        "write docs/notes.md allowed\\n\n\
         write src/lib.rs pub fn two() {}\\n\n\
         write docs/../README.md sneaky\\n\n\
         say done\n\
         @allow docs/**",
        0,
    );
    poll_until(&mut ed, "the turn's end", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.base_status() == Status::ReadyForReview && !a.review.running)
    });
    let wt = worktree(&ed, 0);
    assert_eq!(
        std::fs::read_to_string(wt.join("docs/notes.md")).unwrap(),
        "allowed\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.rs")).unwrap(),
        "pub fn one() {}\n",
        "refused: not under docs/"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "# demo\n",
        "refused: the resolved path is what counts"
    );
    let a = agent(&ed, 0);
    let refused: Vec<&String> = a
        .errors
        .iter()
        .filter(|e| e.contains("outside the task contract"))
        .collect();
    assert_eq!(refused.len(), 2, "{:?}", a.errors);
    assert!(
        refused[0].contains("src/lib.rs matches no allowed pattern"),
        "{refused:?}"
    );
    // The agent was told: its transcript has the refusals.
    let said: Vec<String> = a
        .transcript
        .iter()
        .filter_map(|e| match e {
            TranscriptEntry::Agent(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let said = said.join("\n");
    assert!(said.contains("write failed"), "{said}");
    // The contract went with the first prompt.
    assert!(
        a.transcript.iter().any(
            |e| matches!(e, TranscriptEntry::User(t) if t.contains("Change only paths matching: docs/**"))
        ),
        "{:?}",
        a.transcript
    );
    assert_eq!(review_paths(&ed, 0), ["docs/notes.md"]);
}

fn out_of_scope_files_need_their_own_key_and_verification_is_checked() {
    let f = Fixture::new();
    let flag = f.tmp.join("verification-passes");
    let mut ed = f.editor(config(&[(
        "writer",
        sh_provider("printf 'notes\\n' > notes.txt; printf 'stray\\n' > stray.txt"),
    )]));
    run(&mut ed, " a");
    start_contract_task(
        &mut ed,
        &format!(
            "write the notes\n@allow notes.txt\n@verify sh -c 'test -f {} && echo verified'",
            flag.display()
        ),
        0,
    );
    wait_files(&mut ed, 0, &["notes.txt", "stray.txt"]);
    let outside = agent(&ed, 0).out_of_scope();
    assert_eq!(outside.len(), 1, "{outside:?}");
    assert_eq!(outside[0].0, PathBuf::from("stray.txt"));

    // The review marks it on the file itself.
    run(&mut ed, "\n");
    let at = review_paths(&ed, 0)
        .iter()
        .position(|p| p == "stray.txt")
        .unwrap();
    for _ in 0..at {
        run(&mut ed, "<tab>");
    }
    let rows = maestro_editor::agents::review::current_rows(&ed);
    assert!(
        matches!(&rows[0], Row::Note(n)
            if n.contains("outside the task contract: stray.txt matches no allowed pattern")),
        "{rows:?}"
    );
    run(&mut ed, "<tab>");
    let rows = maestro_editor::agents::review::current_rows(&ed);
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, Row::Note(n) if n.contains("outside"))),
        "notes.txt is allowed: {rows:?}"
    );
    run(&mut ed, "<esc>");

    // Before any verification, integrate says so.
    open_integrate(&mut ed);
    let (text, keys) = dialog(&ed);
    assert!(
        text.contains("Warning: No verification has run on this work"),
        "{text}"
    );
    run(&mut ed, "n");

    // `t` runs the contract's command, not [agents] test-command (there is
    // none, and no Cargo.toml). It fails first: the flag is missing.
    verify(&mut ed, 0);
    let t = agent(&ed, 0).latest_test().unwrap();
    assert!(t.command.contains("test -f"), "{}", t.command);
    assert_eq!(t.passed(), Some(false));
    assert!(
        agent(&ed, 0)
            .history
            .iter()
            .any(|h| h.text.contains("tests started (from the task contract)")),
        "{:?}",
        agent(&ed, 0).history
    );
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(
        text.contains("Warning: The last verification `sh -c test -f"),
        "{text}"
    );
    assert!(text.contains("did not pass: failed (exit 1)"), "{text}");
    run(&mut ed, "n");

    std::fs::write(&flag, "").unwrap();
    verify(&mut ed, 0);
    assert_eq!(agent(&ed, 0).latest_test().unwrap().passed(), Some(true));
    assert!(
        agent(&ed, 0)
            .latest_test()
            .unwrap()
            .output
            .iter()
            .any(|l| l == "verified")
    );

    // The dialog lists the file outside the contract, and y is refused.
    open_integrate(&mut ed);
    let (text, keys2) = dialog(&ed);
    let _ = keys;
    assert!(
        text.contains("OUTSIDE THE TASK CONTRACT: 1 changed file(s) would be integrated: stray.txt matches no allowed pattern"),
        "{text}"
    );
    assert!(
        !text.contains("Warning:"),
        "verified on these files: {text}"
    );
    assert_eq!(keys2[0].0, "O", "{keys2:?}");
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    run(&mut ed, "y");
    assert!(
        status_text(&ed).contains("outside the task contract: O integrates them anyway"),
        "{:?}",
        ed.status
    );
    assert!(ed.agents.panel.overlay.is_some(), "the dialog stays");
    run(&mut ed, "n");
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);

    // A change after the verification makes it stale.
    std::fs::write(worktree(&ed, 0).join("notes.txt"), "notes, edited\n").unwrap();
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(
        text.contains("Warning: The files changed after the last verification")
            && text.contains("ran: notes.txt."),
        "{text}"
    );

    // O integrates anyway, and the commit says what was let in.
    run(&mut ed, "O");
    poll_until(&mut ed, "the integration", |ed| {
        status(ed, 0) == Status::Integrated
    });
    assert_eq!(
        std::fs::read_to_string(f.repo.join("stray.txt")).unwrap(),
        "stray\n"
    );
    let log = f.git(&f.repo, &["log", "--format=%B", "-n", "2", "main^2"]);
    assert!(
        log.contains(
            "Outside the task contract, integrated after a separate confirmation (O): stray.txt"
        ),
        "{log}"
    );
}

fn files_outside_the_contract_written_after_the_dialog_stop_the_integration() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "writer",
        sh_provider("printf 'notes\\n' > notes.txt"),
    )]));
    run(&mut ed, " a");
    start_contract_task(&mut ed, "write the notes\n@allow notes.txt", 0);
    wait_files(&mut ed, 0, &["notes.txt"]);
    open_integrate(&mut ed);
    let (text, keys) = dialog(&ed);
    assert!(!text.contains("OUTSIDE"), "{text}");
    assert_eq!(keys[0].0, "y");
    // Something writes outside the contract while the dialog is up.
    std::fs::write(worktree(&ed, 0).join("late.txt"), "late\n").unwrap();
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    run(&mut ed, "y");
    poll_until(&mut ed, "the refusal", |ed| {
        status_text(ed).contains("not integrated")
    });
    let msg = status_text(&ed);
    assert!(
        msg.contains(
            "the agent's files changed after the dialog was checked; nothing was integrated"
        ),
        "{msg}"
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    assert!(!f.repo.join("late.txt").exists());
    assert_ne!(status(&ed, 0), Status::Integrated);
    // Asked again, the dialog lists it.
    open_integrate(&mut ed);
    let (text, keys) = dialog(&ed);
    assert!(
        text.contains("OUTSIDE THE TASK CONTRACT: 1 changed file(s) would be integrated: late.txt"),
        "{text}"
    );
    assert_eq!(keys[0].0, "O");
    run(&mut ed, "n");
    // A change to an allowed file while the dialog is up stops it too, even
    // with O: the dialog was about the files as they were.
    open_integrate(&mut ed);
    std::fs::write(worktree(&ed, 0).join("notes.txt"), "edited later\n").unwrap();
    run(&mut ed, "O");
    poll_until(&mut ed, "the second refusal", |ed| {
        status_text(ed).contains("changed after the dialog")
    });
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
}

/// Work outside the contract that a commit on the branch made and a later
/// one undid is invisible in the net diff, but a cherry-pick applies each
/// commit: the dialog lists it. Every file is named, however many.
fn history_outside_the_contract_is_listed_in_full() {
    let f = Fixture::new();
    let git = "git -c user.name=A -c user.email=a@example.invalid -c commit.gpgSign=false";
    let script = format!(
        "printf 'x\\n' > secret.txt && {git} add -A && {git} commit -qm add && \
         {git} rm -q secret.txt && {git} commit -qm remove && \
         for i in 1 2 3 4 5 6 7 8 9 10; do printf 's\\n' > stray$i.txt; done && \
         printf 'notes\\n' > notes.txt"
    );
    let mut ed = f.editor(config(&[("writer", sh_provider(&script))]));
    run(&mut ed, " a");
    start_contract_task(&mut ed, "write the notes\n@allow notes.txt", 0);
    poll_until(&mut ed, "the program and its diff", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.exited.is_some() && !a.review.running && a.review.files.len() == 11)
    });
    assert_eq!(
        status(&ed, 0),
        Status::ReadyForReview,
        "{:?}",
        agent(&ed, 0).history
    );
    assert!(!review_paths(&ed, 0).contains(&"secret.txt".to_string()));
    ed.resize(Rect::new(0, 1, 160, 60));
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(
        text.contains("OUTSIDE THE TASK CONTRACT: 11 changed file(s)"),
        "{text}"
    );
    assert!(
        text.contains("secret.txt matches no allowed pattern (changed by a commit on the branch)"),
        "{text}"
    );
    for i in 1..=10 {
        assert!(text.contains(&format!("stray{i}.txt")), "{text}");
    }
    assert!(!text.lines().next().unwrap().contains("more"), "{text}");
}

fn overlapping_agents_show_conflicts_only_where_they_conflict() {
    use maestro_editor::agents::conflicts::{self, Merge};
    let f = Fixture::new();
    let lines = |first: &str, last: &str| {
        format!("printf '{first}\\n2\\n3\\n4\\n5\\n6\\n7\\n8\\n9\\n{last}\\n' > shared.txt")
    };
    std::fs::write(f.repo.join("shared.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
    f.git(&f.repo, &["add", "-A"]);
    f.git(&f.repo, &["commit", "-q", "-m", "shared"]);
    let mut ed = f.editor(config(&[
        // alpha and gamma both change line 1; beta changes line 10; zeta
        // touches another file.
        ("alpha", sh_provider(&lines("A", "10"))),
        ("beta", sh_provider(&lines("1", "B"))),
        ("gamma", sh_provider(&lines("G", "10"))),
        ("zeta", sh_provider("printf 'z\\n' > other.txt")),
    ]));
    run(&mut ed, " a");
    for i in 0..4 {
        start_task(&mut ed, "edit the shared file", i);
        wait_files(
            &mut ed,
            i,
            &[if i == 3 { "other.txt" } else { "shared.txt" }],
        );
    }
    poll_until(&mut ed, "the dry runs", |ed| {
        ed.agents.conflicts.pairs.len() == 3
            && ed
                .agents
                .conflicts
                .pairs
                .iter()
                .all(|p| p.merge != Merge::Checking)
    });
    let name = |ed: &Editor, id: AgentId| ed.agents.get(id).unwrap().name.clone();
    let mut seen: Vec<(String, String, Merge)> = ed
        .agents
        .conflicts
        .pairs
        .iter()
        .map(|p| {
            assert_eq!(p.files, [PathBuf::from("shared.txt")]);
            (name(&ed, p.a), name(&ed, p.b), p.merge.clone())
        })
        .collect();
    seen.sort_by(|x, y| (&x.0, &x.1).cmp(&(&y.0, &y.1)));
    assert_eq!(
        seen,
        [
            ("alpha-1".into(), "beta-2".into(), Merge::Clean),
            (
                "alpha-1".into(),
                "gamma-3".into(),
                Merge::Conflicts(vec!["shared.txt".into()])
            ),
            ("beta-2".into(), "gamma-3".into(), Merge::Clean),
        ]
    );
    let alpha = id(&ed, 0);
    let words = conflicts::describe(&ed, alpha).join("\n");
    assert!(
        words.contains("overlaps beta-2 in shared.txt: would merge cleanly"),
        "{words}"
    );
    assert!(
        words.contains("overlaps gamma-3 in shared.txt: CONFLICT in shared.txt"),
        "{words}"
    );
    assert_eq!(
        conflicts::brief(&ed, id(&ed, 1)).as_deref(),
        Some("overlaps alpha-1 gamma-3")
    );
    assert!(
        conflicts::describe(&ed, id(&ed, 3)).is_empty(),
        "zeta overlaps nothing"
    );

    // The cost bound: recomputing with nothing changed takes one snapshot
    // per overlapping agent (zeta, which overlaps nobody, gets none) and
    // reuses the dry runs cached by snapshot commit ids.
    let state = &ed.agents.conflicts;
    let (runs, snaps) = (state.dry_runs, state.snapshots);
    conflicts::refresh(&mut ed);
    poll_until(&mut ed, "the second pass", |ed| {
        ed.agents
            .conflicts
            .pairs
            .iter()
            .all(|p| p.merge != Merge::Checking)
            && ed.agents.conflicts.snapshots > snaps
    });
    assert_eq!(ed.agents.conflicts.dry_runs, runs, "cached by commit ids");
    assert_eq!(
        ed.agents.conflicts.snapshots,
        snaps + 3,
        "zeta is not snapshotted"
    );

    // The integrate dialog for alpha names gamma, and beta as clean.
    select(&mut ed, 0);
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(
        text.contains("After this, the work of gamma-3 in shared.txt would conflict with main."),
        "{text}"
    );
    assert!(
        text.contains("Overlapping agents whose work would still merge cleanly afterward: beta-2."),
        "{text}"
    );
    // Integrating alpha drops it from the pairs.
    run(&mut ed, "y");
    poll_until(&mut ed, "the integration and the new pairs", |ed| {
        status(ed, 0) == Status::Integrated
            && ed.agents.conflicts.pairs.len() == 1
            && ed.agents.conflicts.pairs[0].merge != Merge::Checking
    });
    assert_eq!(ed.agents.conflicts.pairs[0].merge, Merge::Clean);
    // gamma now conflicts with main itself: the dialog says so first, and
    // integrating is refused.
    select(&mut ed, 2);
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(
        text.starts_with("CONFLICT: this work conflicts with main in shared.txt"),
        "{text}"
    );
    assert!(
        text.contains("Could not check for conflicts afterward with: beta-2: not checked, this work itself conflicts with main."),
        "{text}"
    );
    // No verification command anywhere: nothing to warn about.
    assert!(!text.contains("Warning:"), "{text}");
    run(&mut ed, "y");
    poll_until(&mut ed, "the refusal", |ed| {
        status_text(ed).contains("not integrated")
    });
    assert!(
        status_text(&ed).contains("conflict in shared.txt"),
        "{:?}",
        ed.status
    );
}

fn contracts_and_verification_survive_a_restart() {
    let f = Fixture::new();
    let c = config(&[("writer", sh_provider("printf 'notes\\n' > notes.txt"))]);
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_contract_task(
        &mut ed,
        "write the notes\n@allow notes.txt\n@forbid secret\n@verify sh -c 'echo checked'\n@accept it is there",
        0,
    );
    wait_files(&mut ed, 0, &["notes.txt"]);
    verify(&mut ed, 0);
    let contract = agent(&ed, 0).contract.clone().unwrap();
    let tree = agent(&ed, 0).latest_test().unwrap().tree.clone();
    quit(&mut ed);
    drop(ed);
    let saved = std::fs::read_to_string(f.records()).unwrap();
    assert!(saved.contains("\"contract\""), "{saved}");

    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 1);
    let a = agent(&ed, 0);
    assert_eq!(a.contract.as_ref(), Some(&contract));
    assert_eq!(a.latest_test().unwrap().tree, tree);
    // The loaded agent's `t` runs the contract's command, and integrate
    // still knows the verification ran on these files.
    verify(&mut ed, 0);
    let t = agent(&ed, 0).latest_test().unwrap();
    assert_eq!(t.command, "sh -c echo checked");
    assert!(t.output.iter().any(|l| l == "checked"));
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(!text.contains("Warning:"), "{text}");
    assert!(!text.contains("OUTSIDE"), "{text}");
}

/// Whether process `pid` is running (a zombie counts as gone).
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
    }
}

/// `t` on a verification that starts a background process and exits: the
/// run finishes, and the process it left behind is gone.
fn a_verification_that_leaves_a_process_behind_ends_it() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "writer",
        sh_provider("printf 'notes\\n' > notes.txt"),
    )]));
    run(&mut ed, " a");
    start_contract_task(
        &mut ed,
        "write the notes\n@allow notes.txt\n@verify sh -c 'sleep 1000 & echo leftover $!'",
        0,
    );
    wait_files(&mut ed, 0, &["notes.txt"]);
    verify(&mut ed, 0);
    let t = agent(&ed, 0).latest_test().unwrap();
    assert_eq!(t.passed(), Some(true), "{:?}", t.output);
    let pid: u32 = t
        .output
        .iter()
        .find_map(|l| l.strip_prefix("leftover "))
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("no pid in {:?}", t.output));
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = process_alive(pid);
    if survived {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(
        !survived,
        "the verification's background sleep {pid} outlived the run"
    );
}

/// Confirms the open integrate dialog with `keys` and waits for the
/// refusal, which it returns.
fn confirm_refused(ed: &mut Editor, keys: &str) -> String {
    run(ed, keys);
    poll_until(ed, "the refusal", |ed| {
        status_text(ed).contains("not integrated")
    });
    status_text(ed)
}

/// The agent's branch moves while the integrate dialog is up, without
/// changing the files the dialog was about: a commit of the same files,
/// then history that adds and removes a file. Neither reaches main, for
/// merge or cherry-pick; integrating again after a fresh check does.
fn a_branch_that_moved_after_the_dialog_is_not_integrated() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "writer",
        sh_provider("printf 'notes\\n' > notes.txt"),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "write the notes", 0);
    wait_files(&mut ed, 0, &["notes.txt"]);
    let wt = worktree(&ed, 0);
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);

    // The agent commits the very files the dialog showed.
    open_integrate(&mut ed);
    f.git(&wt, &["add", "-A"]);
    f.git(&wt, &["commit", "-q", "-m", "the agent's own commit"]);
    let msg = confirm_refused(&mut ed, "y");
    assert!(
        msg.contains("the agent's branch moved after the dialog was checked"),
        "{msg}"
    );
    assert!(msg.contains("nothing was integrated"), "{msg}");
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    assert!(!f.repo.join("notes.txt").exists());
    assert_ne!(status(&ed, 0), Status::Integrated);

    // History only: a file added and removed again, while a cherry-pick
    // dialog is up.
    open_integrate(&mut ed);
    run(&mut ed, "<tab><tab>");
    assert!(dialog(&ed).0.contains("cherry-pick"), "{}", dialog(&ed).0);
    std::fs::write(wt.join("secret.txt"), "token\n").unwrap();
    f.git(&wt, &["add", "-A"]);
    f.git(&wt, &["commit", "-q", "-m", "add secret"]);
    f.git(&wt, &["rm", "-q", "secret.txt"]);
    f.git(&wt, &["commit", "-q", "-m", "remove secret"]);
    let msg = confirm_refused(&mut ed, "y");
    assert!(
        msg.contains("the agent's branch moved after the dialog was checked"),
        "{msg}"
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);

    // Uncommitted work, part of which the agent commits while the dialog
    // is up: Maestro's commit of the rest holds the same files, but sits
    // on a commit nobody reviewed.
    std::fs::write(wt.join("extra1.txt"), "1\n").unwrap();
    std::fs::write(wt.join("extra2.txt"), "2\n").unwrap();
    open_integrate(&mut ed);
    f.git(&wt, &["add", "extra1.txt"]);
    f.git(&wt, &["commit", "-q", "-m", "half of it"]);
    let msg = confirm_refused(&mut ed, "y");
    assert!(
        msg.contains("the agent's branch moved after the dialog was checked"),
        "{msg}"
    );
    assert!(
        msg.contains("the agent's work was committed on its branch"),
        "{msg}"
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);

    // Checked again, it integrates what the new dialog was about.
    open_integrate(&mut ed);
    run(&mut ed, "y");
    poll_until(&mut ed, "the integration", |ed| {
        status(ed, 0) == Status::Integrated
    });
    assert!(f.repo.join("notes.txt").exists());
}

/// The main worktree changes branch while Maestro commits the agent's work
/// (after its own checks of the main worktree): a pre-commit hook does it,
/// so the timing is exact. Nothing is integrated into either branch.
fn a_target_that_changed_during_the_commit_is_not_integrated_into() {
    let f = Fixture::new();
    let mut ed = f.editor(config(&[(
        "writer",
        sh_provider("printf 'notes\\n' > notes.txt"),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "write the notes", 0);
    wait_files(&mut ed, 0, &["notes.txt"]);
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    // Hooks live in the shared Git directory, so this one runs for the
    // commit in the agent's worktree. It switches the main worktree.
    let hook = f.repo.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nenv -u GIT_DIR -u GIT_INDEX_FILE -u GIT_WORK_TREE \
             git -C '{}' checkout -q -b elsewhere\n",
            f.repo.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    open_integrate(&mut ed);
    let msg = confirm_refused(&mut ed, "y");
    assert!(
        msg.contains("the main worktree is now on elsewhere, not main"),
        "{msg}"
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "main"]), main_head);
    assert_eq!(f.git(&f.repo, &["rev-parse", "elsewhere"]), main_head);
    assert!(!f.repo.join("notes.txt").exists());
}

// ----- marked hunks and composing ----------------------------------------------

/// Thirty lines, `line 1` to `line 30`.
fn thirty() -> String {
    (1..=30).map(|i| format!("line {i}\n")).collect()
}

/// A fixture whose repository also has `calc.txt` with [`thirty`] lines.
fn calc_fixture() -> Fixture {
    let f = Fixture::new();
    std::fs::write(f.repo.join("calc.txt"), thirty()).unwrap();
    f.git(&f.repo, &["add", "-A"]);
    f.git(&f.repo, &["commit", "-q", "-m", "calc"]);
    f
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn review_rows(ed: &Editor) -> Vec<Row> {
    maestro_editor::agents::review::current_rows(ed)
}

/// Waits for the agent's Git work (an integration) to finish.
fn wait_idle(ed: &mut Editor, i: usize) {
    poll_until(ed, "the agent's Git work", |ed| {
        agent(ed, i).git_busy.is_none()
    });
}

fn marked_hunks_integrate_alone_and_the_rest_stays() {
    use maestro_editor::agents::marks::{self, HunkState};
    let f = calc_fixture();
    let c = config(&[(
        "sh",
        sh_provider(
            "sed 's/^line 2$/line two/; s/^line 28$/line twenty-eight/' calc.txt > calc.tmp && mv calc.tmp calc.txt && \
             printf 'a\\nb\\nc\\n' > notes.txt",
        ),
    )]);
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_task(&mut ed, "tidy the calculator", 0);
    wait_files(&mut ed, 0, &["calc.txt", "notes.txt"]);
    let wt = worktree(&ed, 0);
    run(&mut ed, "\n");
    assert!(ed.agents.panel.review.is_some());
    // space marks the hunk under the cursor (the first one of calc.txt).
    assert!(matches!(review_rows(&ed)[0], Row::Hunk { index: 0, .. }));
    run(&mut ed, " ");
    assert_eq!(marks::counts(agent(&ed, 0)), (1, 2));
    // In notes.txt, v then j marks the first two of its three lines.
    run(&mut ed, "<tab>jvj ");
    assert_eq!(marks::counts(agent(&ed, 0)), (2, 4));
    assert!(
        marks::describe(agent(&ed, 0))
            .unwrap()
            .starts_with("2 hunks (4 lines) marked")
    );
    // I opens the dialog for only them.
    open_integrate(&mut ed);
    let (text, keys) = dialog(&ed);
    assert!(
        text.contains("Only the 2 marked hunk(s) of sh-1:"),
        "{text}"
    );
    assert!(text.contains("  calc.txt -1,5 +1,5\n"), "{text}");
    assert!(
        text.contains("  notes.txt -0,0 +1,3 (2 of 3 changed lines)"),
        "{text}"
    );
    let agent_branch = agent(&ed, 0).worktree.as_ref().unwrap().branch.clone();
    assert!(
        text.contains(&format!(
            "The rest of the work stays on {agent_branch} and in its worktree"
        )),
        "{text}"
    );
    assert!(
        text.contains("to the new branch maestro/sh-1-part-1"),
        "{text}"
    );
    assert_eq!(keys[0], ("y".into(), "integrate by merge".into()));
    run(&mut ed, "y");
    poll_until(&mut ed, "the part's integration", |ed| {
        agent(ed, 0).git_busy.is_none() && !agent(ed, 0).parts.is_empty()
    });
    // Main has exactly the marked hunks; the agent keeps all its work.
    assert_eq!(
        read(&f.repo.join("calc.txt")),
        thirty().replace("line 2\n", "line two\n")
    );
    assert_eq!(read(&f.repo.join("notes.txt")), "a\nb\n");
    assert_eq!(
        read(&wt.join("calc.txt")),
        thirty()
            .replace("line 2\n", "line two\n")
            .replace("line 28\n", "line twenty-eight\n")
    );
    assert_eq!(read(&wt.join("notes.txt")), "a\nb\nc\n");
    assert_ne!(status(&ed, 0), Status::Integrated);
    assert!(agent(&ed, 0).marks.is_empty());
    let part = f.git(
        &f.repo,
        &["log", "-1", "--format=%B", "maestro/sh-1-part-1"],
    );
    assert!(part.contains("- calc.txt -1,5 +1,5"), "{part}");
    assert!(
        part.contains("- notes.txt -0,0 +1,3 (2 of 3 changed lines)"),
        "{part}"
    );
    assert!(
        f.git(&f.repo, &["status", "--porcelain"]).is_empty(),
        "main is clean"
    );
    // The review shows which hunks went in.
    poll_until(&mut ed, "the refreshed review", |ed| {
        !agent(ed, 0).review.running
    });
    let a = agent(&ed, 0);
    let calc = &a.review.files[0];
    assert_eq!(
        marks::hunk_state(a, calc, 0),
        HunkState::Integrated {
            part: 1,
            whole: true
        }
    );
    assert_eq!(marks::hunk_state(a, calc, 1), HunkState::None);
    let notes = &a.review.files[1];
    assert_eq!(
        marks::hunk_state(a, notes, 0),
        HunkState::Integrated {
            part: 1,
            whole: false
        }
    );
    // An integrated hunk cannot be marked again.
    run(&mut ed, "{g ");
    assert!(
        status_text(&ed).contains("integrated in part 1"),
        "{:?}",
        ed.status
    );
    // The rest can be integrated later: the second hunk of calc.txt.
    run(&mut ed, "] ");
    assert_eq!(marks::counts(agent(&ed, 0)), (1, 2));
    // A dirty main worktree is refused before anything is committed, as
    // for a whole agent.
    let main_head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    std::fs::write(f.repo.join("README.md"), "# dirty\n").unwrap();
    run(&mut ed, "I");
    poll_until(&mut ed, "the refusal", |ed| {
        status_text(ed).contains("not integrated")
    });
    assert!(
        status_text(&ed).contains("the main worktree has uncommitted changes in README.md"),
        "{:?}",
        ed.status
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    assert!(
        !f.git(&f.repo, &["branch", "--list", "maestro/sh-1-part-2"])
            .contains("part-2")
    );
    f.git(&f.repo, &["checkout", "-q", "--", "README.md"]);
    // The agent changes the marked hunk while the dialog is up: the refresh
    // unmarks it, and y integrates nothing.
    open_integrate(&mut ed);
    let full = read(&wt.join("calc.txt"));
    std::fs::write(
        wt.join("calc.txt"),
        full.replace("line twenty-eight", "line XXVIII"),
    )
    .unwrap();
    let first = id(&ed, 0);
    maestro_editor::agents::review::request(&mut ed, first);
    poll_until(&mut ed, "the refresh that drops the mark", |ed| {
        agent(ed, 0).marks.is_empty()
    });
    let msg = confirm_refused(&mut ed, "y");
    assert!(
        msg.contains("the marked hunks changed after the dialog was checked"),
        "{msg}"
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), main_head);
    std::fs::write(wt.join("calc.txt"), &full).unwrap();
    let first = id(&ed, 0);
    maestro_editor::agents::review::request(&mut ed, first);
    poll_until(&mut ed, "the refresh", |ed| {
        !agent(ed, 0).review.running
            && agent(ed, 0).review.files[0]
                .hunks
                .iter()
                .any(|h| h.lines.iter().any(|l| l.text == "line twenty-eight"))
    });
    run(&mut ed, "g] ");
    assert_eq!(marks::counts(agent(&ed, 0)), (1, 2));
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(text.contains("  calc.txt -25,6 +25,6\n"), "{text}");
    assert!(text.contains("maestro/sh-1-part-2"), "{text}");
    run(&mut ed, "y");
    poll_until(&mut ed, "the second part", |ed| {
        agent(ed, 0).git_busy.is_none() && agent(ed, 0).parts.len() == 2
    });
    assert_eq!(read(&f.repo.join("calc.txt")), read(&wt.join("calc.txt")));
    assert_eq!(read(&f.repo.join("notes.txt")), "a\nb\n");
    // The parts survive a restart.
    quit(&mut ed);
    drop(ed);
    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 1);
    let a = agent(&ed, 0);
    assert_eq!(a.parts.len(), 2);
    assert_eq!(a.parts[0].branch, "maestro/sh-1-part-1");
    poll_until(&mut ed, "the loaded review", |ed| {
        agent(ed, 0).review.computed.is_some() && !agent(ed, 0).review.running
    });
    let a = agent(&ed, 0);
    assert!(matches!(
        marks::hunk_state(a, &a.review.files[0], 1),
        HunkState::Integrated { part: 2, .. }
    ));
}

/// A dependency: the marked hunk adds `lib/mod.rs` where the base has the
/// file `lib`, whose removal is not marked. Nothing is integrated, and the
/// refusal names the hunk.
fn a_marked_hunk_that_needs_an_unmarked_one_is_refused() {
    let f = Fixture::new();
    std::fs::write(f.repo.join("lib"), "a file\n").unwrap();
    f.git(&f.repo, &["add", "-A"]);
    f.git(&f.repo, &["commit", "-q", "-m", "lib"]);
    let head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    let mut ed = f.editor(config(&[(
        "sh",
        sh_provider("rm lib && mkdir lib && printf 'pub fn x() {}\\n' > lib/mod.rs"),
    )]));
    run(&mut ed, " a");
    start_task(&mut ed, "make lib a module", 0);
    wait_files(&mut ed, 0, &["lib", "lib/mod.rs"]);
    run(&mut ed, "\n");
    // The second file is lib/mod.rs.
    run(&mut ed, "<tab> ");
    assert_eq!(agent(&ed, 0).marks.len(), 1);
    run(&mut ed, "I");
    poll_until(&mut ed, "the refusal", |ed| {
        status_text(ed).contains("not integrated")
    });
    let msg = status_text(&ed);
    assert!(
        msg.contains("lib/mod.rs -0,0 +1,1 from sh-1: it adds lib/mod.rs, but lib is a file"),
        "{msg}"
    );
    assert!(ed.agents.panel.overlay.is_none());
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), head);
}

fn composing_hunks_of_two_agents_tests_and_integrates_the_result() {
    use maestro_editor::agents::Origin;
    use maestro_editor::agents::compose;
    let f = calc_fixture();
    // Not `sed -i`: its syntax differs between GNU and BSD (macOS) sed.
    let sed = |s: &str| {
        sh_provider(&format!(
            "sed '{s}' calc.txt > calc.tmp && mv calc.tmp calc.txt"
        ))
    };
    let mut c = config(&[
        // alpha changes lines 2 and 28; beta changes lines 3 (next to
        // alpha's line 2) and 15.
        ("alpha", sed("s/^line 2$/alpha 2/; s/^line 28$/alpha 28/")),
        ("beta", sed("s/^line 3$/beta 3/; s/^line 15$/beta 15/")),
    ]);
    c.agents.test_command = vec!["sh".into(), "-c".into(), "grep -c line calc.txt".into()];
    let mut ed = f.session_editor(c.clone());
    open_and_load(&mut ed, 0);
    start_contract_task(&mut ed, "improve the calculator\n@allow calc.txt", 0);
    wait_files(&mut ed, 0, &["calc.txt"]);
    // N starts beta on the same task, in the same group.
    run(&mut ed, "N");
    run(&mut ed, "\n");
    wait_files(&mut ed, 1, &["calc.txt"]);
    assert_eq!(agent(&ed, 1).group, agent(&ed, 0).group);
    verify(&mut ed, 1);
    // C on alpha opens the compose view for the group.
    select(&mut ed, 0);
    run(&mut ed, "C");
    let v = ed.agents.panel.compose.clone().expect("the compose view");
    assert_eq!(v.agents, vec![id(&ed, 0), id(&ed, 1)]);
    assert_eq!(compose::files(&ed, &v), [PathBuf::from("calc.txt")]);
    // The hunks side by side: alpha's line 2 and beta's line 3 share a
    // band; then beta's line 15; then alpha's line 28.
    let items = compose::items(&ed, &v);
    let shape: Vec<(usize, usize, usize)> = items.iter().map(|i| (i.col, i.hunk, i.band)).collect();
    assert_eq!(shape, [(0, 0, 0), (1, 0, 0), (1, 1, 1), (0, 1, 2)]);
    // Marking alpha's and beta's neighboring hunks: the check names both.
    run(&mut ed, " l \n");
    poll_until(&mut ed, "the check", |ed| {
        ed.agents
            .panel
            .compose
            .as_ref()
            .is_some_and(|c| !c.checking)
    });
    let msg = status_text(&ed);
    assert!(
        msg.contains("not composed: the marked hunks do not apply together"),
        "{msg}"
    );
    assert!(
        msg.contains(
            "calc.txt -1,5 +1,5 from alpha-1: it conflicts with calc.txt -1,6 +1,6 from beta-2"
        ),
        "{msg}"
    );
    assert!(
        msg.contains(
            "calc.txt -1,6 +1,6 from beta-2: it conflicts with calc.txt -1,5 +1,5 from alpha-1"
        ),
        "{msg}"
    );
    assert_eq!(ed.agents.panel.compose.as_ref().unwrap().problems.len(), 2);
    assert!(ed.agents.panel.overlay.is_none());
    // Beta's line 15 instead of its line 3: they apply together.
    // Beta changes its marked hunk: the refresh drops that mark from the
    // compose view too, so it cannot block the next check.
    let beta_wt = worktree(&ed, 1);
    let beta_text = read(&beta_wt.join("calc.txt"));
    std::fs::write(
        beta_wt.join("calc.txt"),
        beta_text.replace("beta 3", "beta three"),
    )
    .unwrap();
    let beta = id(&ed, 1);
    maestro_editor::agents::review::request(&mut ed, beta);
    poll_until(&mut ed, "the refresh that drops beta's mark", |ed| {
        ed.agents.panel.compose.as_ref().unwrap().marks.len() == 1
    });
    assert!(status_text(&ed).contains("unmarked"), "{:?}", ed.status);
    run(&mut ed, "j \n");
    poll_until(&mut ed, "the compose dialog", |ed| {
        matches!(&ed.agents.panel.overlay,
            Some(Overlay::Confirm(c)) if matches!(c.action, ConfirmAction::Compose(_)))
    });
    let (text, _) = dialog(&ed);
    assert!(
        text.contains("  calc.txt -1,5 +1,5 from alpha-1 (no tests run)"),
        "{text}"
    );
    assert!(
        text.contains("  calc.txt -12,7 +12,7 from beta-2 (tests ok)"),
        "{text}"
    );
    run(&mut ed, "y");
    poll_until(&mut ed, "the composed result", |ed| {
        ed.agents.agents.len() == 3 && agent(ed, 2).git_busy.is_none()
    });
    let want = thirty()
        .replace("line 2\n", "alpha 2\n")
        .replace("line 15\n", "beta 15\n");
    let composed = agent(&ed, 2);
    assert_eq!(composed.origin, Origin::Composed, "{:?}", composed.errors);
    assert_eq!(composed.name, "compose-3");
    assert_eq!(composed.task, agent(&ed, 0).task);
    assert_eq!(composed.contract, agent(&ed, 0).contract);
    assert_eq!(composed.group, agent(&ed, 0).group);
    assert_eq!(composed.status(), Status::ReadyForReview);
    let w = composed.worktree.clone().unwrap();
    assert_eq!(w.branch, "maestro/compose-3");
    assert_eq!(w.base, agent(&ed, 0).worktree.as_ref().unwrap().base);
    assert_eq!(read(&w.path.join("calc.txt")), want);
    // The agents' worktrees are untouched, and so is main.
    assert!(read(&worktree(&ed, 0).join("calc.txt")).contains("alpha 28"));
    assert!(read(&worktree(&ed, 1).join("calc.txt")).contains("beta three"));
    assert_eq!(read(&f.repo.join("calc.txt")), thirty());
    let message = f.git(&f.repo, &["log", "-1", "--format=%B", "maestro/compose-3"]);
    assert!(
        message.contains("- calc.txt -1,5 +1,5 from alpha-1 (no tests run)"),
        "{message}"
    );
    assert!(
        message.contains("- calc.txt -12,7 +12,7 from beta-2 (tests ok)"),
        "{message}"
    );

    // The composition survives a restart.
    let sources = composed.composition.clone().unwrap();
    quit(&mut ed);
    drop(ed);
    let mut ed = f.session_editor(c);
    open_and_load(&mut ed, 3);
    let i = index_of(&ed, "compose-3");
    let composed = agent(&ed, i);
    assert_eq!(composed.origin, Origin::Composed);
    assert_eq!(composed.composition.as_ref(), Some(&sources));
    assert!(!composed.from_earlier_session());
    // It is tested and integrated like any agent, through the same path.
    select(&mut ed, i);
    assert!(hint_keys(&ed).contains(&("t", "test")));
    assert!(!hint_keys(&ed).contains(&("f", "feedback")));
    run(&mut ed, "f");
    assert!(
        status_text(&ed).contains("composed result"),
        "{:?}",
        ed.status
    );
    verify(&mut ed, i);
    assert_eq!(agent(&ed, i).latest_test().unwrap().passed(), Some(true));
    open_integrate(&mut ed);
    let (text, _) = dialog(&ed);
    assert!(!text.contains("Warning:"), "{text}");
    assert!(!text.contains("OUTSIDE"), "{text}");
    run(&mut ed, "y");
    poll_until(&mut ed, "the integration", |ed| {
        status(ed, i) == Status::Integrated
    });
    wait_idle(&mut ed, i);
    assert_eq!(read(&f.repo.join("calc.txt")), want);
    let merged = f.git(&f.repo, &["log", "-1", "--format=%B", "HEAD^2"]);
    assert!(
        merged.contains("from alpha-1") && merged.contains("from beta-2"),
        "{merged}"
    );
}
