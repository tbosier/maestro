//! The agent panel: a full-screen overlay over the editor area with the
//! agent list on the left and the selected agent on the right. Keys go to
//! the panel while it is open; `C-\` hands them to the selected agent's
//! terminal and back.

use maestro_git::Integration;

use super::contract::Contract;
use super::create::NewAgent;
use super::review::ReviewView;
use super::{AgentId, ProviderKind, ProviderSpec};
use crate::editor::{Editor, Mode, MouseKind};
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
    Message(AgentId),
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
        contract: Option<Contract>,
        providers: Vec<ProviderSpec>,
    },
    Permission(AgentId),
    /// The provider a found worktree is adopted with.
    Adopt {
        agent: AgentId,
        providers: Vec<ProviderSpec>,
    },
}

/// What a confirmation dialog does when accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// `y` rejects, `a` rejects and archives the worktree.
    Reject(AgentId),
    /// `y` goes on to the provider list for a task with this contract,
    /// which the dialog shows in full.
    StartTask {
        task: String,
        contract: Box<Contract>,
    },
    /// `y` integrates with `how`; `tab` changes it. When files outside the
    /// task contract would be integrated, `y` is refused and only `O`
    /// integrates.
    Integrate {
        agent: AgentId,
        how: Integration,
        facts: Box<super::actions::IntegrateFacts>,
    },
    /// `y` removes the worktree (refused if it has work in it).
    Archive(AgentId),
    /// `F` force-removes the worktree, deleting what `lines` lists.
    ForceArchive(AgentId),
    /// `y` discards the agent's change to one file in its worktree.
    DropFile {
        agent: AgentId,
        old: Option<std::path::PathBuf>,
        new: Option<std::path::PathBuf>,
    },
    /// `y` sends the permission answer.
    Grant {
        agent: AgentId,
        request_id: serde_json::Value,
        option: String,
    },
    /// `y` commits the checked hunks of several agents as a composed
    /// result with a worktree of its own.
    Compose(Box<super::compose::ComposePlan>),
    /// The `?` list of every key on the current screen. Any key closes it.
    Keys,
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
    /// The compose view, when open.
    pub compose: Option<super::compose::ComposeView>,
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
/// Rows of facts above the content in the agent panel and its review:
/// two more than [`SUMMARY_ROWS`], for the task contract and the overlaps
/// with other agents.
pub const AGENT_SUMMARY_ROWS: u16 = 6;

/// The panel's parts with [`SUMMARY_ROWS`] of facts (the proposal review).
pub fn layout(area: Rect) -> PanelLayout {
    layout_rows(area, SUMMARY_ROWS)
}

/// The agent panel's parts, with [`AGENT_SUMMARY_ROWS`] of facts.
pub fn agent_layout(area: Rect) -> PanelLayout {
    layout_rows(area, AGENT_SUMMARY_ROWS)
}

