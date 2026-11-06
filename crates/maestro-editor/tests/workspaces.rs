//! Workspaces: the sidebar, switching worktrees, the jump palette, agent
//! attention and notifications, and language servers per root. Keys in
//! through `Editor::handle_key`, real Git repositories and worktrees, `sh`
//! agent providers, and the fake language server.
//!
//! `harness = false`: started with `--maestro-fake-lsp`, this binary is the
//! fake language server instead (see `maestro_lsp::fake`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::{
    AgentProviderConfig, AgentProviderKind, Config, LanguageServerConfig, NotifyMethod,
};
use maestro_editor::agents::Status;
use maestro_editor::editor::Severity;
use maestro_editor::git::GitState;
use maestro_editor::navigation::session;
use maestro_editor::picker::PickerAction;
use maestro_editor::prompt::PromptKind;
use maestro_editor::workspaces::manage::{self, ProjectList};
use maestro_editor::workspaces::sidebar::{self, Row, RowKey};
use maestro_editor::workspaces::{Attention, NeedsYou};
use maestro_editor::{Editor, KeyCode, KeyEvent, Rect};
use maestro_git::Git;
use maestro_lsp::fake::{self, FAKE_ARG};

fn main() {
    fake::run_if_requested();
    fake::run_tests(&[
        (
            "switching_saves_and_restores_each_worktrees_session",
            switching_saves_and_restores_each_worktrees_session,
        ),
        (
            "the_finder_and_search_list_the_new_worktrees_files",
            the_finder_and_search_list_the_new_worktrees_files,
        ),
        (
            "unsaved_buffers_stay_open_and_are_listed",
            unsaved_buffers_stay_open_and_are_listed,
        ),
        (
            "sessions_switch_in_memory_without_a_state_directory",
            sessions_switch_in_memory_without_a_state_directory,
        ),
        (
            "the_sidebar_lists_worktrees_with_real_git_facts",
            the_sidebar_lists_worktrees_with_real_git_facts,
        ),
        (
            "escape_leaves_the_sidebar_open_and_space_s_toggles_it",
            escape_leaves_the_sidebar_open_and_space_s_toggles_it,
        ),
        (
            "agents_that_need_you_come_first_and_notify_once_per_change",
            agents_that_need_you_come_first_and_notify_once_per_change,
        ),
        (
            "notify_off_still_says_so_in_the_status_line",
            notify_off_still_says_so_in_the_status_line,
        ),
        (
            "the_jump_palette_goes_to_worktrees_agents_and_buffers",
            the_jump_palette_goes_to_worktrees_agents_and_buffers,
        ),
        (
            "language_servers_start_for_the_new_root_and_stop_when_unused",
            language_servers_start_for_the_new_root_and_stop_when_unused,
        ),
        (
            "a_server_with_an_open_buffer_is_never_stopped",
            a_server_with_an_open_buffer_is_never_stopped,
        ),
        (
            "servers_beyond_the_root_limit_stop_least_recently_used_first",
            servers_beyond_the_root_limit_stop_least_recently_used_first,
        ),
        (
            "a_switch_after_cd_saves_the_session_the_files_came_from",
            a_switch_after_cd_saves_the_session_the_files_came_from,
        ),
        (
            "a_worktree_the_scan_has_not_listed_keeps_its_own_session",
            a_worktree_the_scan_has_not_listed_keeps_its_own_session,
        ),
        (
            "a_path_through_dot_dot_belongs_to_the_worktree_it_names",
            a_path_through_dot_dot_belongs_to_the_worktree_it_names,
        ),
        (
            "paste_and_keys_with_the_sidebar",
            paste_and_keys_with_the_sidebar,
        ),
        (
            "a_verification_that_cannot_start_needs_you",
            a_verification_that_cannot_start_needs_you,
        ),
        (
            "the_palette_lists_a_bare_repositorys_worktrees_and_scratch_buffers",
            the_palette_lists_a_bare_repositorys_worktrees_and_scratch_buffers,
        ),
        (
            "a_nested_worktrees_server_stops_when_you_leave_it",
            a_nested_worktrees_server_stops_when_you_leave_it,
        ),
        (
            "adding_a_project_lists_it_selected_and_keeps_it",
            adding_a_project_lists_it_selected_and_keeps_it,
        ),
        (
            "tab_completes_directories_in_the_add_prompt",
            tab_completes_directories_in_the_add_prompt,
        ),
        (
            "adding_refuses_missing_paths_and_directories_outside_git",
            adding_refuses_missing_paths_and_directories_outside_git,
        ),
        (
            "forgetting_asks_first_and_refuses_configured_projects",
            forgetting_asks_first_and_refuses_configured_projects,
        ),
        (
            "typed_commands_and_the_palette_add_and_forget_projects",
            typed_commands_and_the_palette_add_and_forget_projects,
        ),
        (
            "without_a_state_directory_the_list_is_neither_read_nor_written",
            without_a_state_directory_the_list_is_neither_read_nor_written,
        ),
        (
            "a_malformed_project_list_is_ignored_with_a_note",
            a_malformed_project_list_is_ignored_with_a_note,
        ),
        (
            "adding_before_the_first_scan_selects_the_project_when_it_arrives",
            adding_before_the_first_scan_selects_the_project_when_it_arrives,
        ),
        (
            "an_added_worktree_of_a_bare_repository_stays_usable",
            an_added_worktree_of_a_bare_repository_stays_usable,
        ),
    ]);
}

const WAIT: Duration = Duration::from_secs(20);

static NEXT: AtomicUsize = AtomicUsize::new(0);

const GIT_CONFIG: &[&str] = &[
    "user.name=Maestro Test",
    "user.email=test@example.invalid",
    "init.defaultBranch=main",
    "commit.gpgSign=false",
];

