//! `:` commands.

use std::path::{Path, PathBuf};

use maestro_core::document::SaveError;
use maestro_core::{Change, ChangeSet, Transaction};

use crate::editor::{Editor, Mode, Presentation, Severity};
use crate::layout::Direction;

pub type TypedFn = fn(&mut Editor, &[&str], bool) -> Result<(), String>;

pub struct TypedCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub doc: &'static str,
    pub fun: TypedFn,
}

static TYPED: &[TypedCommand] = &[
    TypedCommand {
        name: "write",
        aliases: &["w"],
        doc: "Save the buffer, optionally to a new path (! overwrites)",
        fun: write,
    },
    TypedCommand {
        name: "write-quit",
        aliases: &["wq", "x"],
        doc: "Save, then close the view or quit",
        fun: write_quit,
    },
    TypedCommand {
        name: "write-all",
        aliases: &["wa"],
        doc: "Save every modified buffer",
        fun: write_all,
    },
    TypedCommand {
        name: "quit",
        aliases: &["q"],
        doc: "Close the view, or quit if it is the last (! discards changes)",
        fun: quit,
    },
    TypedCommand {
        name: "quit-all",
        aliases: &["qa"],
        doc: "Quit (! discards changes)",
        fun: quit_all,
    },
    TypedCommand {
        name: "write-quit-all",
        aliases: &["wqa", "xa"],
        doc: "Save every buffer and quit",
        fun: write_quit_all,
    },
    TypedCommand {
        name: "agents",
        aliases: &[],
        doc: "Show or hide the agent panel",
        fun: agents,
    },
    TypedCommand {
        name: "proposals",
        aliases: &[],
        doc: "Show or hide the review of edit proposals from automation clients",
        fun: proposals,
    },
    TypedCommand {
        name: "open",
        aliases: &["e", "edit"],
        doc: "Open a file (e! reloads the current one)",
        fun: open,
    },
    TypedCommand {
        name: "reload",
        aliases: &[],
        doc: "Reload the buffer from disk (! discards changes)",
        fun: reload,
    },
    TypedCommand {
        name: "new",
        aliases: &["enew"],
        doc: "Open a new scratch buffer",
        fun: new_buffer,
    },
    TypedCommand {
        name: "buffer-close",
        aliases: &["bd", "bc"],
        doc: "Close the buffer (! discards changes)",
        fun: buffer_close,
    },
    TypedCommand {
        name: "buffer-next",
        aliases: &["bn"],
        doc: "Show the next buffer",
        fun: buffer_next,
    },
    TypedCommand {
        name: "buffer-previous",
        aliases: &["bp"],
        doc: "Show the previous buffer",
        fun: buffer_prev,
    },
    TypedCommand {
        name: "buffers",
        aliases: &["ls"],
        doc: "Pick an open buffer",
        fun: buffers,
    },
    TypedCommand {
        name: "split",
        aliases: &["sp"],
        doc: "Split top and bottom, optionally opening a file",
        fun: split,
    },
    TypedCommand {
        name: "vsplit",
        aliases: &["vs"],
        doc: "Split side by side, optionally opening a file",
        fun: vsplit,
    },
    TypedCommand {
        name: "close",
        aliases: &["clo"],
        doc: "Close the view",
        fun: close,
    },
    TypedCommand {
        name: "only",
        aliases: &["on"],
        doc: "Close every other view",
        fun: only,
    },
    TypedCommand {
        name: "set",
        aliases: &[],
        doc: "Set an option: tab-width, indent, indent-width, line-numbers, scrolloff, cursorline",
        fun: set,
    },
    TypedCommand {
        name: "nohlsearch",
        aliases: &["noh"],
        doc: "Forget the last search pattern",
        fun: noh,
    },
    TypedCommand {
        name: "substitute",
        aliases: &["s"],
        doc: "Regex replace: :s/pat/rep/[gi] on selected lines, :%s for the whole file",
        fun: substitute_cmd,
    },
    TypedCommand {
        name: "select-nodes",
        aliases: &[],
        doc: "Select every syntax node a tree-sitter query or shortcut matches (steps chain with |)",
        fun: crate::structural::select_nodes,
    },
    TypedCommand {
        name: "select-inside",
        aliases: &[],
        doc: "Select the syntax nodes a query or shortcut matches inside each selection",
        fun: crate::structural::select_inside,
    },
    TypedCommand {
        name: "keep-nodes",
        aliases: &[],
        doc: "Keep selections that are exactly a syntax node the query matches",
        fun: crate::structural::keep_nodes,
    },
    TypedCommand {
        name: "remove-nodes",
        aliases: &[],
        doc: "Remove selections that are exactly a syntax node the query matches",
        fun: crate::structural::remove_nodes,
    },
    TypedCommand {
        name: "keep-containing",
        aliases: &[],
        doc: "Keep selections that contain a syntax node the query matches",
        fun: crate::structural::keep_containing,
    },
    TypedCommand {
        name: "remove-containing",
        aliases: &[],
        doc: "Remove selections that contain a syntax node the query matches",
        fun: crate::structural::remove_containing,
    },
    TypedCommand {
        name: "keep-matching",
        aliases: &[],
        doc: "Keep selections whose text matches a regex",
        fun: crate::structural::keep_matching,
    },
    TypedCommand {
        name: "remove-matching",
        aliases: &[],
        doc: "Remove selections whose text matches a regex",
        fun: crate::structural::remove_matching,
    },
    TypedCommand {
        name: "expand-to",
        aliases: &[],
        doc: "Grow each selection to its enclosing syntax node of a kind (dropping ones with none)",
        fun: crate::structural::expand_to,
    },
    TypedCommand {
        name: "insert-above",
        aliases: &[],
        doc: "Insert a line above each selection with its indentation, as one undo step",
        fun: crate::structural::insert_above_cmd,
    },
    TypedCommand {
        name: "goto",
        aliases: &["g"],
        doc: "Go to a line number",
        fun: goto,
    },
    TypedCommand {
        name: "pwd",
        aliases: &[],
        doc: "Show the working directory",
        fun: pwd,
    },
    TypedCommand {
        name: "cd",
        aliases: &[],
        doc: "Change the working directory",
        fun: cd,
    },
    TypedCommand {
        name: "recover",
        aliases: &[],
        doc: "Load unsaved changes from the recovery file",
        fun: recover,
    },
    TypedCommand {
        name: "recover-discard",
        aliases: &[],
        doc: "Delete the recovery file for this buffer",
        fun: recover_discard,
    },
    TypedCommand {
        name: "focus",
        aliases: &[],
        doc: "Focus mode: text only",
        fun: focus,
    },
    TypedCommand {
        name: "engineering",
        aliases: &[],
        doc: "Engineering mode: the full editing interface",
        fun: engineering,
    },
    TypedCommand {
        name: "swarm",
        aliases: &[],
        doc: "Swarm mode: agents and worktrees (panels not built yet)",
        fun: swarm,
    },
    TypedCommand {
        name: "grep",
        aliases: &[],
        doc: "Search the workspace for a regex (smart case, skips ignored and binary files)",
        fun: crate::navigation::grep::typed,
    },
    TypedCommand {
        name: "commands",
        aliases: &["help", "h"],
        doc: "Open the command palette",
        fun: commands,
    },
    TypedCommand {
        name: "lsp-restart",
        aliases: &[],
        doc: "Restart language servers (also after a crash)",
        fun: lsp_restart,
    },
    TypedCommand {
        name: "lsp-log",
        aliases: &[],
        doc: "Show language server logs in a scratch buffer",
        fun: lsp_log,
    },
    TypedCommand {
        name: "rename",
        aliases: &[],
        doc: "Rename the symbol under the cursor (prompts when no name is given)",
        fun: rename,
    },
    TypedCommand {
        name: "format",
        aliases: &["fmt"],
        doc: "Format the buffer with the language server",
        fun: format,
    },
    TypedCommand {
        name: "format-selection",
        aliases: &[],
        doc: "Format the primary selection with the language server",
        fun: format_selection,
    },
    TypedCommand {
        name: "code-action",
        aliases: &[],
        doc: "Pick a code action for the cursor or selection",
        fun: code_action,
    },
];

