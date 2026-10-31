//! The agent panel: a full-screen overlay over the editor area with the
//! agent list on the left and the selected agent on the right. Keys go to
//! the panel while it is open; `C-\` hands them to the selected agent's
//! terminal and back.

use maestro_git::Integration;

use super::create::NewAgent;
use super::review::ReviewView;
use super::{AgentId, ProviderKind, ProviderSpec};
use crate::editor::{Editor, Mode};
use crate::key::{KeyCode, KeyEvent};
use crate::layout::Rect;

/// What the right side shows for the selected agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailView {
    /// The terminal, or the ACP transcript with any pending permission.
    #[default]
    Live,
    /// Timestamped events.
    History,
    /// The latest verification run's output and summary.
    Tests,
}

impl DetailView {
    pub fn label(self) -> &'static str {
        match self {
            DetailView::Live => "live",
            DetailView::History => "history",
            DetailView::Tests => "tests",
        }
    }

    fn next(self) -> Self {
        match self {
            DetailView::Live => DetailView::History,
            DetailView::History => DetailView::Tests,
            DetailView::Tests => DetailView::Live,
        }
    }
}

/// Text the panel is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPurpose {
    NewTask,
    Feedback(AgentId),
    Comment {
        agent: AgentId,
        path: std::path::PathBuf,
        line: usize,
        old: bool,
    },
}

/// A list to pick one item from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoicePurpose {
    Provider {
        task: String,
        group: Option<u64>,
        base: Option<String>,
        providers: Vec<ProviderSpec>,
    },
    Permission(AgentId),
}

/// What a confirmation dialog does when accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// `y` rejects, `a` rejects and archives the worktree.
    Reject(AgentId),
    /// `y` integrates with `how`; `tab` changes it.
    Integrate {
        agent: AgentId,
        how: Integration,
        facts: super::actions::IntegrateFacts,
    },
    /// `y` removes the worktree (refused if it has work in it).
    Archive(AgentId),
    /// `F` force-removes the worktree, deleting what `lines` lists.
    ForceArchive(AgentId),
    /// `y` sends the permission answer.
    Grant {
        agent: AgentId,
        request_id: serde_json::Value,
        option: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub title: String,
    /// What will happen, in plain words.
    pub lines: Vec<String>,
    /// Keys and what they do, for the dialog's last line.
    pub keys: Vec<(String, String)>,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Input {
        purpose: InputPurpose,
        label: String,
        text: String,
    },
    Choice {
        purpose: ChoicePurpose,
        title: String,
        /// Label and detail per item.
        items: Vec<(String, String)>,
        selected: usize,
    },
    Confirm(Confirm),
}

#[derive(Debug, Default)]
pub struct Panel {
    pub open: bool,
    /// Index into the agent list.
    pub selected: usize,
    /// `Some(agent)` while that agent's terminal has the keys.
    pub terminal_focus: Option<AgentId>,
    pub view: DetailView,
    /// The review screen, when open.
    pub review: Option<ReviewView>,
    pub overlay: Option<Overlay>,
    /// Lines scrolled back from the bottom of a transcript or log.
    pub scroll: usize,
}

/// Where the panel's parts go inside the editor area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelLayout {
    /// Title and who has the keys.
    pub header: Rect,
    /// The agent list (or the review's file list).
    pub list: Rect,
    /// The column between list and detail.
    pub divider: Rect,
    /// Facts about the selected agent.
    pub summary: Rect,
    /// The terminal, transcript, log or diff. Terminals are sized to it.
    pub content: Rect,
    /// Key hints.
    pub hints: Rect,
}

/// Rows of facts above the content.
pub const SUMMARY_ROWS: u16 = 4;