/// Two repositories, `alpha` (with a second worktree `alpha-feature` on
/// branch `feature`) and `beta`, in a temporary directory.
struct Fixture {
    tmp: PathBuf,
    alpha: PathBuf,
    feature: PathBuf,
    beta: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

impl Fixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("maestro-workspaces-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let f = Fixture {
            alpha: tmp.join("alpha"),
            feature: tmp.join("alpha-feature"),
            beta: tmp.join("beta"),
            tmp,
        };
        for (repo, files) in [
            (
                &f.alpha,
                &[
                    ("a.txt", "alpha one\n"),
                    ("b.txt", "alpha two\n"),
                    ("src/lib.rs", "pub fn alpha() {}\n"),
                ][..],
            ),
            (
                &f.beta,
                &[
                    ("beta.txt", "beta only\n"),
                    ("src/lib.rs", "pub fn beta() {}\n"),
                ][..],
            ),
        ] {
            std::fs::create_dir_all(repo.join("src")).unwrap();
            f.git(repo, &["init", "-q"]);
            for (name, text) in files {
                std::fs::write(repo.join(name), text).unwrap();
            }
            f.git(repo, &["add", "-A"]);
            f.git(repo, &["commit", "-q", "-m", "initial"]);
        }
        f.git(
            &f.alpha,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                f.feature.to_str().unwrap(),
            ],
        );
        std::fs::write(f.feature.join("feature_only.txt"), "needle in feature\n").unwrap();
        f.git(&f.feature, &["add", "-A"]);
        f.git(&f.feature, &["commit", "-q", "-m", "feature file"]);
        f
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        for c in GIT_CONFIG {
            cmd.arg("-c").arg(c);
        }
        let out = cmd
            .args(args)
            .current_dir(dir)
            .env("HOME", self.tmp.join("home"))
            .env("XDG_CONFIG_HOME", self.tmp.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.tmp.join("home/.gitconfig"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
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

    fn state(&self) -> PathBuf {
        self.tmp.join("state")
    }

    /// A config that lists `beta` as a project and hides the built-in
    /// agent providers.
    fn config(&self) -> Config {
        let mut c = Config::default();
        c.projects.paths = vec![self.beta.display().to_string()];
        for builtin in ["gemini", "opencode", "claude", "codex"] {
            c.agents.provider.insert(
                builtin.into(),
                AgentProviderConfig {
                    enabled: false,
                    ..sh_provider("true")
                },
            );
        }
        c
    }

    /// An editor in `alpha`, with sessions saved to [`Self::state`] when
    /// `sessions` is set.
    fn editor(&self, config: Config, sessions: bool) -> Editor {
        let mut ed = Editor::new(config, Rect::new(0, 1, 120, 38));
        ed.git = GitState::new(self.api());
        ed.cwd = self.alpha.clone();
        if sessions {
            maestro_editor::navigation::configure(&mut ed, Some(self.state()), true);
        }
        ed
    }
}

fn sh_provider(script: &str) -> AgentProviderConfig {
    AgentProviderConfig {
        kind: AgentProviderKind::Terminal,
        command: "sh".into(),
        args: vec!["-c".into(), script.into(), "sh".into(), "{task}".into()],
        env: Default::default(),
        enabled: true,
    }
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

fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status {:?}",
            ed.status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until background work (the scan among it) is done.
fn settle(ed: &mut Editor) {
    poll_until(ed, "the scan", |ed| {
        ed.workspaces.scanned && !ed.has_background_work()
    });
}

fn status_text(ed: &Editor) -> String {
    ed.status
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_default()
}

/// Opens (or focuses) the sidebar and waits for the scan.
fn open_sidebar(ed: &mut Editor) {
    if !ed.workspaces.sidebar.has_focus() {
        run(ed, " s");
    }
    assert!(ed.workspaces.sidebar.has_focus());
    settle(ed);
}

/// Moves the sidebar selection to `want` with `g` and `j`, unfolding
/// projects with `l` on the way when `want` is a worktree.
fn select_row(ed: &mut Editor, want: &RowKey) {
    run(ed, "g");
    for _ in 0..60 {
        let rows = sidebar::rows(ed);
        let at = sidebar::selected_index(ed, &rows).map(|i| rows[i].key());
        if at.as_ref() == Some(&Some(want.clone())) {
            return;
        }
        if let (RowKey::Worktree(w), Some(Some(RowKey::Project(p)))) = (want, &at)
            && w.starts_with(p.parent().unwrap())
            && !ed.workspaces.sidebar.expanded.contains(p)
        {
            run(ed, "l");
            continue;
        }
        run(ed, "j");
    }
    panic!("no sidebar row {want:?} in {:#?}", sidebar::rows(ed));
}

/// Switches to the worktree at `path` through the sidebar.
fn switch_with_sidebar(ed: &mut Editor, path: &Path) {
    open_sidebar(ed);
    select_row(ed, &RowKey::Worktree(path.to_path_buf()));
    run(ed, "\n");
    assert_eq!(ed.cwd, path, "status {:?}", ed.status);
    assert!(
        !ed.workspaces.sidebar.focused,
        "focus goes back to the editor"
    );
    assert!(ed.workspaces.sidebar.open);
    settle(ed);
}

fn open_paths(ed: &Editor) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ed
        .buffers
        .values()
        .filter_map(|b| b.doc.path().map(Path::to_path_buf))
        .collect();
    v.sort();
    v
}

fn shown_path(ed: &Editor) -> Option<PathBuf> {
    ed.doc().path().map(Path::to_path_buf)
}

// ----- switching and sessions ---------------------------------------------

fn switching_saves_and_restores_each_worktrees_session() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), true);
    run(&mut ed, ":e a.txt\n");
    run(&mut ed, ":vsplit\n");
    run(&mut ed, ":e b.txt\n");
    run(&mut ed, "l");
    assert_eq!(ed.layout.views().len(), 2);

    switch_with_sidebar(&mut ed, &f.beta);
    // Alpha's clean files are closed and saved as alpha's session, with
    // the split.
    assert_eq!(open_paths(&ed), Vec::<PathBuf>::new());
    assert_eq!(ed.layout.views().len(), 1);
    assert!(
        status_text(&ed).contains("switched to beta:main"),
        "{}",
        status_text(&ed)
    );
    let saved = session::load(&f.state(), &f.alpha).unwrap().unwrap();
    let names: Vec<&str> = saved.buffers.iter().map(|b| b.path.as_str()).collect();
    assert_eq!(
        names,
        [
            f.alpha.join("a.txt").to_str().unwrap(),
            f.alpha.join("b.txt").to_str().unwrap()
        ]
    );
    assert!(matches!(
        saved.layout,
        Some(session::SessionNode::Split { .. })
    ));
    assert_eq!(ed.navigation.session_cwd.as_deref(), Some(f.beta.as_path()));

    run(&mut ed, ":e beta.txt\n");
    switch_with_sidebar(&mut ed, &f.alpha);
    // Alpha's files and split are back, with the focused view on b.txt.
    assert_eq!(
        open_paths(&ed),
        [f.alpha.join("a.txt"), f.alpha.join("b.txt")]
    );
    assert_eq!(ed.layout.views().len(), 2);
    assert_eq!(shown_path(&ed), Some(f.alpha.join("b.txt")));
    assert_eq!(
        ed.view().selection.primary().head,
        1,
        "the cursor came back"
    );
    assert!(
        status_text(&ed).contains("restored session: 2 files"),
        "{}",
        status_text(&ed)
    );
    let beta = session::load(&f.state(), &f.beta).unwrap().unwrap();
    assert_eq!(beta.buffers.len(), 1);
    assert_eq!(
        beta.buffers[0].path,
        f.beta.join("beta.txt").to_str().unwrap()
    );

    // Quitting saves the current worktree's session, only its files.
    run(&mut ed, ":e /etc/hostname\n");
    run(&mut ed, ":qa\n");
    assert!(ed.should_quit);
    let alpha = session::load(&f.state(), &f.alpha).unwrap().unwrap();
    assert_eq!(alpha.buffers.len(), 2, "{alpha:?}");
}