/// Commands whose argument is a single path that may contain spaces.
const PATH_COMMANDS: &[&str] = &["write", "write-quit", "open", "split", "vsplit", "cd"];

fn lsp_restart(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    crate::lsp::restart(ed);
    Ok(())
}

fn lsp_log(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    crate::lsp::show_log(ed);
    Ok(())
}

fn rename(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    match args {
        [] => crate::lsp::actions::rename(ed, None),
        [name] => crate::lsp::actions::rename(ed, Some(name)),
        _ => return Err("usage: :rename [new-name]".into()),
    }
    Ok(())
}

fn format(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    crate::lsp::actions::format(ed, false);
    Ok(())
}

fn format_selection(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    crate::lsp::actions::format(ed, true);
    Ok(())
}

fn code_action(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    crate::lsp::actions::code_actions(ed);
    Ok(())
}

pub fn all() -> &'static [TypedCommand] {
    TYPED
}

pub fn lookup(name: &str) -> Option<&'static TypedCommand> {
    TYPED
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// Runs a typed command by name with no arguments (used by keymaps).
pub fn execute(editor: &mut Editor, name: &str) {
    execute_line(editor, name);
}

/// Parses and runs a `:` command line.
pub fn execute_line(editor: &mut Editor, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if line.parse::<usize>().is_ok() {
        if let Err(e) = goto(editor, &[line], false) {
            editor.error(e);
        }
        return;
    }
    // :s/a/b/ and :%s/a/b/ take the rest of the line verbatim.
    let (whole, rest) = match line.strip_prefix('%') {
        Some(r) => (true, r),
        None => (false, line),
    };
    if let Some(spec) = rest.strip_prefix('s').filter(|r| {
        r.chars()
            .next()
            .is_some_and(|c| !c.is_alphanumeric() && !c.is_whitespace())
    }) {
        if let Err(e) = substitute(editor, spec, whole) {
            editor.error(e);
        }
        return;
    }
    // Structural pipeline steps take the rest of the line (queries and
    // regexes have spaces) and chain with `|`.
    if crate::structural::handles(line) {
        crate::structural::execute(editor, line);
        return;
    }
    // :grep takes the rest of the line as its pattern, spaces included.
    if let Some(pattern) = line.strip_prefix("grep ") {
        if let Err(e) = crate::navigation::grep::start(editor, pattern.trim_start()) {
            editor.error(e);
        }
        return;
    }
    let mut parts = line.split_whitespace();
    let head = parts.next().unwrap_or_default();
    let (name, force) = match head.strip_suffix('!') {
        Some(n) => (n, true),
        None => (head, false),
    };
    let mut args: Vec<&str> = parts.collect();
    match lookup(name) {
        Some(cmd) => {
            // Path arguments keep their spaces: `:w my file.rs` names one file.
            if PATH_COMMANDS.contains(&cmd.name) {
                let rest = line[head.len()..].trim();
                args = if rest.is_empty() { vec![] } else { vec![rest] };
            }
            if let Err(e) = (cmd.fun)(editor, &args, force) {
                editor.error(e);
            }
        }
        // Registry commands also answer to their name with dashes
        // (`:git-status` runs `git_status`).
        None => match crate::commands::lookup(name)
            .or_else(|| crate::commands::lookup(&name.replace('-', "_")))
        {
            Some(cmd) => (cmd.fun)(editor),
            None => editor.error(format!("unknown command: {name}")),
        },
    }
}