pub fn layout(area: Rect) -> PanelLayout {
    let header = Rect::new(area.x, area.y, area.width, 1.min(area.height));
    let hints_y = area.bottom().saturating_sub(1).max(header.bottom());
    let hints = Rect::new(area.x, hints_y, area.width, u16::from(area.height >= 2));
    let body_y = header.bottom();
    let body_h = hints.y.saturating_sub(body_y);
    let list_w = if area.width >= 90 {
        (area.width * 3 / 10).clamp(28, 48)
    } else {
        area.width / 3
    };
    let list = Rect::new(area.x, body_y, list_w, body_h);
    let divider = Rect::new(list.right(), body_y, u16::from(area.width > list_w), body_h);
    let dx = divider.right();
    let dw = area.right().saturating_sub(dx);
    let summary_h = SUMMARY_ROWS.min(body_h);
    let summary = Rect::new(dx, body_y, dw, summary_h);
    let content = Rect::new(dx, body_y + summary_h, dw, body_h - summary_h);
    PanelLayout {
        header,
        list,
        divider,
        summary,
        content,
        hints,
    }
}

/// The size agent terminals get: the panel's content area.
pub fn terminal_size(area: Rect) -> maestro_term::TermSize {
    let c = layout(area).content;
    maestro_term::TermSize {
        rows: c.height.max(1),
        cols: c.width.max(2),
    }
}

pub fn open(ed: &mut Editor) {
    if ed.mode == Mode::Insert {
        ed.run_command("normal_mode");
    }
    ed.agents.panel.open = true;
    let n = ed.agents.agents.len();
    if n > 0 {
        ed.agents.panel.selected = ed.agents.panel.selected.min(n - 1);
    }
    let ids: Vec<AgentId> = ed
        .agents
        .agents
        .iter()
        .filter(|a| a.worktree.is_some() && !a.archived)
        .map(|a| a.id)
        .collect();
    for id in ids {
        super::review::request(ed, id);
    }
    if n == 0 {
        ed.info("no agents yet: n starts a task");
    }
}

pub fn close(ed: &mut Editor) {
    let p = &mut ed.agents.panel;
    p.open = false;
    p.terminal_focus = None;
    p.overlay = None;
}

/// True when `key` is `C-\`. Terminals send it as the byte 0x1c, which
/// crossterm reports as `C-4`, so both count.
pub fn is_toggle(key: KeyEvent) -> bool {
    key.mods.ctrl && !key.mods.alt && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

/// Handles a key while the panel is open.
pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    if let Some(id) = ed.agents.panel.terminal_focus {
        if is_toggle(key) {
            ed.agents.panel.terminal_focus = None;
            ed.info("keys are back with the panel");
            return;
        }
        send_key_to_terminal(ed, id, key);
        return;
    }
    if ed.agents.panel.overlay.is_some() {
        overlay_key(ed, key);
        return;
    }
    if ed.agents.panel.review.is_some() {
        super::review::handle_key(ed, key);
        return;
    }
    list_key(ed, key);
}

/// Pasted text while the panel is open: into the terminal that has the
/// keys, else into an open input line. Dropped otherwise.
pub fn paste(ed: &mut Editor, text: &str) {
    if let Some(id) = ed.agents.panel.terminal_focus {
        if let Some(t) = ed.agents.get(id).and_then(|a| a.runtime.terminal()) {
            let bytes = maestro_term::encode_paste(text, t.bracketed_paste());
            if let Err(e) = t.write(&bytes) {
                ed.error(format!("could not paste into the terminal: {e}"));
            }
        }
        return;
    }
    if let Some(Overlay::Input { text: t, .. }) = &mut ed.agents.panel.overlay {
        t.push_str(&text.replace("\r\n", "\n"));
    }
}

fn send_key_to_terminal(ed: &mut Editor, id: AgentId, key: KeyEvent) {
    let Some(t) = ed
        .agents
        .get(id)
        .and_then(|a| a.runtime.terminal())
        .cloned()
    else {
        ed.agents.panel.terminal_focus = None;
        return;
    };
    let Some(tk) = term_key(key.code) else {
        return;
    };
    let mods = maestro_term::TermMods {
        ctrl: key.mods.ctrl,
        alt: key.mods.alt,
        shift: key.mods.shift,
    };
    let bytes = maestro_term::encode_key(tk, mods, t.application_cursor());
    if let Err(e) = t.write(&bytes) {
        ed.agents.panel.terminal_focus = None;
        ed.error(format!(
            "the agent's terminal does not take input ({e}); keys are back with the panel"
        ));
    }
}