fn the_finder_and_search_list_the_new_worktrees_files() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), true);
    switch_with_sidebar(&mut ed, &f.feature);
    assert!(
        status_text(&ed).contains("switched to alpha:alpha-feature"),
        "{}",
        status_text(&ed)
    );
    run(&mut ed, " f");
    poll_until(&mut ed, "the file list", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading)
    });
    let labels: Vec<String> = ed
        .picker
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|i| i.label.clone())
        .collect();
    assert!(
        labels.contains(&"feature_only.txt".to_string()),
        "{labels:?}"
    );
    assert!(labels.contains(&"a.txt".to_string()));
    run(&mut ed, "<esc>");
    // Workspace search reads the new worktree.
    run(&mut ed, " /needle\n");
    poll_until(&mut ed, "the search", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading)
    });
    let items = ed.picker.as_ref().unwrap().items().to_vec();
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(
        items[0].label.starts_with("feature_only.txt:1"),
        "{items:?}"
    );
    run(&mut ed, "\n");
    assert_eq!(shown_path(&ed), Some(f.feature.join("feature_only.txt")));

    // Back in alpha the file is not there.
    switch_with_sidebar(&mut ed, &f.alpha);
    run(&mut ed, " f");
    poll_until(&mut ed, "the file list", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading)
    });
    let labels: Vec<String> = ed
        .picker
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|i| i.label.clone())
        .collect();
    assert!(
        !labels.contains(&"feature_only.txt".to_string()),
        "{labels:?}"
    );
}

fn unsaved_buffers_stay_open_and_are_listed() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), true);
    run(&mut ed, ":e a.txt\n");
    run(&mut ed, ":vsplit\n:e b.txt\n");
    run(&mut ed, "ichanged <esc>");
    assert!(ed.doc().is_modified());
    let b = ed.view().doc;

    switch_with_sidebar(&mut ed, &f.beta);
    // b.txt stays open with its edit; a.txt (clean) is closed.
    assert_eq!(open_paths(&ed), [f.alpha.join("b.txt")]);
    assert!(ed.buffers[&b].doc.is_modified());
    assert_eq!(ed.buffers[&b].doc.text().to_string(), "changed alpha two\n");
    let msg = status_text(&ed);
    assert!(
        msg.contains("kept open with unsaved changes: b.txt (from alpha:main)"),
        "{msg}"
    );
    // The sidebar counts it on alpha's row; quitting refuses.
    let rows = sidebar::rows(&ed);
    let unsaved = rows.iter().find_map(|r| match r {
        Row::Worktree { row, unsaved, .. } if row.path == f.alpha => Some(*unsaved),
        _ => None,
    });
    assert_eq!(unsaved, Some(1), "{rows:#?}");
    run(&mut ed, ":qa\n");
    assert!(!ed.should_quit);
    assert!(status_text(&ed).contains("unsaved changes"));
    // Alpha's session names both files; going back shows the edit.
    let saved = session::load(&f.state(), &f.alpha).unwrap().unwrap();
    assert_eq!(saved.buffers.len(), 2);
    switch_with_sidebar(&mut ed, &f.alpha);
    assert_eq!(ed.view().doc, b, "the same buffer, not a reload");
    assert_eq!(ed.doc().text().to_string(), "changed alpha two\n");
    assert!(ed.doc().is_modified());
    assert_eq!(
        std::fs::read_to_string(f.alpha.join("b.txt")).unwrap(),
        "alpha two\n"
    );
}

fn sessions_switch_in_memory_without_a_state_directory() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), false);
    run(&mut ed, ":e a.txt\n");
    switch_with_sidebar(&mut ed, &f.beta);
    assert_eq!(open_paths(&ed), Vec::<PathBuf>::new());
    assert!(status_text(&ed).contains("no saved session"));
    switch_with_sidebar(&mut ed, &f.alpha);
    assert_eq!(open_paths(&ed), [f.alpha.join("a.txt")]);
    assert!(!f.state().exists());
}