fn save_current(editor: &mut Editor, path: Option<&str>, force: bool) -> Result<(), String> {
    if let Some(p) = path {
        let target = editor.resolve_path(Path::new(p));
        let same = editor.doc().path().is_some_and(|cur| {
            crate::editor::normalize_path(cur) == crate::editor::normalize_path(&target)
        });
        if !same {
            if target.exists() && !force {
                return Err(format!("{} exists (add ! to overwrite)", target.display()));
            }
            if editor.find_buffer_by_path(&target).is_some() {
                return Err(format!("{} is open in another buffer", target.display()));
            }
            editor.buffer_mut().doc.set_path(target);
            editor.refresh_syntax(editor.view().doc);
        }
    }
    let (view, buf) = editor.current();
    let sel = view.selection.clone();
    let old_path: Option<PathBuf> = buf.doc.path().map(Path::to_path_buf);
    match buf.doc.save(&sel, force) {
        Ok(()) => {
            buf.disk_change_reported = false;
            buf.recovery_version = None;
            let name = buf.doc.display_name();
            let lines = crate::motion::last_line(buf.doc.text().slice(..)) + 1;
            let bytes = buf.doc.text().len_bytes();
            if let (Some(dir), Some(p)) = (editor.recovery_dir.clone(), old_path) {
                crate::recovery::remove(&dir, &p, &editor.session_id);
            }
            let id = editor.view().doc;
            // A new name means a new document for the server.
            crate::lsp::reconcile(editor);
            crate::lsp::document_saved(editor, id);
            editor.info(format!("wrote {name}: {lines} lines, {bytes} bytes"));
            Ok(())
        }
        Err(SaveError::NoPath) => Err("no file name; use :w <path>".into()),
        Err(e) => Err(e.to_string()),
    }
}