fn term_key(code: KeyCode) -> Option<maestro_term::TermKey> {
    use maestro_term::TermKey as T;
    Some(match code {
        KeyCode::Char(c) => T::Char(c),
        KeyCode::Enter => T::Enter,
        KeyCode::Esc => T::Esc,
        KeyCode::Backspace => T::Backspace,
        KeyCode::Delete => T::Delete,
        KeyCode::Tab => T::Tab,
        KeyCode::BackTab => T::BackTab,
        KeyCode::Left => T::Left,
        KeyCode::Right => T::Right,
        KeyCode::Up => T::Up,
        KeyCode::Down => T::Down,
        KeyCode::Home => T::Home,
        KeyCode::End => T::End,
        KeyCode::PageUp => T::PageUp,
        KeyCode::PageDown => T::PageDown,
        KeyCode::Insert => T::Insert,
        KeyCode::F(n) => T::F(n),
    })
}

fn selected_id(ed: &Editor) -> Option<AgentId> {
    ed.agents.selected().map(|a| a.id)
}

fn list_key(ed: &mut Editor, key: KeyEvent) {
    let n = ed.agents.agents.len();
    let page = (layout(ed.area).content.height as usize / 2).max(1);
    match (key.code, key.mods.ctrl) {
        (KeyCode::Esc, _) | (KeyCode::Char('q'), false) => close(ed),
        (KeyCode::Char('c'), true) => close(ed),
        (KeyCode::Char('j') | KeyCode::Down, false) if n > 0 => {
            let p = &mut ed.agents.panel;
            p.selected = (p.selected + 1) % n;
            p.scroll = 0;
        }
        (KeyCode::Char('k') | KeyCode::Up, false) if n > 0 => {
            let p = &mut ed.agents.panel;
            p.selected = (p.selected + n - 1) % n;
            p.scroll = 0;
        }
        (KeyCode::Char(':'), false) => {
            crate::prompt::open(ed, crate::prompt::PromptKind::Command);
        }
        (KeyCode::Char('n'), false) => {
            ed.agents.panel.overlay = Some(Overlay::Input {
                purpose: InputPurpose::NewTask,
                label: "task".into(),
                text: String::new(),
            });
        }
        (KeyCode::Char('N'), false) => compete(ed),
        (KeyCode::Char('v'), false) => {
            let p = &mut ed.agents.panel;
            p.view = p.view.next();
            p.scroll = 0;
        }
        (KeyCode::PageUp, _) => scroll(ed, page as isize),
        (KeyCode::PageDown, _) => scroll(ed, -(page as isize)),
        _ if is_toggle(key) => take_keys(ed),
        (KeyCode::Enter | KeyCode::Char('r'), false) => {
            if let Some(id) = selected_id(ed) {
                super::review::open(ed, id);
            }
        }
        (KeyCode::Char('t'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::run_tests(ed, id);
            }
        }
        (KeyCode::Char('f'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::ask_feedback(ed, id);
            }
        }
        (KeyCode::Char('p'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::choose_permission(ed, id);
            }
        }
        (KeyCode::Char('I'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::prepare_integrate(ed, id);
            }
        }
        (KeyCode::Char('X'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::confirm_reject(ed, id);
            }
        }
        (KeyCode::Char('A'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::confirm_archive(ed, id);
            }
        }
        _ => {}
    }
}

fn scroll(ed: &mut Editor, delta: isize) {
    if ed.agents.panel.view == DetailView::Live
        && let Some(t) = ed.agents.selected().and_then(|a| a.runtime.terminal())
    {
        t.scroll(delta);
        return;
    }
    let p = &mut ed.agents.panel;
    p.scroll = (p.scroll as isize + delta).max(0) as usize;
}

/// `C-\` in the list: give the keys to the selected agent's terminal.
fn take_keys(ed: &mut Editor) {
    let Some(a) = ed.agents.selected() else {
        return;
    };
    let id = a.id;
    match (&a.runtime, a.kind()) {
        (super::Runtime::Terminal(t), _) if t.exit_status().is_none() => {
            t.scroll(0);
            let p = &mut ed.agents.panel;
            p.terminal_focus = Some(id);
            p.view = DetailView::Live;
            ed.info("keys go to the agent's terminal; C-\\ takes them back");
        }
        (super::Runtime::Terminal(_), _) => ed.error("the agent's program has exited"),
        (_, ProviderKind::Acp) => {
            ed.error("this is an ACP agent: it has no terminal; f sends it a prompt")
        }
        _ => ed.error("the agent is not running"),
    }
}