fn the_sidebar_lists_worktrees_with_real_git_facts() {
    let f = Fixture::new();
    // An upstream for alpha's main branch: a bare clone it pushes to, one
    // commit behind.
    let remote = f.tmp.join("remote.git");
    f.git(
        &f.tmp,
        &[
            "clone",
            "-q",
            "--bare",
            f.alpha.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    f.git(
        &f.alpha,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    f.git(&f.alpha, &["fetch", "-q", "origin"]);
    f.git(&f.alpha, &["branch", "-q", "--set-upstream-to=origin/main"]);
    f.git(&f.alpha, &["commit", "-q", "--allow-empty", "-m", "local"]);
    std::fs::write(f.alpha.join("new.txt"), "x").unwrap();
    std::fs::write(f.alpha.join("a.txt"), "edited\n").unwrap();
    let mut ed = f.editor(f.config(), false);
    open_sidebar(&mut ed);
    let rows = sidebar::rows(&ed);
    let projects: Vec<&str> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Project { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    // The configured project first, then the working directory's.
    assert_eq!(projects, ["beta", "alpha"]);
    let wt: Vec<_> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Worktree { row, current, .. } => Some((row.clone(), *current)),
            _ => None,
        })
        .collect();
    // Alpha is unfolded (current), main worktree first; beta is folded.
    assert_eq!(wt.len(), 2, "{rows:#?}");
    let (main, current) = &wt[0];
    assert!(main.is_main && *current);
    assert_eq!(main.branch.as_deref(), Some("main"));
    assert_eq!(main.dirty, Some(2));
    let up = main.upstream.as_ref().unwrap();
    assert_eq!((up.ahead, up.behind), (1, 0));
    let (feature, current) = &wt[1];
    assert_eq!(feature.path, f.feature);
    assert_eq!(feature.branch.as_deref(), Some("feature"));
    assert_eq!(feature.dirty, Some(0));
    assert!(!current);
    // No upstream: nothing is guessed.
    assert_eq!(feature.upstream, None);
    // The status line label.
    assert_eq!(
        maestro_editor::workspaces::status_label(&ed).as_deref(),
        Some("alpha:main")
    );
    // A configured path that is not a repository is listed with why.
    let mut config = f.config();
    config
        .projects
        .paths
        .push(f.tmp.join("home").display().to_string());
    let mut ed = f.editor(config, false);
    open_sidebar(&mut ed);
    let errs: Vec<String> = sidebar::rows(&ed)
        .iter()
        .filter_map(|r| match r {
            Row::Project { error: Some(e), .. } => Some(e.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("not in a Git repository"), "{errs:?}");
}

fn escape_leaves_the_sidebar_open_and_space_s_toggles_it() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), false);
    run(&mut ed, ":e a.txt\n");
    run(&mut ed, " s");
    assert!(ed.workspaces.sidebar.open && ed.workspaces.sidebar.focused);
    // Keys go to the sidebar, not the buffer.
    run(&mut ed, "d");
    assert!(!ed.doc().is_modified());
    run(&mut ed, "<esc>");
    assert!(ed.workspaces.sidebar.open && !ed.workspaces.sidebar.focused);
    // Now keys edit again.
    run(&mut ed, "d");
    assert!(ed.doc().is_modified());
    run(&mut ed, "u");
    run(&mut ed, " s");
    assert!(ed.workspaces.sidebar.has_focus());
    // A space command from the sidebar acts on the editor.
    run(&mut ed, " f");
    assert!(ed.picker.is_some());
    assert!(!ed.workspaces.sidebar.focused);
    run(&mut ed, "<esc>");
    run(&mut ed, " s");
    run(&mut ed, " s");
    assert!(!ed.workspaces.sidebar.open);
    run(&mut ed, " s");
    run(&mut ed, "q");
    assert!(!ed.workspaces.sidebar.open);
}

// ----- agents --------------------------------------------------------------

fn start_task(ed: &mut Editor, task: &str, provider_index: usize) {
    run(ed, "n");
    ed.paste_text(task);
    run(ed, "\n");
    for _ in 0..provider_index {
        run(ed, "j");
    }
    run(ed, "\n");
}

fn agent_index(ed: &Editor, provider: &str) -> usize {
    ed.agents
        .agents
        .iter()
        .position(|a| a.provider.name == provider)
        .unwrap()
}

fn agents_that_need_you_come_first_and_notify_once_per_change() {
    let f = Fixture::new();
    let mut config = f.config();
    config
        .agents
        .provider
        .insert("quick".into(), sh_provider("printf done > notes.txt"));
    config
        .agents
        .provider
        .insert("slow".into(), sh_provider("sleep 30"));
    config.agents.test_command = vec!["sh".into(), "-c".into(), "exit 3".into()];
    config.notify.method = NotifyMethod::Osc;
    let mut ed = f.editor(config, false);
    run(&mut ed, " a");
    // Providers are listed by name: quick, slow.
    start_task(&mut ed, "slow work", 1);
    poll_until(&mut ed, "the slow agent", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.status() == Status::Running)
    });
    start_task(&mut ed, "quick work", 0);
    let quick = agent_index(&ed, "quick");
    poll_until(&mut ed, "the quick agent's exit", |ed| {
        ed.agents.agents[quick].status() == Status::ReadyForReview
    });
    let name = ed.agents.agents[quick].name.clone();
    assert_eq!(ed.workspaces.notifications.len(), 1);
    let note = &ed.workspaces.notifications[0];
    assert_eq!(note.method, NotifyMethod::Osc);
    assert_eq!(
        note.text,
        format!("Maestro: {name} needs you: ready for review")
    );
    assert!(status_text(&ed).contains(&format!("{name} needs you: ready for review")));
    // The same state does not notify again.
    for _ in 0..50 {
        ed.poll_background();
        ed.tick(Instant::now());
    }
    assert_eq!(ed.workspaces.notifications.len(), 1);

    // The sidebar: needs you first, then working.
    run(&mut ed, "<esc>");
    open_sidebar(&mut ed);
    let rows = sidebar::rows(&ed);
    let heads: Vec<String> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Heading(h) => Some(h.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(heads, ["needs you (1)", "working (1)", "projects"]);
    match (&rows[1], &rows[3]) {
        (Row::Agent(a), Row::Agent(b)) => {
            assert_eq!(a.name, name);
            assert_eq!(a.attention, Attention::NeedsYou(NeedsYou::Review));
            assert_eq!(b.attention, Attention::Working);
        }
        other => panic!("{other:?}"),
    }
    // Its worktree row shows the agent's name and state.
    let agent_wt = ed.agents.agents[quick]
        .worktree
        .as_ref()
        .unwrap()
        .path
        .clone();
    select_row(&mut ed, &RowKey::Worktree(agent_wt.clone()));
    let rows = sidebar::rows(&ed);
    let row_agent = rows.iter().find_map(|r| match r {
        Row::Worktree { row, agent, .. } if row.path == agent_wt => agent.clone(),
        _ => None,
    });
    assert_eq!(row_agent.map(|a| a.name), Some(name.clone()));

    // A failed test run is a new state: one more notification.
    run(&mut ed, "<esc> a");
    while ed.agents.panel.selected != quick {
        let key = if ed.agents.panel.selected < quick {
            "j"
        } else {
            "k"
        };
        run(&mut ed, key);
    }
    run(&mut ed, "t");
    poll_until(&mut ed, "the test run", |ed| {
        ed.agents.agents[quick]
            .latest_test()
            .is_some_and(|t| t.finished.is_some())
    });
    ed.poll_background();
    assert_eq!(
        ed.workspaces.notifications.len(),
        2,
        "{:?}",
        ed.workspaces.notifications
    );
    assert!(ed.workspaces.notifications[1].text.contains("tests failed"));
    let taken = ed.workspaces.take_notifications();
    assert_eq!(taken.len(), 2);
    assert!(ed.workspaces.notifications.is_empty());
    // Choosing the agent in the sidebar switches to its worktree.
    run(&mut ed, "<esc>");
    open_sidebar(&mut ed);
    let key = RowKey::Agent(format!("worktree:{}", agent_wt.display()));
    select_row(&mut ed, &key);
    run(&mut ed, "\n");
    assert_eq!(ed.cwd, agent_wt);
}

fn notify_off_still_says_so_in_the_status_line() {
    let f = Fixture::new();
    let mut config = f.config();
    config
        .agents
        .provider
        .insert("broken".into(), sh_provider("exit 4"));
    config.notify.method = NotifyMethod::Off;
    let mut ed = f.editor(config, false);
    run(&mut ed, " a");
    start_task(&mut ed, "fail", 0);
    poll_until(&mut ed, "the failure", |ed| {
        ed.agents
            .agents
            .first()
            .is_some_and(|a| a.status() == Status::Failed)
    });
    assert!(
        status_text(&ed).contains("needs you: failed"),
        "{}",
        status_text(&ed)
    );
    assert!(ed.workspaces.notifications.is_empty());
}

// ----- the jump palette ----------------------------------------------------

fn the_jump_palette_goes_to_worktrees_agents_and_buffers() {
    let f = Fixture::new();
    let mut config = f.config();
    config
        .agents
        .provider
        .insert("quick".into(), sh_provider("printf done > notes.txt"));
    let mut ed = f.editor(config, true);
    run(&mut ed, " a");
    poll_until(&mut ed, "the saved records", |ed| {
        !ed.agents.persist.loading
    });
    start_task(&mut ed, "write the release notes", 0);
    poll_until(&mut ed, "the agent", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    let agent_wt = ed.agents.agents[0].worktree.as_ref().unwrap().path.clone();
    run(&mut ed, "<esc>");
    run(&mut ed, ":e a.txt\nifix <esc>");

    run(&mut ed, " j");
    poll_until(&mut ed, "the palette", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading) && !ed.has_background_work()
    });
    let labels: Vec<String> = ed
        .picker
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|i| i.label.clone())
        .collect();
    for want in [
        "project alpha",
        "project beta",
        "worktree alpha:main main",
        "worktree alpha:alpha-feature feature",
        "worktree beta:main main",
        "buffer a.txt",
    ] {
        assert!(labels.iter().any(|l| l == want), "{want} in {labels:?}");
    }
    assert!(
        labels
            .iter()
            .any(|l| l.starts_with("agent quick-") && l.ends_with("write the release notes")),
        "{labels:?}"
    );
    // A worktree by name.
    run(&mut ed, "alpha-feature\n");
    assert_eq!(ed.cwd, f.feature);
    // An agent by its task.
    run(&mut ed, " jrelease notes");
    assert!(matches!(
        ed.picker.as_ref().unwrap().items()[ed.picker.as_ref().unwrap().matches()[0].index].action,
        PickerAction::Jump(_)
    ));
    run(&mut ed, "\n");
    assert_eq!(ed.cwd, agent_wt);
    // The unsaved buffer in alpha: back to alpha, showing it.
    run(&mut ed, " jbuffer a.txt\n");
    assert_eq!(ed.cwd, f.alpha);
    assert_eq!(shown_path(&ed), Some(f.alpha.join("a.txt")));
    assert_eq!(ed.doc().text().to_string(), "fix alpha one\n");
}

