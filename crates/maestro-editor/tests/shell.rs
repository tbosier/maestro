//! Shell pane behavior tests: keys in through `Editor::handle_key`, a real
//! `sh` in a terminal, real processes. Every wait polls with a deadline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use maestro_config::Config;
use maestro_core::Document;
use maestro_editor::{Editor, KeyCode, KeyEvent, MouseKind, Rect};

const WAIT: Duration = Duration::from_secs(20);

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A scratch directory with two subdirectories standing in for worktrees.
struct Fixture {
    tmp: PathBuf,
    one: PathBuf,
    two: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("maestro-shell-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("one")).unwrap();
        std::fs::create_dir_all(tmp.join("two")).unwrap();
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        Fixture {
            one: tmp.join("one"),
            two: tmp.join("two"),
            tmp,
        }
    }

    /// An editor in `one` with a two-line buffer, its shell set to `sh`.
    fn editor(&self) -> Editor {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 1, 120, 40));
        ed.cwd = self.one.clone();
        ed.shell.program = Some("sh".into());
        let doc = ed.add_document(Document::from_text("first\nsecond\n"));
        ed.switch_to(doc);
        ed
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
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

/// Types text with no key names in it (`<` is a plain character here).
fn type_text(ed: &mut Editor, text: &str) {
    for c in text.chars() {
        let key = if c == '\n' {
            KeyEvent::plain(KeyCode::Enter)
        } else {
            KeyEvent::char(c)
        };
        ed.handle_key(key);
    }
}

/// Polls background work until `done` holds, as the terminal loop does.
fn poll_until(ed: &mut Editor, what: &str, mut done: impl FnMut(&Editor) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        ed.poll_background();
        if done(ed) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status {:?}; screen:\n{}",
            ed.status,
            screen_text(ed)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn screen_text(ed: &Editor) -> String {
    let Some(s) = ed.shell.session() else {
        return "(no shell)".into();
    };
    let screen = s.terminal.snapshot();
    (0..screen.rows)
        .map(|r| screen.row_text(r))
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when a screen line is exactly `line`: output, not the echo of the
/// command that printed it.
fn shows_line(ed: &Editor, line: &str) -> bool {
    screen_text(ed).lines().any(|l| l.trim_end() == line)
}

/// Waits for the shell's first prompt, so typed keys meet a shell that
/// reads them.
fn wait_for_prompt(ed: &mut Editor) {
    poll_until(ed, "the shell's prompt", |ed| {
        !screen_text(ed).trim().is_empty() && ed.shell.running()
    });
}

/// The number printed right after `prefix` and ending its line. A job's
/// output can land after the prompt on the same line; the echo of the
/// command (`prefix$$`) has no number there.
fn number_after(ed: &Editor, prefix: &str) -> Option<u32> {
    screen_text(ed).lines().find_map(|l| {
        let l = l.trim_end();
        let at = l.rfind(prefix)?;
        l[at + prefix.len()..].parse().ok()
    })
}

fn wait_for_file(ed: &mut Editor, path: &Path) -> String {
    let mut text = String::new();
    poll_until(ed, &format!("{}", path.display()), |_| {
        text = std::fs::read_to_string(path).unwrap_or_default();
        text.ends_with('\n')
    });
    text
}

/// True while process `pid` exists and is not a zombie.
fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

fn wait_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid}) still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cursor_line(ed: &Editor) -> usize {
    let text = ed.doc().text().slice(..);
    text.char_to_line(ed.view().selection.primary().head)
}

#[test]
fn space_o_runs_a_shell_in_the_working_directory_with_the_keys() {
    let f = Fixture::new();
    let mut ed = f.editor();
    let views_before = ed.area;
    run(&mut ed, " o");
    assert!(ed.shell.open);
    assert!(ed.shell.has_focus(), "status {:?}", ed.status);
    // The pane takes its rows from the bottom of the views.
    let pane = ed.shell.area().expect("the pane is shown");
    assert_eq!(pane.bottom(), views_before.bottom());
    assert_eq!(ed.area.height, views_before.height - pane.height);
    let msg = ed.status.clone().unwrap().1;
    assert!(
        msg.contains("typing into the shell") && msg.contains("C-\\"),
        "{msg}"
    );
    wait_for_prompt(&mut ed);

    type_text(&mut ed, "printf 'hi-from-%s\\n' shell\n");
    poll_until(&mut ed, "the shell's output", |ed| {
        shows_line(ed, "hi-from-shell")
    });
    type_text(&mut ed, "pwd > where.txt\n");
    let pwd = wait_for_file(&mut ed, &f.one.join("where.txt"));
    assert_eq!(pwd.trim_end(), f.one.to_str().unwrap());
    // None of it reached the buffer.
    assert_eq!(ed.doc().text().to_string(), "first\nsecond\n");
    assert_eq!(cursor_line(&ed), 0);

    // A paste goes to the shell too.
    ed.paste_text("printf 'pasted-%s\\n' text");
    type_text(&mut ed, "\n");
    poll_until(&mut ed, "the pasted command's output", |ed| {
        shows_line(ed, "pasted-text")
    });
    assert_eq!(ed.doc().text().to_string(), "first\nsecond\n");
}