fn layout_rows(area: Rect, summary_rows: u16) -> PanelLayout {
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
    let summary_h = summary_rows.min(body_h);
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
    let c = agent_layout(area).content;
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
    // Records saved by an earlier session, and found worktrees, arrive
    // from a background scan (when sessions are on).
    super::persist::load_for_cwd(ed);
    if n == 0 && !ed.agents.persist.loading {
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
    if ed.agents.panel.compose.is_some() {
        super::compose::handle_key(ed, key);
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
    // As in a terminal emulator, typing returns to the live screen.
    t.scroll(0);
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
    let page = (agent_layout(ed.area).content.height as usize / 2).max(1);
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
        // `f` was this key's name before 0.7 ("feedback"); it still works.
        (KeyCode::Char('m' | 'f'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::ask_message(ed, id);
            }
        }
        (KeyCode::Char('i'), false) => type_into(ed),
        (KeyCode::Char('?'), _) => show_keys(ed),
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
        (KeyCode::Char('R'), false) => {
            if let Some(id) = selected_id(ed) {
                super::create::relaunch(ed, id);
            }
        }
        (KeyCode::Char('a'), false) => {
            if let Some(id) = selected_id(ed) {
                super::actions::ask_adopt(ed, id);
            }
        }
        (KeyCode::Char('C'), false) => {
            if let Some(id) = selected_id(ed) {
                super::compose::open(ed, id);
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

/// A mouse event while the panel is open, at screen cell (x, y). Only the
/// agent list uses the mouse: a click on a row selects that agent, a click
/// in a running terminal agent's live screen gives it the keys, and a
/// click anywhere else takes them back. The wheel scrolls the list or the
/// detail under the pointer. Dialogs, the review and the compose view
/// keep to the keyboard.
pub fn handle_mouse(ed: &mut Editor, kind: MouseKind, x: u16, y: u16) {
    let p = &ed.agents.panel;
    if p.overlay.is_some() || p.review.is_some() || p.compose.is_some() {
        return;
    }
    let l = agent_layout(ed.area);
    let n = ed.agents.agents.len();
    match kind {
        MouseKind::ScrollUp | MouseKind::ScrollDown => {
            let up = kind == MouseKind::ScrollUp;
            if l.list.contains(x, y) {
                if ed.agents.panel.terminal_focus.is_some() || n == 0 {
                    return;
                }
                let p = &mut ed.agents.panel;
                p.selected = if up {
                    p.selected.saturating_sub(1)
                } else {
                    (p.selected + 1).min(n - 1)
                };
                p.scroll = 0;
            } else if l.summary.contains(x, y) || l.content.contains(x, y) {
                scroll(ed, if up { 3 } else { -3 });
            }
        }
        MouseKind::Down => {
            // A click in the panel takes the keys from the sidebar.
            ed.workspaces.sidebar.focused = false;
            if l.content.contains(x, y) {
                if ed.agents.panel.terminal_focus.is_some() {
                    return;
                }
                let Some(a) = ed.agents.selected() else {
                    return;
                };
                let id = a.id;
                let live_terminal = ed.agents.panel.view == DetailView::Live
                    && a.runtime
                        .terminal()
                        .is_some_and(|t| t.exit_status().is_none());
                if live_terminal {
                    focus_terminal(ed, id);
                }
                return;
            }
            if ed.agents.panel.terminal_focus.take().is_some() {
                ed.info("keys are back with the panel");
            }
            if l.list.contains(x, y) && n > 0 {
                let p = &mut ed.agents.panel;
                let (first, visible) = list_window(p.selected, l.list.height);
                let row = ((y - l.list.y) / LIST_ROW_HEIGHT) as usize;
                let i = first + row;
                // The list's last line is blank when its height is odd.
                if row < visible && i < n {
                    p.selected = i;
                    p.scroll = 0;
                }
            }
        }
        MouseKind::Drag => {}
    }
}

/// Lines each agent takes in the list.
pub const LIST_ROW_HEIGHT: u16 = 2;

/// The first agent the list shows and how many fit, so the selected one
/// is on screen. The front end draws with it; the mouse maps clicks with it.
pub fn list_window(selected: usize, height: u16) -> (usize, usize) {
    let visible = (height / LIST_ROW_HEIGHT).max(1) as usize;
    let first = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };
    (first, visible)
}

/// `C-\` in the list: give the keys to the selected agent's terminal.
fn take_keys(ed: &mut Editor) {
    let Some(a) = ed.agents.selected() else {
        return;
    };
    let id = a.id;
    match (&a.runtime, a.kind()) {
        (super::Runtime::Terminal(t), _) if t.exit_status().is_none() => focus_terminal(ed, id),
        (super::Runtime::Terminal(_), _) => ed.error("the agent's program has exited"),
        (_, ProviderKind::Acp) => {
            ed.error("this is an ACP agent: it has no terminal; m sends it a message")
        }
        _ => ed.error("the agent is not running"),
    }
}

/// `i` in the list: talk to the selected agent. A running terminal agent
/// gets the keys, as with `C-\`; any other agent gets the message box
/// (`m`), which says why when nothing can reach it.
fn type_into(ed: &mut Editor) {
    let Some(a) = ed.agents.selected() else {
        ed.error("no agent selected: n starts one");
        return;
    };
    let id = a.id;
    match &a.runtime {
        super::Runtime::Terminal(t) if t.exit_status().is_none() => focus_terminal(ed, id),
        _ => super::actions::ask_message(ed, id),
    }
}

/// Gives the keys to a running terminal agent and shows its live screen.
pub(crate) fn focus_terminal(ed: &mut Editor, id: AgentId) {
    if let Some(t) = ed.agents.get(id).and_then(|a| a.runtime.terminal()) {
        t.scroll(0);
    }
    let p = &mut ed.agents.panel;
    p.terminal_focus = Some(id);
    p.view = DetailView::Live;
    ed.info(
        "you are typing into the agent's terminal; C-\\ or a click outside it takes the keys back",
    );
}

/// `N`: the same task again with another provider.
fn compete(ed: &mut Editor) {
    let Some(a) = ed.agents.selected() else {
        ed.error("select an agent first; N starts another provider on its task");
        return;
    };
    if let Some(why) = super::actions::found_refusal(a) {
        ed.error(why);
        return;
    }
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
    // A competing implementation works under the same contract.
    let contract = a.contract.clone();
    choose_provider(ed, task, group, base, contract, providers);
}

pub(crate) fn choose_provider(
    ed: &mut Editor,
    task: String,
    group: Option<u64>,
    base: Option<String>,
    contract: Option<Contract>,
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
    let title = match &contract {
        Some(_) => "provider (task with a contract)",
        None => "provider (no task contract)",
    };
    ed.agents.panel.overlay = Some(Overlay::Choice {
        purpose: ChoicePurpose::Provider {
            task,
            group,
            base,
            contract,
            providers,
        },
        title: title.into(),
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
            let parsed = match super::contract::parse_task(&text, &ed.cwd) {
                Ok(p) => p,
                Err(e) => {
                    // The text stays, so the mistake can be fixed.
                    ed.error(format!("task contract: {e}"));
                    ed.agents.panel.overlay = Some(Overlay::Input {
                        purpose: InputPurpose::NewTask,
                        label: "task".into(),
                        text,
                    });
                    return;
                }
            };
            if parsed.task.is_empty() {
                ed.error("the task is empty");
                return;
            }
            match parsed.contract {
                // The whole contract is shown before anything starts: it
                // may come from a file, and `t` runs its command.
                Some(contract) => {
                    let mut lines = contract.describe();
                    lines.push(
                        "It is sent to the agent with the task as a request. Maestro refuses \
                         file writes the agent asks it to make outside the allowed paths, marks \
                         changed files outside them in the review and asks again before \
                         integrating them; the agent's own tools are not stopped."
                            .into(),
                    );
                    ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
                        title: "task contract".into(),
                        lines,
                        keys: vec![
                            ("y".into(), "choose a provider".into()),
                            ("n".into(), "cancel".into()),
                        ],
                        action: ConfirmAction::StartTask {
                            task: parsed.task,
                            contract: Box::new(contract),
                        },
                    }));
                }
                None => {
                    let providers = super::providers(ed);
                    choose_provider(ed, parsed.task, None, None, None, providers);
                }
            }
        }
        InputPurpose::Message(id) => super::actions::send_message(ed, id, text.trim()),
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
            contract,
            providers,
        } => {
            let Some(provider) = providers.get(index).cloned() else {
                return;
            };
            if ed.agents.persist.loading {
                // Ids of new agents continue after the saved ones; wait
                // for them so a new branch never reuses a saved name.
                ed.error(
                    "still reading the agent records saved for this repository; \
                     try again in a moment",
                );
                return;
            }
            super::create::create(
                ed,
                NewAgent {
                    task,
                    provider,
                    group,
                    base,
                    contract,
                },
            );
            ed.agents.panel.view = DetailView::Live;
            ed.agents.panel.scroll = 0;
        }
        ChoicePurpose::Permission(id) => super::actions::permission_chosen(ed, id, index),
        ChoicePurpose::Adopt { agent, providers } => {
            if let Some(p) = providers.get(index).cloned() {
                super::actions::adopt(ed, agent, p);
            }
        }
    }
}