// ----- language servers ----------------------------------------------------

fn lsp_config() -> Config {
    let mut c = Config::default();
    c.language_server.insert(
        "rust".into(),
        LanguageServerConfig {
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![FAKE_ARG.to_string()],
            enabled: true,
        },
    );
    c
}

fn roots(ed: &Editor) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ed.lsp.server_roots().into_iter().map(|(_, r)| r).collect();
    v.sort();
    v
}

/// An editor with language servers on, in alpha with `src/lib.rs` synced,
/// switched to beta with its `src/lib.rs` synced.
fn two_roots(f: &Fixture, mut config: Config) -> Editor {
    config.projects.paths = vec![f.beta.display().to_string()];
    let mut ed = f.editor(config, false);
    ed.lsp.enabled = true;
    run(&mut ed, ":e src/lib.rs\n");
    let doc = ed.view().doc;
    poll_until(&mut ed, "alpha's server", |ed| ed.lsp.is_synced(doc));
    switch_with_sidebar(&mut ed, &f.beta);
    run(&mut ed, ":e src/lib.rs\n");
    let doc = ed.view().doc;
    poll_until(&mut ed, "beta's server", |ed| ed.lsp.is_synced(doc));
    ed
}

fn language_servers_start_for_the_new_root_and_stop_when_unused() {
    let f = Fixture::new();
    let mut config = lsp_config();
    config.lsp.max_live_roots = 0;
    let mut ed = two_roots(&f, config);
    assert_eq!(roots(&ed), [f.alpha.clone(), f.beta.clone()]);
    // Alpha has no open buffer now, but it has not been unused for long.
    ed.tick(Instant::now() + Duration::from_secs(60));
    assert_eq!(roots(&ed), [f.alpha.clone(), f.beta.clone()]);
    ed.tick(Instant::now() + Duration::from_secs(301));
    assert_eq!(roots(&ed), std::slice::from_ref(&f.beta));
    run(&mut ed, ":lsp-log\n");
    let log = ed.doc().text().to_string();
    assert!(log.contains("unused for 300 s"), "{log}");
    // Going back starts alpha's server again.
    switch_with_sidebar(&mut ed, &f.alpha);
    run(&mut ed, ":e src/lib.rs\n");
    let doc = ed.view().doc;
    poll_until(&mut ed, "alpha's server again", |ed| ed.lsp.is_synced(doc));
    assert!(roots(&ed).contains(&f.alpha));
    // Beta's last buffer closed with the switch; after the timeout its
    // server stops too, and alpha's (the current root) does not.
    ed.tick(Instant::now() + Duration::from_secs(700));
    assert_eq!(roots(&ed), std::slice::from_ref(&f.alpha));
}

fn a_server_with_an_open_buffer_is_never_stopped() {
    let f = Fixture::new();
    let mut config = lsp_config();
    config.lsp.max_live_roots = 1;
    config.projects.paths = vec![f.beta.display().to_string()];
    let mut ed = f.editor(config, false);
    ed.lsp.enabled = true;
    run(&mut ed, ":e src/lib.rs\n");
    let doc = ed.view().doc;
    poll_until(&mut ed, "alpha's server", |ed| ed.lsp.is_synced(doc));
    run(&mut ed, "ox<esc>");
    switch_with_sidebar(&mut ed, &f.beta);
    run(&mut ed, ":e src/lib.rs\n");
    let beta_doc = ed.view().doc;
    poll_until(&mut ed, "beta's server", |ed| ed.lsp.is_synced(beta_doc));
    // Alpha's lib.rs is open with unsaved changes: over the limit and
    // past the timeout, its server keeps running.
    ed.tick(Instant::now() + Duration::from_secs(1000));
    assert_eq!(roots(&ed), [f.alpha.clone(), f.beta.clone()]);
    assert!(ed.lsp.is_synced(doc));
}

fn servers_beyond_the_root_limit_stop_least_recently_used_first() {
    let f = Fixture::new();
    let mut config = lsp_config();
    config.lsp.max_live_roots = 1;
    config.lsp.stop_unused_after_secs = 0;
    let mut ed = two_roots(&f, config);
    // Without waiting: one root may have servers, and beta is current.
    ed.tick(Instant::now());
    assert_eq!(roots(&ed), std::slice::from_ref(&f.beta));
}

// ----- regressions from the review ------------------------------------------

/// `:cd` moves the working directory, not the session: a switch afterwards
/// saves the open files under the worktree they came from and never
/// replaces the `:cd` target's saved session.
fn a_switch_after_cd_saves_the_session_the_files_came_from() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), true);
    // Beta gets a saved session with beta.txt.
    switch_with_sidebar(&mut ed, &f.beta);
    run(&mut ed, ":e beta.txt\n");
    switch_with_sidebar(&mut ed, &f.alpha);
    run(&mut ed, ":e a.txt\n");
    run(&mut ed, &format!(":cd {}\n", f.beta.display()));
    assert_eq!(ed.cwd, f.beta);
    switch_with_sidebar(&mut ed, &f.feature);
    let beta = session::load(&f.state(), &f.beta).unwrap().unwrap();
    assert_eq!(beta.buffers.len(), 1);
    assert_eq!(
        beta.buffers[0].path,
        f.beta.join("beta.txt").to_str().unwrap()
    );
    let alpha = session::load(&f.state(), &f.alpha).unwrap().unwrap();
    assert_eq!(
        alpha.buffers[0].path,
        f.alpha.join("a.txt").to_str().unwrap()
    );
    // Back in alpha, then `:cd` away and choose alpha again: the same
    // session, only the directory comes back.
    switch_with_sidebar(&mut ed, &f.alpha);
    run(&mut ed, &format!(":cd {}\n", f.beta.display()));
    open_sidebar(&mut ed);
    select_row(&mut ed, &RowKey::Worktree(f.alpha.clone()));
    run(&mut ed, "\n");
    assert_eq!(ed.cwd, f.alpha);
    assert!(
        status_text(&ed).starts_with("back in alpha:main"),
        "{}",
        status_text(&ed)
    );
    assert_eq!(open_paths(&ed), [f.alpha.join("a.txt")]);
}

/// A worktree made after the last scan, nested in another: its files are
/// its own, not the outer worktree's, even before a scan lists it.
fn a_worktree_the_scan_has_not_listed_keeps_its_own_session() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), true);
    run(&mut ed, ":e a.txt\n");
    open_sidebar(&mut ed);
    run(&mut ed, "<esc>");
    let nested = f.alpha.join("nested");
    f.git(
        &f.alpha,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "nested",
            nested.to_str().unwrap(),
        ],
    );
    // Straight to the switch, as a stale palette or agent row would,
    // without letting the scan that lists `nested` finish.
    assert!(maestro_editor::workspaces::switch::switch(&mut ed, &nested));
    run(&mut ed, ":e b.txt\n");
    assert!(maestro_editor::workspaces::switch::switch(&mut ed, &f.beta));
    let alpha = session::load(&f.state(), &f.alpha).unwrap().unwrap();
    let saved: Vec<String> = alpha.buffers.iter().map(|b| b.path.clone()).collect();
    assert_eq!(saved, [f.alpha.join("a.txt").to_str().unwrap()]);
    let inner = session::load(&f.state(), &nested).unwrap().unwrap();
    assert_eq!(
        inner.buffers[0].path,
        nested.join("b.txt").to_str().unwrap()
    );
    settle(&mut ed);
}