#[test]
fn ctrl_backslash_returns_the_keys_and_toggling_hides_and_shows_the_same_shell() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    type_text(&mut ed, "printf 'mark-%s\\n' one\n");
    poll_until(&mut ed, "the first output", |ed| shows_line(ed, "mark-one"));
    let first = Arc::clone(&ed.shell.session().unwrap().terminal);

    // C-\ (crossterm reports it as C-4 too) gives the keys back; the
    // pane stays.
    ed.handle_key(KeyEvent::ctrl('4'));
    assert!(!ed.shell.has_focus());
    assert!(ed.shell.area().is_some());
    // `j` moves the editor's cursor and reaches no shell.
    run(&mut ed, "j");
    assert_eq!(cursor_line(&ed), 1);
    // C-\ from the editor gives the keys to the shown shell again. Had the
    // `j` reached the shell, this line would run `jprintf` and fail.
    ed.handle_key(KeyEvent::ctrl('\\'));
    assert!(ed.shell.has_focus());
    type_text(&mut ed, "printf 'back-%s\\n' ok\n");
    poll_until(&mut ed, "the output after the keys came back", |ed| {
        shows_line(ed, "back-ok")
    });
    assert!(!screen_text(&ed).contains("jprintf"));

    // `space o` from the editor hides the pane; the shell keeps running
    // and the views get their rows back.
    ed.handle_key(KeyEvent::ctrl('\\'));
    run(&mut ed, " o");
    assert!(!ed.shell.open);
    assert_eq!(ed.shell.area(), None);
    assert_eq!(ed.area, ed.region());
    assert!(ed.shell.running());
    // Hidden, C-\ gives nothing the keys and `j` still moves the cursor.
    ed.handle_key(KeyEvent::ctrl('\\'));
    assert!(!ed.shell.has_focus());
    run(&mut ed, "k");
    assert_eq!(cursor_line(&ed), 0);

    // `:terminal` (the alias of `:term`) shows the same shell with the
    // keys: the earlier output is still on its screen.
    run(&mut ed, ":terminal\n");
    assert!(ed.shell.has_focus());
    let again = Arc::clone(&ed.shell.session().unwrap().terminal);
    assert!(Arc::ptr_eq(&first, &again), "a new shell was started");
    assert!(shows_line(&ed, "mark-one"), "{}", screen_text(&ed));
    type_text(&mut ed, "printf 'still-%s\\n' here\n");
    poll_until(&mut ed, "output in the same shell", |ed| {
        shows_line(ed, "still-here")
    });

    // The toggle while the shell has the keys hides the pane too (the
    // command palette or the automation API can run it).
    ed.run_command("toggle_shell");
    assert!(!ed.shell.open && !ed.shell.has_focus());
    assert!(ed.shell.running());
}

#[test]
fn a_click_in_the_pane_gives_it_the_keys_and_a_click_outside_takes_them() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    let pane = ed.shell.area().unwrap();
    // A click in the views takes the keys back.
    ed.handle_mouse(MouseKind::Down, ed.area.x + 10, ed.area.y + 1);
    assert!(!ed.shell.has_focus());
    run(&mut ed, "j");
    assert_eq!(cursor_line(&ed), 1);
    // A half-typed key sequence does not survive a click into the shell.
    run(&mut ed, "g");
    assert!(!ed.pending.is_empty());
    ed.handle_mouse(MouseKind::Down, pane.x + 3, pane.y + 2);
    assert!(ed.shell.has_focus());
    assert!(ed.pending.is_empty(), "{:?}", ed.pending);
    type_text(&mut ed, "printf 'clicked-%s\\n' in\n");
    poll_until(&mut ed, "the output after the click", |ed| {
        shows_line(ed, "clicked-in")
    });
    assert_eq!(cursor_line(&ed), 1);

    // The wheel over the pane scrolls the shell's scrollback.
    type_text(
        &mut ed,
        "i=0; while [ $i -lt 120 ]; do echo line-$i; i=$((i+1)); done\n",
    );
    poll_until(&mut ed, "the long output", |ed| shows_line(ed, "line-119"));
    ed.handle_mouse(MouseKind::ScrollUp, pane.x + 3, pane.y + 2);
    assert!(ed.shell.session().unwrap().terminal.snapshot().scrolled);
    ed.handle_mouse(MouseKind::ScrollDown, pane.x + 3, pane.y + 2);
    assert!(!ed.shell.session().unwrap().terminal.snapshot().scrolled);
}