/// `N`: the same task again with another provider.
fn compete(ed: &mut Editor) {
    let Some(a) = ed.agents.selected() else {
        ed.error("select an agent first; N starts another provider on its task");
        return;
    };
    let task = a.task.clone();
    let group = Some(a.group);
    let base = a.worktree.as_ref().map(|w| w.base.clone());
    let taken: Vec<String> = ed
        .agents
        .agents
        .iter()
        .filter(|x| x.group == a.group)
        .map(|x| x.provider.name.clone())
        .collect();
    let providers: Vec<ProviderSpec> = super::providers(ed)
        .into_iter()
        .filter(|p| !taken.contains(&p.name))
        .collect();
    if providers.is_empty() {
        ed.error("every configured provider already works on this task");
        return;
    }
    choose_provider(ed, task, group, base, providers);
}

pub(crate) fn choose_provider(
    ed: &mut Editor,
    task: String,
    group: Option<u64>,
    base: Option<String>,
    providers: Vec<ProviderSpec>,
) {
    if providers.is_empty() {
        ed.error(
            "no agent providers: install gemini, opencode, claude or codex, \
             or add [agents.provider.<name>] to the config",
        );
        return;
    }
    let items = providers
        .iter()
        .map(|p| {
            let detail = match p.kind {
                ProviderKind::Acp => format!("acp  {}", p.program),
                ProviderKind::Terminal => format!("terminal only  {}", p.program),
            };
            (p.name.clone(), detail)
        })
        .collect();
    ed.agents.panel.overlay = Some(Overlay::Choice {
        purpose: ChoicePurpose::Provider {
            task,
            group,
            base,
            providers,
        },
        title: "provider".into(),
        items,
        selected: 0,
    });
}

fn overlay_key(ed: &mut Editor, key: KeyEvent) {
    let Some(overlay) = ed.agents.panel.overlay.take() else {
        return;
    };
    match overlay {
        Overlay::Input {
            purpose,
            label,
            mut text,
        } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Char('c') if key.mods.ctrl => {}
            KeyCode::Enter if key.mods.alt => {
                text.push('\n');
                ed.agents.panel.overlay = Some(Overlay::Input {
                    purpose,
                    label,
                    text,
                });
            }
            KeyCode::Enter => submit_input(ed, purpose, text),
            _ => {
                match key.code {
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char('u') if key.mods.ctrl => text.clear(),
                    _ => {
                        if let Some(c) = key.typed_char() {
                            text.push(c);
                        }
                    }
                }
                ed.agents.panel.overlay = Some(Overlay::Input {
                    purpose,
                    label,
                    text,
                });
            }
        },
        Overlay::Choice {
            purpose,
            title,
            items,
            mut selected,
        } => {
            let n = items.len().max(1);
            match key.code {
                KeyCode::Esc => return,
                KeyCode::Char('c') if key.mods.ctrl => return,
                KeyCode::Enter => {
                    choose(ed, purpose, selected);
                    return;
                }
                KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => selected = (selected + 1) % n,
                KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                    selected = (selected + n - 1) % n
                }
                _ => {}
            }
            ed.agents.panel.overlay = Some(Overlay::Choice {
                purpose,
                title,
                items,
                selected,
            });
        }
        Overlay::Confirm(c) => confirm_key(ed, c, key),
    }
}

fn submit_input(ed: &mut Editor, purpose: InputPurpose, text: String) {
    match purpose {
        InputPurpose::NewTask => {
            let task = text.trim().to_string();
            if task.is_empty() {
                ed.error("the task is empty");
                return;
            }
            let providers = super::providers(ed);
            choose_provider(ed, task, None, None, providers);
        }
        InputPurpose::Feedback(id) => super::actions::send_feedback(ed, id, text.trim()),
        InputPurpose::Comment {
            agent,
            path,
            line,
            old,
        } => {
            let text = text.trim().to_string();
            if let Some(a) = ed.agents.get_mut(agent) {
                // An empty comment removes the one on that line.
                a.comments
                    .retain(|c| !(c.path == path && c.line == line && c.old == old && !c.sent));
                if !text.is_empty() {
                    a.comments.push(super::Comment {
                        path,
                        line,
                        old,
                        text,
                        sent: false,
                    });
                }
            }
        }
    }
}