/// The box width and text width of a dialog drawn in `area`.
pub fn dialog_width(area: Rect) -> (u16, usize) {
    let w = area.width.saturating_sub(4).min(90);
    (w, w.saturating_sub(4) as usize)
}

/// The terminal cells a grapheme takes when the front end draws it: control
/// characters are drawn as one `?`, everything else at its display width
/// and at least one cell (as `maestro-tui`'s surface does).
pub fn cell_width(g: &str) -> usize {
    if g.chars().any(char::is_control) {
        1
    } else {
        maestro_core::text::grapheme_width(g, 0, 1).max(1)
    }
}

fn graphemes(s: &str) -> Vec<String> {
    let rope = maestro_core::Rope::from_str(s);
    maestro_core::text::graphemes(rope.slice(..))
        .map(|g| g.into_owned())
        .collect()
}

/// Cells `s` takes on screen.
pub fn text_width(s: &str) -> usize {
    graphemes(s).iter().map(|g| cell_width(g)).sum()
}

/// Splits `text` into lines of at most `width` terminal cells, at spaces
/// where it can. Widths are counted as the screen draws them, so a line
/// of wide characters is never cut off at the dialog's edge.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for word in text.split(' ') {
        let mut gs = graphemes(word);
        loop {
            let word_w: usize = gs.iter().map(|g| cell_width(g)).sum();
            let sep = usize::from(!cur.is_empty());
            if cur_w + sep + word_w <= width {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(&gs.concat());
                cur_w += sep + word_w;
                break;
            }
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                continue;
            }
            // A word wider than a line is cut, by whole graphemes.
            let mut head_w = 0;
            let mut n = 0;
            while n < gs.len() && head_w + cell_width(&gs[n]) <= width {
                head_w += cell_width(&gs[n]);
                n += 1;
            }
            // A single grapheme wider than the line still goes somewhere.
            let n = n.max(1);
            out.push(gs[..n].concat());
            gs.drain(..n);
            if gs.is_empty() {
                break;
            }
        }
    }
    out.push(cur);
    out
}