fn write(ed: &mut Editor, args: &[&str], force: bool) -> Result<(), String> {
    save_current(ed, args.first().copied(), force)
}

fn write_quit(ed: &mut Editor, args: &[&str], force: bool) -> Result<(), String> {
    save_current(ed, args.first().copied(), force)?;
    quit(ed, &[], false)
}

fn write_all(ed: &mut Editor, _: &[&str], force: bool) -> Result<(), String> {
    let ids: Vec<_> = ed
        .buffers
        .values()
        .filter(|b| b.doc.is_modified() && b.doc.path().is_some())
        .map(|b| b.id)
        .collect();
    let mut errors = Vec::new();
    for id in &ids {
        let sel = ed
            .views
            .values()
            .find(|v| v.doc == *id)
            .map(|v| v.selection.clone())
            .unwrap_or_else(|| ed.buffers[id].last_selection.clone());
        let dir = ed.recovery_dir.clone();
        let session = ed.session_id.clone();
        let buf = ed.buffers.get_mut(id).expect("buffer exists");
        match buf.doc.save(&sel, force) {
            Ok(()) => {
                buf.recovery_version = None;
                if let (Some(dir), Some(p)) = (dir, buf.doc.path()) {
                    crate::recovery::remove(&dir, p, &session);
                }
                crate::lsp::document_saved(ed, *id);
            }
            Err(e) => errors.push(format!("{}: {e}", buf.doc.display_name())),
        }
    }
    let unnamed = ed
        .buffers
        .values()
        .filter(|b| b.doc.is_modified() && b.doc.path().is_none())
        .count();
    if unnamed > 0 {
        errors.push(format!("{unnamed} unnamed buffer(s) not saved"));
    }
    if errors.is_empty() {
        ed.info(format!("wrote {} buffer(s)", ids.len()));
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn quit(ed: &mut Editor, _: &[&str], force: bool) -> Result<(), String> {
    if ed.layout.views().len() > 1 {
        ed.close_view();
        return Ok(());
    }
    quit_all(ed, &[], force)
}

fn quit_all(ed: &mut Editor, _: &[&str], force: bool) -> Result<(), String> {
    let modified = ed.modified_buffers();
    if !modified.is_empty() && !force {
        return Err(format!(
            "unsaved changes in {} (:wa saves, :qa! discards)",
            modified.join(", ")
        ));
    }
    // Git work for an agent (a worktree being made, an integration, an
    // archive) must not be cut off half done, even with `!`.
    let busy = ed.agents.git_busy();
    if !busy.is_empty() {
        return Err(format!(
            "wait: Git work is running for {}; quitting now could leave it half done",
            busy.join(", ")
        ));
    }
    // Running agents are asked about even with `!`, which only discards
    // unsaved buffers.
    let running = ed.agents.running().len();
    if running > 0 {
        ed.on_next_char(
            &format!("{running} agent(s) running; stop them and quit? y/n"),
            |ed, c| {
                if c == 'y' {
                    crate::agents::stop_all(ed);
                    finish_quit(ed);
                } else {
                    ed.info("not quitting; the agents keep running");
                }
            },
        );
        return Ok(());
    }
    finish_quit(ed);
    Ok(())
}

fn finish_quit(ed: &mut Editor) {
    crate::proposals::stop_all(ed);
    // Best effort: a session that cannot be written must not block quitting.
    let _ = crate::navigation::session::save(ed);
    crate::agents::persist::save(ed, true);
    ed.should_quit = true;
}

fn agents(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.run_command("agent_panel");
    Ok(())
}

fn proposals(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.run_command("proposal_review");
    Ok(())
}

fn write_quit_all(ed: &mut Editor, _: &[&str], force: bool) -> Result<(), String> {
    write_all(ed, &[], force)?;
    quit_all(ed, &[], false)
}

fn open(ed: &mut Editor, args: &[&str], force: bool) -> Result<(), String> {
    if args.is_empty() {
        if force {
            return reload(ed, &[], true);
        }
        return Err("usage: :open <path>".into());
    }
    ed.open(Path::new(args[0]))?;
    Ok(())
}

fn reload(ed: &mut Editor, _: &[&str], force: bool) -> Result<(), String> {
    if ed.doc().is_modified() && !force {
        return Err("buffer has unsaved changes (:reload! discards them)".into());
    }
    let doc_id = ed.view().doc;
    let (view, buf) = ed.current();
    let sel = view.selection.clone();
    let old = buf.doc.text().clone();
    let changes = buf.doc.reload(&sel).map_err(|e| e.to_string())?;
    buf.disk_change_reported = false;
    let text = buf.doc.text().clone();
    ed.on_text_changed(doc_id, &old, std::slice::from_ref(&changes));
    for v in ed.views.values_mut().filter(|v| v.doc == doc_id) {
        v.selection = v.selection.map(&changes).clamp_normal(text.slice(..));
    }
    crate::lsp::document_changed(ed, doc_id, &[changes]);
    ed.info("reloaded from disk (u undoes the reload)");
    Ok(())
}

fn new_buffer(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    let id = ed.add_document(maestro_core::Document::default());
    ed.switch_to(id);
    Ok(())
}

fn buffer_close(ed: &mut Editor, _: &[&str], force: bool) -> Result<(), String> {
    let id = ed.view().doc;
    ed.close_buffer(id, force)
}

fn buffer_next(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.run_command("buffer_next");
    Ok(())
}

fn buffer_prev(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.run_command("buffer_prev");
    Ok(())
}

fn buffers(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.run_command("buffer_picker");
    Ok(())
}

fn split_impl(ed: &mut Editor, args: &[&str], dir: Direction) -> Result<(), String> {
    ed.split(dir);
    if let Some(p) = args.first() {
        ed.open(Path::new(p))?;
    }
    Ok(())
}

fn split(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    split_impl(ed, args, Direction::Horizontal)
}

fn vsplit(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    split_impl(ed, args, Direction::Vertical)
}

fn close(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    if ed.close_view() {
        Ok(())
    } else {
        Err("cannot close the last view; use :q".into())
    }
}

fn only(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.only_view();
    Ok(())
}

fn set(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    use maestro_config::{IndentStyle, LineNumbers};
    if args.is_empty() {
        let e = &ed.config.editor;
        let msg = format!(
            "tab-width={} indent={:?} indent-width={} line-numbers={:?} scrolloff={} cursorline={}",
            e.tab_width, e.indent, e.indent_width, e.line_numbers, e.scrolloff, e.cursorline
        );
        ed.info(msg);
        return Ok(());
    }
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("expected key=value, got {arg:?}"))?;
        let num = || {
            value
                .parse::<usize>()
                .map_err(|_| format!("{key} needs a number"))
        };
        let e = &mut ed.config.editor;
        match key {
            "tab-width" => e.tab_width = num()?.clamp(1, 16),
            "indent-width" => e.indent_width = num()?.clamp(1, 16),
            "scrolloff" => e.scrolloff = num()?,
            "indent" => {
                e.indent = match value {
                    "tabs" => IndentStyle::Tabs,
                    "spaces" => IndentStyle::Spaces,
                    _ => return Err("indent is tabs or spaces".into()),
                }
            }
            "line-numbers" => {
                e.line_numbers = match value {
                    "absolute" => LineNumbers::Absolute,
                    "relative" => LineNumbers::Relative,
                    "off" => LineNumbers::Off,
                    _ => return Err("line-numbers is absolute, relative or off".into()),
                }
            }
            "cursorline" => {
                e.cursorline = value
                    .parse()
                    .map_err(|_| "cursorline is true or false".to_string())?
            }
            _ => return Err(format!("unknown option {key}")),
        }
    }
    ed.relayout();
    Ok(())
}