fn choose(ed: &mut Editor, purpose: ChoicePurpose, index: usize) {
    match purpose {
        ChoicePurpose::Provider {
            task,
            group,
            base,
            providers,
        } => {
            let Some(provider) = providers.get(index).cloned() else {
                return;
            };
            super::create::create(
                ed,
                NewAgent {
                    task,
                    provider,
                    group,
                    base,
                },
            );
            ed.agents.panel.view = DetailView::Live;
            ed.agents.panel.scroll = 0;
        }
        ChoicePurpose::Permission(id) => super::actions::permission_chosen(ed, id, index),
    }
}

fn confirm_key(ed: &mut Editor, c: Confirm, key: KeyEvent) {
    let ch = key.typed_char();
    if key.code == KeyCode::Esc || ch == Some('n') || (key.mods.ctrl && ch.is_none()) {
        ed.info(format!("{}: cancelled, nothing changed", c.title));
        return;
    }
    match (&c.action, ch, key.code) {
        (ConfirmAction::Reject(id), Some('y'), _) => super::actions::reject(ed, *id, false),
        (ConfirmAction::Reject(id), Some('a'), _) => super::actions::reject(ed, *id, true),
        (ConfirmAction::Integrate { agent, how, facts }, _, KeyCode::Tab) => {
            let how = match how {
                Integration::Merge => Integration::FastForwardOnly,
                Integration::FastForwardOnly => Integration::CherryPick,
                Integration::CherryPick => Integration::Merge,
            };
            ed.agents.panel.overlay = Some(Overlay::Confirm(super::actions::integrate_dialog(
                *agent,
                how,
                facts.clone(),
            )));
        }
        (ConfirmAction::Integrate { agent, how, facts }, Some('y'), _) => {
            super::actions::integrate(ed, *agent, *how, facts.clone())
        }
        (ConfirmAction::Archive(id), Some('y'), _) => super::actions::archive(ed, *id, false),
        (ConfirmAction::ForceArchive(id), Some('F'), _) => super::actions::archive(ed, *id, true),
        (
            ConfirmAction::Grant {
                agent,
                request_id,
                option,
            },
            Some('y'),
            _,
        ) => super::actions::answer_permission(ed, *agent, request_id.clone(), option),
        _ => {
            // Any other key leaves the dialog up.
            ed.agents.panel.overlay = Some(Overlay::Confirm(c));
        }
    }
}

/// Key hints for the panel's last line, as (keys, what they do).
pub fn hints(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    let p = &ed.agents.panel;
    if p.terminal_focus.is_some() {
        return vec![("C-\\", "take the keys back from the terminal")];
    }
    match &p.overlay {
        Some(Overlay::Input { .. }) => {
            return vec![
                ("ret", "submit"),
                ("A-ret", "new line"),
                ("C-u", "clear"),
                ("esc", "cancel"),
            ];
        }
        Some(Overlay::Choice { .. }) => {
            return vec![("j/k", "move"), ("ret", "choose"), ("esc", "cancel")];
        }
        Some(Overlay::Confirm(_)) => return vec![("esc/n", "cancel")],
        None => {}
    }
    if p.review.is_some() {
        return super::review::hints();
    }
    let mut out = vec![("n", "new task"), ("j/k", "select")];
    let Some(a) = ed.agents.selected() else {
        out.push(("esc", "close"));
        return out;
    };
    if a.kind() == ProviderKind::Terminal && a.is_running() {
        out.push(("C-\\", "keys to terminal"));
    }
    if a.permission.is_some() {
        out.push(("p", "answer permission"));
    }
    out.extend([
        ("ret", "review"),
        ("t", "test"),
        ("f", "feedback"),
        ("N", "compete"),
        ("v", "view"),
        ("I", "integrate"),
        ("X", "reject"),
        ("A", "archive"),
        ("esc", "close"),
    ]);
    out
}