fn a_path_through_dot_dot_belongs_to_the_worktree_it_names() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), true);
    run(&mut ed, ":e a.txt\n");
    run(&mut ed, ":e ../beta/beta.txt\n");
    switch_with_sidebar(&mut ed, &f.feature);
    // beta.txt is beta's: not saved in alpha's session, not closed.
    let alpha = session::load(&f.state(), &f.alpha).unwrap().unwrap();
    assert_eq!(alpha.buffers.len(), 1, "{alpha:?}");
    let open = open_paths(&ed);
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(open[0].ends_with("beta.txt"));
}

fn paste_and_keys_with_the_sidebar() {
    let f = Fixture::new();
    let mut ed = f.editor(f.config(), false);
    run(&mut ed, ":e a.txt\n");
    open_sidebar(&mut ed);
    // The selection starts on the current worktree.
    assert_eq!(
        ed.workspaces.sidebar.selected,
        Some(RowKey::Worktree(f.alpha.clone()))
    );
    ed.paste_text("pasted");
    assert!(!ed.doc().is_modified());
    assert!(status_text(&ed).contains("paste ignored"));
    // A sidebar the screen cannot show takes no keys.
    run(&mut ed, " s");
    ed.workspaces.sidebar.too_narrow = true;
    run(&mut ed, " s");
    assert!(
        status_text(&ed).contains("too narrow"),
        "{}",
        status_text(&ed)
    );
    run(&mut ed, "d");
    assert!(ed.doc().is_modified());
}

fn a_verification_that_cannot_start_needs_you() {
    let f = Fixture::new();
    let mut config = f.config();
    config
        .agents
        .provider
        .insert("quick".into(), sh_provider("true"));
    config.agents.test_command = vec![f.tmp.join("no-such-program").display().to_string()];
    let mut ed = f.editor(config, false);
    run(&mut ed, " a");
    start_task(&mut ed, "noop", 0);
    poll_until(&mut ed, "the agent", |ed| {
        ed.agents.agents.first().is_some_and(|a| a.exited.is_some())
    });
    run(&mut ed, "t");
    poll_until(&mut ed, "the test run", |ed| {
        ed.agents.agents[0]
            .latest_test()
            .is_some_and(|t| t.finished.is_some())
    });
    ed.poll_background();
    let e = &maestro_editor::workspaces::agent_entries(&ed)[0];
    assert_eq!(e.attention, Attention::NeedsYou(NeedsYou::TestsFailed));
    assert_eq!(ed.workspaces.notifications.len(), 2, "review, then tests");
}

fn the_palette_lists_a_bare_repositorys_worktrees_and_scratch_buffers() {
    let f = Fixture::new();
    let bare = f.tmp.join("bare.git");
    let linked = f.tmp.join("linked");
    f.git(
        &f.tmp,
        &[
            "clone",
            "-q",
            "--bare",
            f.alpha.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    f.git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "work",
            linked.to_str().unwrap(),
        ],
    );
    let mut config = f.config();
    config.projects.paths.push(linked.display().to_string());
    let mut ed = f.editor(config, false);
    run(&mut ed, "iscratch notes<esc>");
    run(&mut ed, " j");
    poll_until(&mut ed, "the palette", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading) && !ed.has_background_work()
    });
    let labels: Vec<String> = ed
        .picker
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|i| i.label.clone())
        .collect();
    for want in [
        "worktree bare.git:linked work",
        "project bare.git",
        "buffer [scratch] scratch notes",
    ] {
        assert!(labels.iter().any(|l| l == want), "{want} in {labels:?}");
    }
    run(&mut ed, "linked work\n");
    assert_eq!(ed.cwd, linked);
}

fn a_nested_worktrees_server_stops_when_you_leave_it() {
    let f = Fixture::new();
    let nested = f.alpha.join("nested");
    f.git(
        &f.alpha,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "nested",
            nested.to_str().unwrap(),
        ],
    );
    let mut config = lsp_config();
    config.lsp.max_live_roots = 0;
    let mut ed = f.editor(config, false);
    ed.lsp.enabled = true;
    switch_with_sidebar(&mut ed, &nested);
    run(&mut ed, ":e src/lib.rs\n");
    let doc = ed.view().doc;
    poll_until(&mut ed, "the nested server", |ed| ed.lsp.is_synced(doc));
    switch_with_sidebar(&mut ed, &f.alpha);
    assert_eq!(roots(&ed), std::slice::from_ref(&nested));
    ed.tick(Instant::now() + Duration::from_secs(301));
    assert_eq!(roots(&ed), Vec::<PathBuf>::new());
}

// ----- adding and forgetting projects --------------------------------------

/// A third repository `gamma` with a linked worktree `gamma-wt` on branch
/// `side`, a plain directory `plain` and a file `gamma-notes.txt`, none
/// of them listed anywhere.
fn gamma(f: &Fixture) -> (PathBuf, PathBuf) {
    let gamma = f.tmp.join("gamma");
    std::fs::create_dir_all(gamma.join("src")).unwrap();
    f.git(&gamma, &["init", "-q"]);
    std::fs::write(gamma.join("src/main.rs"), "fn main() {}\n").unwrap();
    f.git(&gamma, &["add", "-A"]);
    f.git(&gamma, &["commit", "-q", "-m", "initial"]);
    let wt = f.tmp.join("gamma-wt");
    f.git(
        &gamma,
        &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
    );
    std::fs::create_dir_all(f.tmp.join("plain")).unwrap();
    std::fs::write(f.tmp.join("gamma-notes.txt"), "not a directory\n").unwrap();
    (gamma, wt)
}

fn project_roots(ed: &Editor) -> Vec<PathBuf> {
    sidebar::rows(ed)
        .iter()
        .filter_map(|r| match r {
            Row::Project { root, .. } => Some(root.clone()),
            _ => None,
        })
        .collect()
}

fn list_file(f: &Fixture) -> PathBuf {
    manage::path(&f.state())
}

/// Waits until no background work runs and nothing it sent is left to
/// apply. A job sends its result before it stops counting as running, so
/// one more poll after it stops applies the result; that can start more
/// work (an add starts a scan), hence the loop.
fn drain(ed: &mut Editor) {
    loop {
        poll_until(ed, "background work", |ed| !ed.has_background_work());
        if !ed.poll_background() {
            return;
        }
    }
}

/// `a` in the sidebar, the prompt cleared, `path` typed and accepted;
/// waits for the check and the scan after it.
fn add_with_sidebar(ed: &mut Editor, path: &Path) {
    open_sidebar(ed);
    run(ed, "a");
    assert_eq!(
        ed.prompt.as_ref().map(|p| p.kind),
        Some(PromptKind::ProjectAdd)
    );
    run(ed, "<C-u>");
    run(ed, path.to_str().unwrap());
    run(ed, "\n");
    drain(ed);
}