/// A confirmation dialog's text as drawn in `area`.
pub fn confirm_lines(area: Rect, c: &Confirm) -> Vec<String> {
    dialog_lines(area, &c.lines)
}

/// True when the whole dialog, keys included, fits on screen. A dialog
/// that does not fit cannot be accepted, so nothing it would hide can be
/// agreed to unseen.
pub fn confirm_fits(area: Rect, c: &Confirm) -> bool {
    dialog_fits(area, &c.lines, &c.keys)
}

/// Any dialog's text lines, wrapped as drawn in `area`.
pub fn dialog_lines(area: Rect, lines: &[String]) -> Vec<String> {
    let (_, inner) = dialog_width(area);
    lines.iter().flat_map(|l| wrap_words(l, inner)).collect()
}

/// [`confirm_fits`] for any dialog's lines and keys.
pub fn dialog_fits(area: Rect, lines: &[String], keys: &[(String, String)]) -> bool {
    let (_, inner) = dialog_width(area);
    let keys: usize = keys
        .iter()
        .map(|(k, d)| k.chars().count() + d.chars().count() + 4)
        .sum();
    area.width >= 20
        && area.height >= 6
        && dialog_lines(area, lines).len() + 4 < area.height as usize
        && keys <= inner + 3
}

fn confirm_key(ed: &mut Editor, c: Confirm, key: KeyEvent) {
    if c.action == ConfirmAction::Keys {
        return;
    }
    let ch = key.typed_char();
    if key.code == KeyCode::Esc || ch == Some('n') || (key.mods.ctrl && ch.is_none()) {
        ed.info(format!("{}: canceled, nothing changed", c.title));
        return;
    }
    if !confirm_fits(ed.area, &c) && key.code != KeyCode::Tab {
        ed.error("the window is too small to show this whole dialog; make it larger, or press n");
        ed.agents.panel.overlay = Some(Overlay::Confirm(c));
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
                (**facts).clone(),
            )));
        }
        (ConfirmAction::Integrate { facts, .. }, Some('y'), _)
            if !facts.out_of_scope.is_empty() =>
        {
            ed.error(format!(
                "{} file(s) are outside the task contract: O integrates them anyway; \
                 D in the review drops one",
                facts.out_of_scope.len()
            ));
            ed.agents.panel.overlay = Some(Overlay::Confirm(c));
        }
        (ConfirmAction::Integrate { agent, how, facts }, Some('y'), _) => {
            super::actions::integrate(ed, *agent, *how, (**facts).clone())
        }
        (ConfirmAction::Integrate { agent, how, facts }, Some('O'), _)
            if !facts.out_of_scope.is_empty() =>
        {
            super::actions::integrate(ed, *agent, *how, (**facts).clone())
        }
        (ConfirmAction::StartTask { task, contract }, Some('y'), _) => {
            let providers = super::providers(ed);
            choose_provider(
                ed,
                task.clone(),
                None,
                None,
                Some((**contract).clone()),
                providers,
            );
        }
        (ConfirmAction::Archive(id), Some('y'), _) => super::actions::archive(ed, *id, false),
        (ConfirmAction::ForceArchive(id), Some('F'), _) => super::actions::archive(ed, *id, true),
        (ConfirmAction::DropFile { agent, old, new }, Some('y'), _) => {
            super::actions::drop_file(ed, *agent, old.clone(), new.clone())
        }
        (
            ConfirmAction::Grant {
                agent,
                request_id,
                option,
            },
            Some('y'),
            _,
        ) => super::actions::answer_permission(ed, *agent, request_id.clone(), option),
        (ConfirmAction::Compose(plan), Some('y'), _) => {
            super::compose::create(ed, (**plan).clone())
        }
        _ => {
            // Any other key leaves the dialog up.
            ed.agents.panel.overlay = Some(Overlay::Confirm(c));
        }
    }
}