#[test]
fn exit_reports_the_status_and_the_next_toggle_starts_a_new_shell_where_the_editor_is() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    let first = Arc::clone(&ed.shell.session().unwrap().terminal);
    type_text(&mut ed, "exit 3\n");
    poll_until(&mut ed, "the shell's exit", |ed| {
        ed.shell.session().is_some_and(|s| s.exited.is_some())
    });
    assert!(!ed.shell.focused && !ed.shell.has_focus());
    let msg = ed.status.clone().unwrap().1;
    assert!(
        msg.contains("exited with code 3") && msg.contains("keys are back"),
        "{msg}"
    );
    // The pane stays and says how the shell ended.
    assert!(ed.shell.area().is_some());
    let header = maestro_editor::shell::header(&ed);
    assert!(
        header.badges[0].contains("exited with code 3"),
        "{header:?}"
    );
    // Keys are the editor's.
    run(&mut ed, "j");
    assert_eq!(cursor_line(&ed), 1);

    // The editor moves to another worktree; the next toggle starts a new
    // shell there.
    assert!(maestro_editor::workspaces::switch::switch(&mut ed, &f.two));
    assert_eq!(ed.cwd, f.two);
    run(&mut ed, " o");
    assert!(ed.shell.has_focus(), "status {:?}", ed.status);
    let second = Arc::clone(&ed.shell.session().unwrap().terminal);
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(ed.shell.session().unwrap().dir, f.two);
    wait_for_prompt(&mut ed);
    type_text(&mut ed, "pwd > where.txt\n");
    let pwd = wait_for_file(&mut ed, &f.two.join("where.txt"));
    assert_eq!(pwd.trim_end(), f.two.to_str().unwrap());
}

#[test]
fn a_running_shell_stays_in_its_directory_when_the_editor_switches_worktrees() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    let first = Arc::clone(&ed.shell.session().unwrap().terminal);
    ed.handle_key(KeyEvent::ctrl('\\'));
    assert!(maestro_editor::workspaces::switch::switch(&mut ed, &f.two));
    assert_eq!(ed.cwd, f.two);
    ed.poll_background();
    let s = ed.shell.session().unwrap();
    assert!(Arc::ptr_eq(&first, &s.terminal));
    assert_eq!(s.dir, f.one, "the header names where the shell started");
    assert_eq!(
        maestro_editor::shell::header(&ed).dir,
        maestro_editor::navigation::home_path(&f.one)
    );
    // Still the same shell, still in `one`.
    ed.handle_key(KeyEvent::ctrl('\\'));
    type_text(&mut ed, "pwd > where.txt\n");
    let pwd = wait_for_file(&mut ed, &f.one.join("where.txt"));
    assert_eq!(pwd.trim_end(), f.one.to_str().unwrap());
    assert!(!f.two.join("where.txt").exists());
}

#[test]
fn quitting_ends_the_shell_and_the_program_it_runs() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    type_text(&mut ed, "echo shell-pid-$$\n");
    poll_until(&mut ed, "the shell's pid", |ed| {
        number_after(ed, "shell-pid-").is_some()
    });
    let shell = number_after(&ed, "shell-pid-").unwrap();
    // A program in the foreground, as when quitting mid-command: `exec`
    // keeps the pid it printed.
    type_text(&mut ed, "sh -c 'echo child-pid-$$; exec sleep 300'\n");
    poll_until(&mut ed, "the child's pid", |ed| {
        number_after(ed, "child-pid-").is_some()
    });
    let child = number_after(&ed, "child-pid-").unwrap();
    assert!(alive(shell) && alive(child));

    ed.handle_key(KeyEvent::ctrl('\\'));
    let started = Instant::now();
    run(&mut ed, ":qa\n");
    assert!(ed.should_quit, "status {:?}", ed.status);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "quitting took {:?}",
        started.elapsed()
    );
    // Quitting waits for the shell to end before it returns.
    assert!(
        !ed.shell.running(),
        "the shell still runs when quit returns"
    );
    wait_gone(shell, "the shell");
    wait_gone(child, "the program in the shell");
    drop(ed);
}