fn adding_a_project_lists_it_selected_and_keeps_it() {
    let f = Fixture::new();
    let (gamma, wt) = gamma(&f);
    let mut ed = f.editor(f.config(), true);
    open_sidebar(&mut ed);
    assert!(!project_roots(&ed).contains(&gamma));
    // The prompt starts at the parent of the working directory.
    run(&mut ed, "a");
    let p = ed.prompt.as_ref().expect("the add prompt");
    assert_eq!(p.kind, PromptKind::ProjectAdd);
    assert!(p.input.ends_with('/'), "{:?}", p.input);
    assert_eq!(ed.resolve_path(Path::new(&p.input)), f.tmp);
    // A relative path into a subdirectory of a linked worktree: the
    // project is the repository, keyed by its main worktree.
    run(&mut ed, "<C-u>../gamma-wt/src\n");
    drain(&mut ed);
    assert_eq!(status_text(&ed), "added gamma");
    assert!(
        project_roots(&ed).contains(&gamma),
        "{:#?}",
        project_roots(&ed)
    );
    assert!(!project_roots(&ed).contains(&wt));
    assert_eq!(
        ed.workspaces.sidebar.selected,
        Some(RowKey::Project(gamma.clone()))
    );
    let rows = sidebar::rows(&ed);
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::Worktree { row, .. } if row.path == wt)),
        "gamma is unfolded: {rows:#?}"
    );
    assert_eq!(
        manage::read(&list_file(&f)),
        Ok(Some(ProjectList {
            added: vec![gamma.clone()],
            hidden: Vec::new(),
        }))
    );
    // Adding it again says so and changes nothing.
    add_with_sidebar(&mut ed, &gamma);
    assert_eq!(status_text(&ed), "gamma is already in the list");
    // The keys are named where you look for them.
    run(&mut ed, "z");
    assert!(
        status_text(&ed).contains("a add project, d forget project"),
        "{}",
        status_text(&ed)
    );

    // A new editor with the same state directory lists it.
    let mut again = f.editor(f.config(), true);
    open_sidebar(&mut again);
    assert!(project_roots(&again).contains(&gamma));
}

fn tab_completes_directories_in_the_add_prompt() {
    let f = Fixture::new();
    let (gamma, _) = gamma(&f);
    let mut ed = f.editor(f.config(), true);
    open_sidebar(&mut ed);
    run(&mut ed, "a<C-u>");
    run(&mut ed, &format!("{}/gam<tab>", f.tmp.display()));
    let input = |ed: &Editor| ed.prompt.as_ref().unwrap().input.clone();
    // Sorted as text: `-` comes before `/`.
    assert_eq!(input(&ed), format!("{}/gamma-wt/", f.tmp.display()));
    // Directories only: the file gamma-notes.txt is not offered.
    run(&mut ed, "<tab>");
    assert_eq!(input(&ed), format!("{}/gamma/", f.tmp.display()));
    run(&mut ed, "<tab>");
    assert_eq!(input(&ed), format!("{}/gamma-wt/", f.tmp.display()));
    run(&mut ed, "<S-tab>");
    assert_eq!(input(&ed), format!("{}/gamma/", f.tmp.display()));
    run(&mut ed, "\n");
    drain(&mut ed);
    assert_eq!(status_text(&ed), "added gamma");
    assert!(project_roots(&ed).contains(&gamma));
}

fn adding_refuses_missing_paths_and_directories_outside_git() {
    let f = Fixture::new();
    gamma(&f);
    let mut ed = f.editor(f.config(), true);
    open_sidebar(&mut ed);
    let before = project_roots(&ed);
    for (path, want) in [
        (f.tmp.join("nope"), "no such directory: "),
        (f.tmp.join("plain"), "not a Git repository: "),
        (f.tmp.join("gamma-notes.txt"), "not a directory: "),
    ] {
        add_with_sidebar(&mut ed, &path);
        assert_eq!(
            ed.status,
            Some((Severity::Error, format!("{want}{}", path.display())))
        );
    }
    assert_eq!(project_roots(&ed), before);
    assert!(!list_file(&f).exists(), "nothing was written");
}

fn forgetting_asks_first_and_refuses_configured_projects() {
    let f = Fixture::new();
    let (gamma, _) = gamma(&f);
    // Gamma is listed only because an earlier session was there.
    let session = session::SessionFile {
        version: 1,
        cwd: gamma.display().to_string(),
        focused: None,
        buffers: Vec::new(),
        layout: None,
        focused_view: None,
    };
    let file = session::session_path(&f.state(), &gamma);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, toml::to_string(&session).unwrap()).unwrap();
    let mut ed = f.editor(f.config(), true);
    ed.workspaces.config_path = Some(f.tmp.join("home/config.toml"));
    open_sidebar(&mut ed);
    assert!(project_roots(&ed).contains(&gamma));

    // On a worktree row, d says what it is for.
    select_row(&mut ed, &RowKey::Worktree(f.alpha.clone()));
    run(&mut ed, "d");
    assert!(status_text(&ed).starts_with("d forgets a project"));
    assert_eq!(ed.pending_char_label(), None);

    select_row(&mut ed, &RowKey::Project(gamma.clone()));
    run(&mut ed, "d");
    assert_eq!(ed.pending_char_label(), Some("y/n"));
    assert_eq!(
        status_text(&ed),
        "forget gamma? It only leaves this list; no files are deleted"
    );
    run(&mut ed, "n");
    assert_eq!(status_text(&ed), "kept gamma");
    assert!(project_roots(&ed).contains(&gamma));

    run(&mut ed, "dy");
    assert!(status_text(&ed).starts_with("forgot gamma; no files were touched"));
    assert!(!project_roots(&ed).contains(&gamma), "gone at once");
    drain(&mut ed);
    assert!(!project_roots(&ed).contains(&gamma), "and after a scan");
    assert!(gamma.join("src/main.rs").exists());
    assert_eq!(
        manage::read(&list_file(&f)),
        Ok(Some(ProjectList {
            added: Vec::new(),
            hidden: vec![gamma.clone()],
        }))
    );

    // The session is still there, but a new editor leaves gamma out until
    // it is added again.
    let mut again = f.editor(f.config(), true);
    open_sidebar(&mut again);
    assert!(!project_roots(&again).contains(&gamma));
    add_with_sidebar(&mut again, &gamma);
    assert_eq!(status_text(&again), "added gamma");
    assert_eq!(
        manage::read(&list_file(&f)).unwrap().unwrap().hidden,
        Vec::<PathBuf>::new()
    );

    // A configured project is refused with where to remove it.
    select_row(&mut ed, &RowKey::Project(f.beta.clone()));
    run(&mut ed, "d");
    assert_eq!(ed.pending_char_label(), None);
    assert_eq!(
        status_text(&ed),
        format!(
            "beta is listed in [projects] paths in {}; remove it there",
            f.tmp.join("home/config.toml").display()
        )
    );
    // So is the project the editor is in.
    select_row(&mut ed, &RowKey::Project(f.alpha.clone()));
    run(&mut ed, "d");
    assert_eq!(ed.pending_char_label(), None);
    assert!(status_text(&ed).contains("the project you are in"));
    assert!(project_roots(&ed).contains(&f.alpha));
}