/// `?`: the current screen's keys, all of them the hint line names, in a
/// box, for when the hint line is too narrow to show them all. Several go
/// on each line so the box fits a short window.
pub fn show_keys(ed: &mut Editor) {
    let (_, inner) = dialog_width(ed.area);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for (k, d) in hints(ed) {
        let item = format!("{k} {d}");
        if !line.is_empty() && line.chars().count() + 3 + item.chars().count() > inner {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push_str("   ");
        }
        line.push_str(&item);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    ed.agents.panel.overlay = Some(Overlay::Confirm(Confirm {
        title: "keys".into(),
        lines,
        keys: vec![("any key".into(), "close".into())],
        action: ConfirmAction::Keys,
    }));
}

/// `?` opens the key list here: the list, the review and the compose
/// view, with no dialog open and the keys not given to a terminal.
pub fn keys_box_available(ed: &Editor) -> bool {
    let p = &ed.agents.panel;
    p.terminal_focus.is_none() && p.overlay.is_none()
}

/// Key hints for the panel's last line, as (keys, what they do).
pub fn hints(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    let p = &ed.agents.panel;
    if p.terminal_focus.is_some() {
        return vec![
            ("C-\\", "back to the panel"),
            ("click", "outside the terminal also returns"),
        ];
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
        Some(Overlay::Confirm(c)) if c.action == ConfirmAction::Keys => {
            return vec![("any key", "close")];
        }
        Some(Overlay::Confirm(_)) => return vec![("esc/n", "cancel")],
        None => {}
    }
    if p.compose.is_some() {
        return super::compose::hints();
    }
    if p.review.is_some() {
        return super::review::hints();
    }
    let mut out = vec![("n", "new task"), ("j/k", "select")];
    let Some(a) = ed.agents.selected() else {
        out.push(("esc", "close"));
        return out;
    };
    if a.origin == super::Origin::Found {
        out.extend([
            ("ret", "review"),
            ("a", "adopt"),
            ("A", "archive"),
            ("v", "view"),
            ("esc", "close"),
        ]);
        return out;
    }
    // First, so it is the last hint a narrow panel cuts: the way into the
    // agent.
    if a.kind() == ProviderKind::Terminal && a.is_running() {
        out.insert(0, ("i", "type into agent"));
    }
    if !a.permissions.is_empty() {
        out.push(("p", "answer permission"));
    }
    // Relaunch comes first for an agent with no process, where it is the
    // next thing to do, and is listed for any agent it applies to.
    let relaunchable = a.can_relaunch();
    if relaunchable && a.from_earlier_session() {
        out.push(("R", "relaunch"));
    }
    out.extend([("ret", "review"), ("t", "test")]);
    // A message cannot reach an agent with no process; R comes first
    // instead. A composed result has no agent at all.
    if !a.from_earlier_session() && a.origin != super::Origin::Composed {
        out.push(("m", "message"));
    }
    out.push(("N", "compete"));
    let group = a.group;
    if ed
        .agents
        .agents
        .iter()
        .filter(|x| x.group == group && !matches!(x.origin, super::Origin::Composed))
        .count()
        >= 2
    {
        out.push(("C", "compose"));
    }
    out.extend([
        ("v", "view"),
        ("I", "integrate"),
        ("X", "reject"),
        ("A", "archive"),
    ]);
    if relaunchable && !a.from_earlier_session() {
        out.push(("R", "relaunch"));
    }
    out.extend([("pgup/pgdn", "scroll"), (":", "command"), ("esc", "close")]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_wrap_and_long_words_are_cut() {
        assert_eq!(wrap_words("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap_words("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap_words("", 3), [""]);
    }

    #[test]
    fn wrapping_counts_screen_cells_not_chars() {
        // 44 wide characters take 88 cells: a line of 80 cells cannot hold
        // them, so the command after them must land on a line of its own.
        let cmd = format!("echo {};touch${{IFS}}marker", "界".repeat(44));
        let lines = wrap_words(&cmd, 80);
        assert!(lines.iter().all(|l| text_width(l) <= 80), "{lines:?}");
        assert_eq!(lines.concat().replace(' ', ""), cmd.replace(' ', ""));
        assert!(lines.len() >= 2, "{lines:?}");
        assert_eq!(wrap_words("ab 界界", 4), ["ab", "界界"]);
    }

    #[test]
    fn a_dialog_that_does_not_fit_cannot_be_accepted() {
        let c = Confirm {
            title: "t".into(),
            lines: vec!["one line".into(); 6],
            keys: vec![("y".into(), "do it".into())],
            action: ConfirmAction::Archive(AgentId(1)),
        };
        assert!(confirm_fits(Rect::new(0, 0, 80, 20), &c));
        // Six lines plus the frame and keys need eleven rows.
        assert!(!confirm_fits(Rect::new(0, 0, 80, 10), &c));
        assert!(!confirm_fits(Rect::new(0, 0, 10, 40), &c));
    }
}