#[test]
fn dropping_the_editor_ends_the_shell() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    type_text(&mut ed, "echo shell-pid-$$\n");
    poll_until(&mut ed, "the shell's pid", |ed| {
        number_after(ed, "shell-pid-").is_some()
    });
    let shell = number_after(&ed, "shell-pid-").unwrap();
    // Hidden, still running.
    ed.handle_key(KeyEvent::ctrl('\\'));
    run(&mut ed, " o");
    assert!(ed.shell.running() && !ed.shell.open);
    let started = Instant::now();
    drop(ed);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "dropping took {:?}",
        started.elapsed()
    );
    // Gone when drop returns, not just later: the front end's process
    // ends right after, and the SIGKILL would end with it.
    assert!(!alive(shell), "the shell (pid {shell}) runs after drop");
}

/// Kills `pid` when dropped, so a failing test leaves nothing running.
struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if alive(self.0) {
            let _ = Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .status();
        }
    }
}

/// An interactive shell puts a background job in a process group of its
/// own, which a signal to the shell's group does not reach. A job that
/// ignores SIGHUP must still end when the editor quits.
#[test]
fn quitting_ends_a_background_job_that_ignores_hangup() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    // The job prints its pid only once it ignores SIGHUP, and `exec` keeps
    // that pid.
    type_text(
        &mut ed,
        "sh -c 'trap \"\" HUP; echo job-pid-$$; exec sleep 300' &\n",
    );
    poll_until(&mut ed, "the job's pid", |ed| {
        number_after(ed, "job-pid-").is_some()
    });
    let job = number_after(&ed, "job-pid-").unwrap();
    let _guard = KillOnDrop(job);
    assert!(alive(job));
    ed.handle_key(KeyEvent::ctrl('\\'));
    run(&mut ed, ":qa\n");
    assert!(ed.should_quit, "status {:?}", ed.status);
    // The shell's exit is reported only after its session got SIGHUP and
    // SIGKILL, so every signal was sent before quitting returned; what is
    // left is the kernel delivering SIGKILL.
    assert!(
        ed.shell.session().unwrap().terminal.exit_status().is_some(),
        "quitting returned before the shell ended"
    );
    drop(ed);
    wait_gone(job, "the background job");
}

#[test]
fn keys_typed_into_the_shell_are_not_repeated_by_dot() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    ed.handle_key(KeyEvent::ctrl('\\'));
    // An insert session, then C-\ to the shell in the middle of it.
    run(&mut ed, "iab");
    ed.handle_key(KeyEvent::ctrl('\\'));
    assert!(ed.shell.has_focus());
    assert_eq!(ed.mode, maestro_editor::Mode::Normal, "insert mode ended");
    type_text(&mut ed, "printf 'once-%s\\n' only\n");
    poll_until(&mut ed, "the command's output", |ed| {
        shows_line(ed, "once-only")
    });
    ed.handle_key(KeyEvent::ctrl('\\'));
    // `.` repeats the insert alone: no C-\ and nothing typed into the shell.
    let toggle = |k: &KeyEvent| k.mods.ctrl && k.code == KeyCode::Char('\\');
    assert!(!ed.last_change.iter().any(toggle), "{:?}", ed.last_change);
    assert!(
        !ed.last_change.contains(&KeyEvent::char('p')),
        "{:?}",
        ed.last_change
    );
    run(&mut ed, "j0.");
    assert!(!ed.shell.has_focus());
    assert_eq!(ed.doc().text().to_string(), "abfirst\nabsecond\n");
}

#[test]
fn a_pane_that_loses_its_room_gives_the_keys_back_for_good() {
    let f = Fixture::new();
    let mut ed = f.editor();
    run(&mut ed, " o");
    wait_for_prompt(&mut ed);
    let region = ed.region();
    // Too short for the views and the pane: the keys come back.
    ed.resize(Rect::new(region.x, region.y, region.width, 10));
    assert!(ed.shell.area().is_none());
    assert!(!ed.shell.has_focus());
    // `r` waits for its character in the editor.
    run(&mut ed, "r");
    // Grown again, the pane shows but the keys stay with the editor, so
    // the `r` gets its character here, not in the shell.
    ed.resize(region);
    assert!(ed.shell.area().is_some());
    assert!(!ed.shell.has_focus());
    run(&mut ed, "X");
    assert_eq!(ed.doc().text().to_string(), "Xirst\nsecond\n");
}