fn typed_commands_and_the_palette_add_and_forget_projects() {
    let f = Fixture::new();
    let (gamma, _) = gamma(&f);
    let mut ed = f.editor(f.config(), true);
    run(&mut ed, &format!(":project-add {}\n", gamma.display()));
    drain(&mut ed);
    assert_eq!(status_text(&ed), "added gamma; space s shows it");
    // With no path it asks for one.
    run(&mut ed, ":project-add\n");
    assert_eq!(
        ed.prompt.as_ref().map(|p| p.kind),
        Some(PromptKind::ProjectAdd)
    );
    run(&mut ed, "<esc>");
    // The palette's last entry asks too.
    run(&mut ed, " j");
    poll_until(&mut ed, "the palette", |ed| {
        ed.picker.as_ref().is_some_and(|p| !p.loading) && !ed.has_background_work()
    });
    let last = ed.picker.as_ref().unwrap().items().last().cloned().unwrap();
    assert_eq!(last.label, "add a project");
    run(&mut ed, "add a project\n");
    assert!(ed.picker.is_none());
    assert_eq!(
        ed.prompt.as_ref().map(|p| p.kind),
        Some(PromptKind::ProjectAdd)
    );
    run(&mut ed, "<esc>");

    run(&mut ed, ":project-remove\n");
    assert!(status_text(&ed).starts_with("give a project name or path"));
    run(&mut ed, ":project-remove nothing-here\n");
    assert!(status_text(&ed).starts_with("no project named nothing-here"));
    // By path, from inside it.
    run(
        &mut ed,
        &format!(":project-remove {}/src\n", gamma.display()),
    );
    assert_eq!(ed.pending_char_label(), Some("y/n"));
    run(&mut ed, "y");
    drain(&mut ed);
    assert!(
        !ed.workspaces.projects.iter().any(|p| p.root == gamma),
        "{:?}",
        ed.workspaces.projects
    );
    // By name: beta is configured.
    run(&mut ed, ":project-remove beta\n");
    assert!(status_text(&ed).contains("[projects] paths in the config file"));
}

fn without_a_state_directory_the_list_is_neither_read_nor_written() {
    let f = Fixture::new();
    let (gamma, wt) = gamma(&f);
    // A list on disk names gamma; an editor without a state directory
    // (--clean) does not read it.
    let text = format!("version = 1\nadded = [\"{}\"]\n", gamma.display());
    std::fs::create_dir_all(f.state()).unwrap();
    std::fs::write(list_file(&f), &text).unwrap();
    let mut ed = f.editor(f.config(), false);
    open_sidebar(&mut ed);
    assert!(!project_roots(&ed).contains(&gamma));

    add_with_sidebar(&mut ed, &wt);
    assert_eq!(
        status_text(&ed),
        "added gamma (for this run only: no state directory)"
    );
    assert!(project_roots(&ed).contains(&gamma));
    select_row(&mut ed, &RowKey::Project(gamma.clone()));
    run(&mut ed, "dy");
    drain(&mut ed);
    assert!(!project_roots(&ed).contains(&gamma));
    // Nothing was written.
    assert_eq!(std::fs::read_to_string(list_file(&f)).unwrap(), text);
    let names: Vec<_> = std::fs::read_dir(f.state())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [manage::FILE]);
}

fn a_malformed_project_list_is_ignored_with_a_note() {
    let f = Fixture::new();
    let (gamma, _) = gamma(&f);
    std::fs::create_dir_all(f.state()).unwrap();
    std::fs::write(list_file(&f), "version = 1\nadded = [[[\n").unwrap();
    let mut ed = f.editor(f.config(), true);
    open_sidebar(&mut ed);
    let note = status_text(&ed);
    assert!(
        note.starts_with(&format!("ignored {}: ", list_file(&f).display())),
        "{note}"
    );
    assert!(note.ends_with("adding or forgetting a project replaces it"));
    assert!(project_roots(&ed).contains(&f.alpha));
    assert!(project_roots(&ed).contains(&f.beta));
    // The next change replaces it with a good file.
    add_with_sidebar(&mut ed, &gamma);
    assert_eq!(status_text(&ed), "added gamma");
    assert_eq!(
        manage::read(&list_file(&f)),
        Ok(Some(ProjectList {
            added: vec![gamma],
            hidden: Vec::new(),
        }))
    );
}

fn adding_before_the_first_scan_selects_the_project_when_it_arrives() {
    let f = Fixture::new();
    let (gamma, _) = gamma(&f);
    let mut ed = f.editor(f.config(), true);
    // `space s` starts the first scan; the path is typed before it ends.
    run(&mut ed, " s");
    assert!(!ed.workspaces.scanned);
    run(&mut ed, "a<C-u>");
    run(&mut ed, gamma.to_str().unwrap());
    run(&mut ed, "\n");
    drain(&mut ed);
    assert!(ed.workspaces.scanned);
    assert!(project_roots(&ed).contains(&gamma));
    assert_eq!(
        ed.workspaces.sidebar.selected,
        Some(RowKey::Project(gamma.clone()))
    );
    assert!(ed.workspaces.sidebar.expanded.contains(&gamma));
    // A path through a directory that does not exist is refused, even
    // when `..` would lead back to a real one.
    let detour = gamma.join("no-such-dir/..");
    add_with_sidebar(&mut ed, &detour);
    assert_eq!(
        ed.status,
        Some((
            Severity::Error,
            format!("no such directory: {}", detour.display())
        ))
    );
}

fn an_added_worktree_of_a_bare_repository_stays_usable() {
    let f = Fixture::new();
    let bare = f.tmp.join("bare.git");
    let linked = f.tmp.join("linked");
    f.git(
        &f.tmp,
        &[
            "clone",
            "-q",
            "--bare",
            f.alpha.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    f.git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "work",
            linked.to_str().unwrap(),
        ],
    );
    let mut ed = f.editor(f.config(), true);
    add_with_sidebar(&mut ed, &linked);
    assert_eq!(status_text(&ed), "added bare.git");
    // A bare directory is not a usable project: the linked worktree is
    // what is kept, and later scans still list its worktree.
    assert_eq!(
        manage::read(&list_file(&f)).unwrap().unwrap().added,
        std::slice::from_ref(&linked)
    );
    let mut again = f.editor(f.config(), true);
    open_sidebar(&mut again);
    let project = again
        .workspaces
        .projects
        .iter()
        .find(|p| p.root == bare)
        .unwrap_or_else(|| panic!("{:#?}", again.workspaces.projects));
    assert_eq!(project.error, None);
    assert!(project.worktrees.iter().any(|w| w.path == linked));
    // Forgetting it drops the linked worktree's entry too.
    select_row(&mut again, &RowKey::Project(bare.clone()));
    run(&mut again, "dy");
    drain(&mut again);
    assert_eq!(
        manage::read(&list_file(&f)),
        Ok(Some(ProjectList {
            added: Vec::new(),
            hidden: vec![bare.clone()],
        }))
    );
    assert!(!project_roots(&again).contains(&bare));
}