fn noh(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    ed.last_search = None;
    Ok(())
}

fn substitute_cmd(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    let spec = args.join(" ");
    substitute(ed, &spec, false)
}

/// Converts Vim-style `\1` group references to the regex crate's `${1}`.
fn convert_replacement(rep: &str) -> String {
    let mut out = String::with_capacity(rep.len());
    let mut chars = rep.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(d)) if d.is_ascii_digit() => {
                out.push_str(&format!("${{{d}}}"));
                chars.next();
            }
            ('\\', Some('n')) => {
                out.push('\n');
                chars.next();
            }
            ('\\', Some('t')) => {
                out.push('\t');
                chars.next();
            }
            ('\\', Some(&d)) => {
                out.push(d);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// Splits `/pat/rep/flags` on its delimiter, honoring `\` escapes of it.
fn split_spec(spec: &str) -> Result<(String, String, String), String> {
    let mut chars = spec.chars();
    let delim = chars.next().ok_or("usage: :s/pattern/replacement/[gi]")?;
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in chars {
        if escaped {
            if c != delim {
                parts.last_mut().expect("non-empty").push('\\');
            }
            parts.last_mut().expect("non-empty").push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == delim && parts.len() < 3 {
            parts.push(String::new());
        } else {
            parts.last_mut().expect("non-empty").push(c);
        }
    }
    if escaped {
        parts.last_mut().expect("non-empty").push('\\');
    }
    if parts.len() < 2 {
        return Err("usage: :s/pattern/replacement/[gi]".into());
    }
    let flags = parts.get(2).cloned().unwrap_or_default();
    Ok((parts[0].clone(), parts[1].clone(), flags))
}

pub fn substitute(ed: &mut Editor, spec: &str, whole: bool) -> Result<(), String> {
    let (pattern, rep, flags) = split_spec(spec)?;
    let pattern = if pattern.is_empty() {
        ed.last_search.clone().ok_or("no previous pattern")?
    } else {
        pattern
    };
    if let Some(bad) = flags.chars().find(|c| !matches!(c, 'g' | 'i' | 'I')) {
        return Err(format!("unsupported flag {bad:?} (supported: g, i, I)"));
    }
    let global = flags.contains('g');
    let mut builder = regex::RegexBuilder::new(&pattern);
    builder.multi_line(true);
    if flags.contains('i') {
        builder.case_insensitive(true);
    } else if !flags.contains('I') {
        builder.case_insensitive(!pattern.chars().any(char::is_uppercase));
    }
    let re = builder.build().map_err(|e| format!("invalid regex: {e}"))?;
    let rep = convert_replacement(&rep);

    let text = ed.doc().text().clone();
    let s = text.slice(..);
    let line_spans: Vec<(usize, usize)> = if whole {
        vec![(0, crate::motion::last_line(s))]
    } else {
        let mut v: Vec<(usize, usize)> = ed
            .view()
            .selection
            .iter()
            .map(|r| (s.char_to_line(r.from()), s.char_to_line(r.last())))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut changes = Vec::new();
    let mut lines_touched = std::collections::BTreeSet::new();
    let mut last_end = 0;
    for (first, last) in line_spans {
        for line in first..=last {
            let start = s.line_to_char(line);
            let end = maestro_core::text::line_end_excluding_terminator(s, line);
            if start < last_end {
                continue;
            }
            let hay = s.slice(start..end).to_string();
            let base = text.char_to_byte(start);
            for caps in re.captures_iter(&hay) {
                let m = caps.get(0).expect("group 0 always exists");
                let mut replaced = String::new();
                caps.expand(&rep, &mut replaced);
                let from = text.byte_to_char(base + m.start());
                let to = text.byte_to_char(base + m.end());
                changes.push(Change::replace(from, to, replaced));
                lines_touched.insert(line);
                if !global {
                    break;
                }
            }
            last_end = end;
        }
    }
    if changes.is_empty() {
        return Err(format!("pattern not found: {pattern}"));
    }
    let n = changes.len();
    let cs = ChangeSet::from_changes(text.len_chars(), changes).map_err(|e| e.to_string())?;
    if !ed.apply_or_report(Transaction::new(cs)) {
        return Ok(());
    }
    ed.last_search = Some(pattern);
    ed.info(format!(
        "{n} substitution{} on {} line{}",
        if n == 1 { "" } else { "s" },
        lines_touched.len(),
        if lines_touched.len() == 1 { "" } else { "s" }
    ));
    Ok(())
}

fn goto(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    let n: usize = args
        .first()
        .ok_or("usage: :goto <line>")?
        .parse()
        .map_err(|_| "line must be a number".to_string())?;
    ed.push_jump();
    let text = ed.doc().text().clone();
    let p = crate::motion::goto_line(text.slice(..), n.saturating_sub(1));
    ed.set_selection(maestro_core::Selection::point(p));
    Ok(())
}

fn pwd(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    let msg = ed.cwd.display().to_string();
    ed.info(msg);
    Ok(())
}

fn cd(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
    let target = ed.resolve_path(Path::new(args.first().copied().unwrap_or("~")));
    let target =
        std::fs::canonicalize(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    if !target.is_dir() {
        return Err(format!("{} is not a directory", target.display()));
    }
    ed.cwd = target;
    let msg = format!("cwd: {}", ed.cwd.display());
    ed.info(msg);
    Ok(())
}

fn recover(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    let dir = ed.recovery_dir.clone().ok_or("recovery is disabled")?;
    let path = ed.doc().path().ok_or("buffer has no file")?.to_path_buf();
    let (foreign, content) = crate::recovery::newest_foreign(&dir, &path, &ed.session_id)
        .ok_or("no recovery file from another session for this buffer")?;
    let len = ed.doc().text().len_chars();
    let cs = ChangeSet::from_changes(len, [Change::replace(0, len, content)])
        .map_err(|e| e.to_string())?;
    if ed.apply_or_report(Transaction::new(cs)) {
        // Journal the recovered text under this session before deleting the
        // other session's file, so a crash in between loses nothing.
        let session = ed.session_id.clone();
        let text = ed.doc().text().clone();
        crate::recovery::write(&dir, &path, &session, &text)
            .map_err(|e| format!("recovered, but could not journal it: {e}"))?;
        ed.buffer_mut().recovery_version = Some(ed.doc().version());
        let _ = std::fs::remove_file(foreign);
        ed.set_status(
            Severity::Info,
            "recovered unsaved changes; review, then :w (u undoes the recovery)",
        );
    }
    Ok(())
}

fn recover_discard(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    let dir = ed.recovery_dir.clone().ok_or("recovery is disabled")?;
    let path = ed.doc().path().ok_or("buffer has no file")?.to_path_buf();
    let n = crate::recovery::remove_all(&dir, &path);
    ed.info(format!("deleted {n} recovery file(s) for this buffer"));
    Ok(())
}

fn set_presentation(ed: &mut Editor, p: Presentation) -> Result<(), String> {
    ed.presentation = p;
    ed.relayout();
    Ok(())
}

fn focus(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    set_presentation(ed, Presentation::Focus)
}

fn engineering(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    set_presentation(ed, Presentation::Engineering)
}

fn swarm(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    set_presentation(ed, Presentation::Swarm)?;
    ed.set_status(
        Severity::Warning,
        "swarm mode looks like engineering mode for now; the agent panel is space a or :agents",
    );
    Ok(())
}

fn commands(ed: &mut Editor, _: &[&str], _: bool) -> Result<(), String> {
    if ed.mode == Mode::Insert {
        ed.mode = Mode::Normal;
    }
    ed.run_command("command_palette");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_splitting_and_replacement_conversion() {
        assert_eq!(
            split_spec("/a\\/b/c/g").unwrap(),
            ("a/b".into(), "c".into(), "g".into())
        );
        assert_eq!(
            split_spec("#x#y").unwrap(),
            ("x".into(), "y".into(), "".into())
        );
        assert_eq!(convert_replacement("\\1-\\2\\n"), "${1}-${2}\n");
        assert!(split_spec("/only").is_err());
    }
}
