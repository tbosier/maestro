//! The command registry.
//!
//! Every editing action is a named command with a one-line description.
//! Keymaps, the command palette and (later) the automation API all invoke
//! commands by these names, so there is one implementation per action.

use maestro_core::text::{
    CharClass, char_class, is_line_ending_char, line_end_excluding_terminator, line_indent,
    next_grapheme_boundary, prev_grapheme_boundary,
};
use maestro_core::{Change, ChangeSet, Range, Rope, RopeSlice, Selection, Transaction};

use crate::editor::{Editor, Mode, Presentation, Severity};
use crate::key::parse_sequence;
use crate::layout::Direction;
use crate::motion::{self, ObjectKind};
use crate::picker::{Picker, PickerAction, PickerItem};
use crate::prompt::{self, PromptKind};
use crate::registers::{self, is_clipboard};

pub type CommandFn = fn(&mut Editor);

pub struct Command {
    pub name: &'static str,
    pub doc: &'static str,
    pub fun: CommandFn,
}

/// Declares a `COMMANDS` table in the calling module. Workstreams put their
/// commands in their own module with their own table and add it to
/// [`TABLES`], so the registry grows without everyone editing one list.
#[macro_export]
macro_rules! commands {
    ($($name:ident => $doc:literal,)*) => {
        pub static COMMANDS: &[$crate::commands::Command] = &[
            $($crate::commands::Command { name: stringify!($name), doc: $doc, fun: $name },)*
        ];
    };
}

/// Every command table. Add new modules' tables here.
pub static TABLES: &[&[Command]] = &[
    COMMANDS,
    crate::git::COMMANDS,
    crate::git_screen::COMMANDS,
    crate::git_hunks::COMMANDS,
    crate::agents::COMMANDS,
    crate::proposals::COMMANDS,
    crate::lsp_commands::COMMANDS,
    crate::navigation::COMMANDS,
    crate::syntax_commands::COMMANDS,
];

/// Iterates every registered command.
pub fn all() -> impl Iterator<Item = &'static Command> {
    TABLES.iter().flat_map(|t| t.iter())
}

commands! {
    move_char_left => "Move left",
    move_char_right => "Move right",
    move_line_up => "Move up a line",
    move_line_down => "Move down a line",
    select_next_word_start => "Select to the start of the next word",
    select_prev_word_start => "Select to the start of the previous word",
    select_next_word_end => "Select to the end of the next word",
    select_next_long_word_start => "Select to the start of the next WORD",
    select_prev_long_word_start => "Select to the start of the previous WORD",
    select_next_long_word_end => "Select to the end of the next WORD",
    goto_line_start => "Go to the start of the line",
    goto_first_nonblank => "Go to the first non-blank character of the line",
    goto_line_end => "Go to the end of the line",
    goto_line_end_insert => "Go past the last character of the line",
    goto_file_start => "Go to the first line, or line N with a count",
    goto_last_line => "Go to the last line, or line N with a count",
    goto_window_top => "Go to the top of the screen",
    goto_window_center => "Go to the middle of the screen",
    goto_window_bottom => "Go to the bottom of the screen",
    goto_alternate_file => "Switch to the previously shown file",
    find_next_char => "Select to the next occurrence of a character",
    till_next_char => "Select until the next occurrence of a character",
    find_prev_char => "Select back to the previous occurrence of a character",
    till_prev_char => "Select back until the previous occurrence of a character",
    repeat_last_find => "Repeat the last f/t/F/T",
    half_page_down => "Move half a page down",
    half_page_up => "Move half a page up",
    page_down => "Move a page down",
    page_up => "Move a page up",
    scroll_down => "Scroll the view down one line",
    scroll_up => "Scroll the view up one line",
    align_view_center => "Scroll so the cursor line is centered",
    align_view_top => "Scroll so the cursor line is at the top",
    align_view_bottom => "Scroll so the cursor line is at the bottom",
    jump_backward => "Jump back in the jump list",
    jump_forward => "Jump forward in the jump list",
    extend_line_below => "Select the line, or extend the line selection downward",
    extend_to_line_bounds => "Extend selections to whole lines",
    select_all => "Select the whole document",
    collapse_selection => "Collapse selections to their cursors",
    flip_selections => "Swap anchor and cursor of each selection",
    keep_primary_selection => "Keep only the primary selection",
    remove_primary_selection => "Remove the primary selection",
    rotate_selections_forward => "Make the next selection primary",
    rotate_selections_backward => "Make the previous selection primary",
    copy_selection_on_next_line => "Add a cursor on the next line",
    copy_selection_on_prev_line => "Add a cursor on the previous line",
    select_regex => "Select regex matches inside the selections",
    split_selection => "Split selections on a regex",
    split_selection_on_newline => "Split selections into lines",
    keep_selections => "Keep selections that match a regex",
    remove_selections => "Remove selections that match a regex",
    trim_selections => "Trim whitespace from the ends of selections",
    select_mode => "Enter select mode: motions extend selections",
    select_line_mode => "Select whole lines and enter select mode",
    search => "Search forward",
    rsearch => "Search backward",
    search_next => "Select the next search match",
    search_prev => "Select the previous search match",
    search_selection => "Use the primary selection as the search pattern",
    match_brackets => "Jump to the matching bracket",
    surround_add => "Surround selections with a pair",
    surround_replace => "Replace the surrounding pair",
    surround_delete => "Delete the surrounding pair",
    select_textobject_inner => "Select inside a text object (w, W, p, or a bracket/quote)",
    select_textobject_around => "Select around a text object",
    insert_mode => "Insert before the selection",
    append_mode => "Insert after the selection",
    insert_at_line_start => "Insert at the first non-blank of the line",
    insert_at_line_end => "Insert at the end of the line",
    open_below => "Open a new line below",
    open_above => "Open a new line above",
    normal_mode => "Return to normal mode",
    command_mode => "Enter a : command",
    delete_selection => "Delete the selection (yanks it)",
    delete_selection_noyank => "Delete the selection without yanking",
    change_selection => "Delete the selection and insert (yanks it)",
    change_selection_noyank => "Delete the selection and insert without yanking",
    delete_char_forward => "Delete the character under the cursor",
    delete_char_backward => "Delete the character before the cursor",
    delete_word_backward => "Delete the word before the cursor",
    kill_to_line_start => "Delete to the start of the line",
    delete_to_line_end => "Delete to the end of the line",
    delete_line => "Delete whole lines",
    yank_line => "Yank whole lines",
    change_line => "Replace the contents of the line",
    insert_newline => "Insert a line break with indentation",
    insert_tab => "Insert one level of indentation",
    yank => "Yank (copy) the selections",
    paste_after => "Paste after the selections",
    paste_before => "Paste before the selections",
    replace_with_yanked => "Replace selections with the yanked text",
    replace => "Replace each selected character with a typed character",
    yank_to_clipboard => "Yank selections to the system clipboard",
    paste_clipboard_after => "Paste the system clipboard after the selections",
    paste_clipboard_before => "Paste the system clipboard before the selections",
    replace_selections_with_clipboard => "Replace selections with the system clipboard",
    undo => "Undo",
    redo => "Redo",
    indent => "Indent the selected lines",
    unindent => "Unindent the selected lines",
    switch_case => "Toggle the case of the selections",
    switch_to_lowercase => "Lowercase the selections",
    switch_to_uppercase => "Uppercase the selections",
    join_selections => "Join lines",
    repeat_last_change => "Repeat the last change",
    earlier => "Go to the previous state, across undo branches",
    later => "Go to the next state, across undo branches",
    record_macro => "Start or stop recording a macro",
    replay_macro => "Replay a recorded macro",
    set_mark => "Set a mark at the cursor",
    goto_mark => "Jump to a mark",
    vsplit => "Split the view side by side",
    hsplit => "Split the view top and bottom",
    rotate_view => "Focus the next view",
    wclose => "Close the view",
    wonly => "Close every other view",
    jump_view_left => "Focus the view to the left",
    jump_view_right => "Focus the view to the right",
    jump_view_up => "Focus the view above",
    jump_view_down => "Focus the view below",
    buffer_next => "Show the next buffer",
    buffer_prev => "Show the previous buffer",
    buffer_picker => "Pick an open buffer",
    command_palette => "Search every command",
    toggle_focus_mode => "Toggle focus mode (text only)",
}

pub fn lookup(name: &str) -> Option<&'static Command> {
    all().find(|c| c.name == name)
}

// ----- helpers -------------------------------------------------------------

fn text_of(ed: &Editor) -> Rope {
    ed.doc().text().clone()
}

/// Applies `f` to every range of the focused view.
fn move_each(ed: &mut Editor, mut f: impl FnMut(RopeSlice, Range) -> Range) {
    let text = text_of(ed);
    let s = text.slice(..);
    let sel = ed.view().selection.transform(s, |r| f(s, r));
    ed.view_mut().selection = sel;
}

/// Moves each head to `f(head)`; extends in select mode, collapses otherwise.
fn move_heads(ed: &mut Editor, mut f: impl FnMut(RopeSlice, usize) -> usize) {
    let extend = ed.extending();
    move_each(ed, |s, r| r.put_head(f(s, r.head), extend));
}

/// For selection-producing motions: in select mode keep the old anchor.
fn select_each(ed: &mut Editor, mut f: impl FnMut(RopeSlice, Range) -> Range) {
    let extend = ed.extending();
    let count = ed.count_or(1);
    move_each(ed, |s, r| {
        let mut new = r;
        for _ in 0..count {
            new = f(s, new);
        }
        if extend {
            Range::new(r.anchor, new.head)
        } else {
            new
        }
    });
}

fn changes_from(text: &Rope, changes: Vec<Change>) -> ChangeSet {
    ChangeSet::from_changes_clipped(text.len_chars(), changes)
}

/// Applies changes and selects, for each insertion point, the text inserted
/// there (`select_inserted`) or puts a cursor after it.
fn apply_changes(ed: &mut Editor, changes: Vec<Change>, after: AfterEdit) {
    let text = text_of(ed);
    // Where each change lands after the same clipping `changes_from` does,
    // and how many chars it inserts.
    let mut last = 0;
    let anchors: Vec<(usize, usize)> = changes
        .iter()
        .map(|c| {
            let to = c.to.min(text.len_chars());
            let from = c.from.min(to).max(last);
            last = to.max(from);
            (from, c.text.as_deref().map_or(0, |t| t.chars().count()))
        })
        .collect();
    let cs = changes_from(&text, changes);
    let primary = ed.view().selection.primary_index();
    let ranges: Vec<Range> = anchors
        .iter()
        .map(|&(from, inserted)| {
            // Mapping `from` lands before a replacement's inserted text;
            // the inserted length gives the other end.
            let start = cs.map_pos(from, maestro_core::Assoc::Before);
            let end = start + inserted;
            match after {
                // Head at the start, so a Vim-style follow-up acts there.
                AfterEdit::SelectInserted if end > start => {
                    Range::new(prev_grapheme_boundary_in(&cs, end, start), start)
                }
                AfterEdit::SelectInserted | AfterEdit::CursorAtStart => Range::point(start),
                AfterEdit::CursorAfter => Range::point(end),
            }
        })
        .collect();
    let tx = if ranges.is_empty() {
        Transaction::new(cs)
    } else {
        Transaction::new(cs).with_selection(Selection::new(ranges, primary))
    };
    ed.apply_or_report(tx);
}

/// The char before `end` (inserted text is plain chars, so one char is safe
/// for selection purposes; renderers clamp to graphemes).
fn prev_grapheme_boundary_in(_cs: &ChangeSet, end: usize, start: usize) -> usize {
    end.saturating_sub(1).max(start)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterEdit {
    SelectInserted,
    CursorAtStart,
    CursorAfter,
}

fn register_name(ed: &Editor) -> char {
    ed.selected_register.unwrap_or(registers::DEFAULT)
}

fn read_register(ed: &mut Editor, name: char) -> Option<Vec<String>> {
    if is_clipboard(name) {
        return match ed.clipboard.get() {
            Ok(s) if !s.is_empty() => Some(vec![s]),
            Ok(_) => None,
            Err(e) => {
                ed.error(format!("clipboard: {e}"));
                None
            }
        };
    }
    ed.registers.get(name).map(<[String]>::to_vec)
}

fn write_register(ed: &mut Editor, name: char, values: Vec<String>) {
    if is_clipboard(name) {
        let joined = values.join(ed.doc().line_ending().as_str());
        if let Err(e) = ed.clipboard.set(&joined) {
            ed.error(format!("clipboard: {e}"));
        }
        return;
    }
    ed.registers.set(name, values);
}

fn yank_impl(ed: &mut Editor, name: char) {
    let text = text_of(ed);
    let s = text.slice(..);
    let values: Vec<String> = ed.view().selection.iter().map(|r| r.fragment(s)).collect();
    let n = values.len();
    write_register(ed, name, values);
    if ed.status.is_none() {
        let plural = if n == 1 { "" } else { "s" };
        ed.info(format!("yanked {n} selection{plural} to register {name}"));
    }
}

fn delete_impl(ed: &mut Editor, yank: bool) {
    if yank {
        let name = register_name(ed);
        let text = text_of(ed);
        let s = text.slice(..);
        let values = ed.view().selection.iter().map(|r| r.fragment(s)).collect();
        write_register(ed, name, values);
    }
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| Change::delete(r.from(), r.to(s)))
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
}

fn enter_insert(ed: &mut Editor, f: impl Fn(RopeSlice, Range) -> usize) {
    let text = text_of(ed);
    let s = text.slice(..);
    let sel = ed.view().selection.transform(s, |r| Range::point(f(s, r)));
    ed.view_mut().selection = sel;
    ed.mode = Mode::Insert;
}

// ----- movement --------------------------------------------------------------

fn move_char_left(ed: &mut Editor) {
    let n = ed.count_or(1);
    move_heads(ed, |s, h| motion::char_left(s, h, n));
}

fn move_char_right(ed: &mut Editor) {
    let n = ed.count_or(1);
    move_heads(ed, |s, h| motion::char_right(s, h, n));
}

fn move_vertical(ed: &mut Editor, delta: isize) {
    let tab = ed.tab_width();
    let extend = ed.extending();
    let text = text_of(ed);
    let s = text.slice(..);
    let sel = ed.view().selection.clone();
    let cols = ed
        .view_mut()
        .sticky_cols
        .take()
        .filter(|c| c.len() == sel.len())
        .unwrap_or_else(|| {
            sel.iter()
                .map(|r| motion::display_col(s, r.head, tab))
                .collect()
        });
    let ranges: Vec<Range> = sel
        .iter()
        .zip(&cols)
        .map(|(r, &col)| r.put_head(motion::line_vertical(s, r.head, delta, col, tab), extend))
        .collect();
    let new = Selection::new(ranges, sel.primary_index()).normalize(s);
    let keep_cols = new.len() == cols.len();
    let view = ed.view_mut();
    view.selection = new;
    view.sticky_cols = keep_cols.then_some(cols);
}

fn move_line_up(ed: &mut Editor) {
    let n = ed.count_or(1) as isize;
    move_vertical(ed, -n);
}

fn move_line_down(ed: &mut Editor) {
    let n = ed.count_or(1) as isize;
    move_vertical(ed, n);
}

fn select_next_word_start(ed: &mut Editor) {
    select_each(ed, |s, r| motion::next_word_start(s, r, false));
}
fn select_prev_word_start(ed: &mut Editor) {
    select_each(ed, |s, r| motion::prev_word_start(s, r, false));
}
fn select_next_word_end(ed: &mut Editor) {
    select_each(ed, |s, r| motion::next_word_end(s, r, false));
}
fn select_next_long_word_start(ed: &mut Editor) {
    select_each(ed, |s, r| motion::next_word_start(s, r, true));
}
fn select_prev_long_word_start(ed: &mut Editor) {
    select_each(ed, |s, r| motion::prev_word_start(s, r, true));
}
fn select_next_long_word_end(ed: &mut Editor) {
    select_each(ed, |s, r| motion::next_word_end(s, r, true));
}

fn goto_line_start(ed: &mut Editor) {
    move_heads(ed, motion::line_start);
}
fn goto_first_nonblank(ed: &mut Editor) {
    move_heads(ed, motion::first_non_blank);
}
fn goto_line_end(ed: &mut Editor) {
    if ed.mode == Mode::Insert {
        return goto_line_end_insert(ed);
    }
    move_heads(ed, motion::line_end);
}
fn goto_line_end_insert(ed: &mut Editor) {
    move_heads(ed, |s, h| {
        line_end_excluding_terminator(s, s.char_to_line(h))
    });
}

fn goto_line_impl(ed: &mut Editor, line: usize) {
    ed.push_jump();
    let text = text_of(ed);
    let p = motion::goto_line(text.slice(..), line);
    let extend = ed.extending();
    let sel = if extend {
        let r = ed.view().selection.primary();
        Selection::single(r.anchor, p)
    } else {
        Selection::point(p)
    };
    ed.set_selection(sel);
}

fn goto_file_start(ed: &mut Editor) {
    let line = ed.count.map(|c| c.saturating_sub(1)).unwrap_or(0);
    goto_line_impl(ed, line);
}

fn goto_last_line(ed: &mut Editor) {
    let text = text_of(ed);
    let line = match ed.count {
        Some(c) => c.saturating_sub(1),
        None => motion::last_line(text.slice(..)),
    };
    goto_line_impl(ed, line);
}

fn goto_window(ed: &mut Editor, which: u8) {
    let text = text_of(ed);
    let s = text.slice(..);
    let view = ed.view();
    let height = view.text_height().max(1);
    let last = motion::last_line(s);
    let bottom = (view.top_line + height - 1).min(last);
    let so = ed.config.editor.scrolloff.min(height / 2);
    let line = match which {
        0 => (view.top_line + so).min(bottom),
        1 => (view.top_line + bottom) / 2,
        _ => bottom.saturating_sub(so).max(view.top_line),
    };
    let p = motion::goto_line(s, line);
    move_heads(ed, |_, _| p);
}
fn goto_window_top(ed: &mut Editor) {
    goto_window(ed, 0);
}
fn goto_window_center(ed: &mut Editor) {
    goto_window(ed, 1);
}
fn goto_window_bottom(ed: &mut Editor) {
    goto_window(ed, 2);
}

fn goto_alternate_file(ed: &mut Editor) {
    match ed.view().alternate {
        Some(id) if ed.buffers.contains_key(&id) => ed.switch_to(id),
        _ => ed.error("no alternate file"),
    }
}

fn find_char_impl(ed: &mut Editor, forward: bool, till: bool) {
    let count = ed.count_or(1);
    let label = match (forward, till) {
        (true, false) => "find",
        (true, true) => "till",
        (false, false) => "find back",
        (false, true) => "till back",
    };
    ed.on_next_char(label, move |ed, c| {
        ed.last_find = Some((c, forward, till));
        do_find(ed, c, forward, till, count);
    });
}

fn do_find(ed: &mut Editor, c: char, forward: bool, till: bool, count: usize) {
    let extend = ed.extending();
    move_each(ed, |s, r| {
        match motion::find_char(s, r.head, c, forward, till, count) {
            Some(p) if extend => Range::new(r.anchor, p),
            // Selection-first: select from the cursor to the target.
            Some(p) => Range::new(r.head, p),
            None => r,
        }
    });
}

fn find_next_char(ed: &mut Editor) {
    find_char_impl(ed, true, false);
}
fn till_next_char(ed: &mut Editor) {
    find_char_impl(ed, true, true);
}
fn find_prev_char(ed: &mut Editor) {
    find_char_impl(ed, false, false);
}
fn till_prev_char(ed: &mut Editor) {
    find_char_impl(ed, false, true);
}
fn repeat_last_find(ed: &mut Editor) {
    if let Some((c, f, t)) = ed.last_find {
        let n = ed.count_or(1);
        do_find(ed, c, f, t, n);
    }
}

fn page_move(ed: &mut Editor, fraction: f32, down: bool) {
    let height = ed.view().text_height().max(1);
    let n = ((height as f32 * fraction) as usize).max(1) * ed.count_or(1);
    let delta = if down { n as isize } else { -(n as isize) };
    let text = text_of(ed);
    let last = motion::last_line(text.slice(..));
    let view = ed.view_mut();
    view.top_line = (view.top_line as isize + delta).clamp(0, last as isize) as usize;
    let saved = ed.count.take();
    move_vertical(ed, delta);
    ed.count = saved;
}

fn half_page_down(ed: &mut Editor) {
    page_move(ed, 0.5, true);
}
fn half_page_up(ed: &mut Editor) {
    page_move(ed, 0.5, false);
}
fn page_down(ed: &mut Editor) {
    page_move(ed, 1.0, true);
}
fn page_up(ed: &mut Editor) {
    page_move(ed, 1.0, false);
}
fn scroll_down(ed: &mut Editor) {
    let n = ed.count_or(1) as isize;
    ed.scroll(n);
}
fn scroll_up(ed: &mut Editor) {
    let n = ed.count_or(1) as isize;
    ed.scroll(-n);
}

fn align_view(ed: &mut Editor, which: u8) {
    let text = text_of(ed);
    let s = text.slice(..);
    let line = s.char_to_line(ed.view().selection.primary().head.min(s.len_chars()));
    let height = ed.view().text_height().max(1);
    let so = ed.config.editor.scrolloff.min(height.saturating_sub(1) / 2);
    let top = match which {
        0 => line.saturating_sub(so),
        1 => line.saturating_sub(height / 2),
        _ => (line + so + 1).saturating_sub(height),
    };
    ed.view_mut().top_line = top;
}
pub fn align_view_center(ed: &mut Editor) {
    align_view(ed, 1);
}
fn align_view_top(ed: &mut Editor) {
    align_view(ed, 0);
}
fn align_view_bottom(ed: &mut Editor) {
    align_view(ed, 2);
}

fn jump_backward(ed: &mut Editor) {
    ed.jump(true);
}
fn jump_forward(ed: &mut Editor) {
    ed.jump(false);
}

// ----- selections -------------------------------------------------------------

fn extend_line_below(ed: &mut Editor) {
    let count = ed.count_or(1);
    move_each(ed, |s, r| {
        let lr = motion::line_range(s, r);
        let mut r = if lr.from() == r.from() && lr.last() == r.last() {
            // Already whole lines: grow by one line.
            let next_line = s.char_to_line(r.last()) + 1;
            if next_line > motion::last_line(s) {
                return lr;
            }
            motion::line_range(s, Range::new(r.from(), s.line_to_char(next_line)))
        } else {
            lr
        };
        for _ in 1..count {
            let next_line = s.char_to_line(r.last()) + 1;
            if next_line > motion::last_line(s) {
                break;
            }
            r = motion::line_range(s, Range::new(r.from(), s.line_to_char(next_line)));
        }
        r
    });
}

fn extend_to_line_bounds(ed: &mut Editor) {
    move_each(ed, |s, r| {
        let lr = motion::line_range(s, r);
        if r.is_forward() { lr } else { lr.flip() }
    });
}

fn select_all(ed: &mut Editor) {
    let text = text_of(ed);
    let len = text.len_chars();
    let end = if len == 0 {
        0
    } else {
        prev_grapheme_boundary(text.slice(..), len)
    };
    ed.set_selection(Selection::single(0, end));
}

fn collapse_selection(ed: &mut Editor) {
    move_each(ed, |_, r| r.collapse());
}

fn flip_selections(ed: &mut Editor) {
    move_each(ed, |_, r| r.flip());
}

fn keep_primary_selection(ed: &mut Editor) {
    let sel = ed.view().selection.keep_primary();
    ed.view_mut().selection = sel;
}

fn remove_primary_selection(ed: &mut Editor) {
    let idx = ed.view().selection.primary_index();
    match ed.view().selection.remove(idx) {
        Some(sel) => ed.view_mut().selection = sel,
        None => ed.error("cannot remove the only selection"),
    }
}

fn rotate_selections(ed: &mut Editor, delta: isize) {
    let n = ed.view().selection.len() as isize;
    let i = ed.view().selection.primary_index() as isize;
    let next = (i + delta).rem_euclid(n) as usize;
    ed.view_mut().selection.set_primary_index(next);
}
fn rotate_selections_forward(ed: &mut Editor) {
    rotate_selections(ed, 1);
}
fn rotate_selections_backward(ed: &mut Editor) {
    rotate_selections(ed, -1);
}

fn copy_selection_on_line(ed: &mut Editor, delta: isize) {
    let tab = ed.tab_width();
    let text = text_of(ed);
    let s = text.slice(..);
    let count = ed.count_or(1);
    let mut sel = ed.view().selection.clone();
    for _ in 0..count {
        let mut added = None;
        for r in sel.ranges().to_vec() {
            let (al, hl) = (
                s.char_to_line(r.anchor) as isize,
                s.char_to_line(r.head) as isize,
            );
            let mut offset = delta * ((al - hl).abs() + 1);
            // Skip lines too short to hold the copy (like Kakoune/Helix).
            loop {
                let (na, nh) = (al + offset, hl + offset);
                let last = motion::last_line(s) as isize;
                if na < 0 || nh < 0 || na > last || nh > last {
                    break;
                }
                let ac = motion::display_col(s, r.anchor, tab);
                let hc = motion::display_col(s, r.head, tab);
                let a = maestro_core::pos::line_col_to_char(
                    s,
                    maestro_core::pos::LineCol {
                        line: na as usize,
                        col: ac,
                    },
                    tab,
                );
                let h = maestro_core::pos::line_col_to_char(
                    s,
                    maestro_core::pos::LineCol {
                        line: nh as usize,
                        col: hc,
                    },
                    tab,
                );
                if motion::display_col(s, a, tab) == ac && motion::display_col(s, h, tab) == hc {
                    added = Some(Range::new(a, h));
                    sel = sel.push(Range::new(a, h), s);
                    break;
                }
                offset += delta;
            }
        }
        if added.is_none() {
            break;
        }
    }
    ed.view_mut().selection = sel;
}
fn copy_selection_on_next_line(ed: &mut Editor) {
    copy_selection_on_line(ed, 1);
}
fn copy_selection_on_prev_line(ed: &mut Editor) {
    copy_selection_on_line(ed, -1);
}

fn select_regex(ed: &mut Editor) {
    prompt::open(ed, PromptKind::SelectRegex);
}
fn split_selection(ed: &mut Editor) {
    prompt::open(ed, PromptKind::SplitRegex);
}
fn keep_selections(ed: &mut Editor) {
    prompt::open(ed, PromptKind::Keep { remove: false });
}
fn remove_selections(ed: &mut Editor) {
    prompt::open(ed, PromptKind::Keep { remove: true });
}

fn split_selection_on_newline(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let sel = ed.view().selection.transform_many(s, |r| {
        let (from, to) = (r.from(), r.to(s));
        let mut out = Vec::new();
        let mut start = from;
        let mut i = from;
        while i < to {
            if is_line_ending_char(s.char(i)) {
                if i > start {
                    out.push(Range::new(start, prev_grapheme_boundary(s, i)));
                }
                start = next_grapheme_boundary(s, i);
                i = start;
            } else {
                i += 1;
            }
        }
        if start < to {
            out.push(Range::new(start, prev_grapheme_boundary(s, to)));
        }
        if out.is_empty() { vec![r] } else { out }
    });
    if let Some(sel) = sel {
        ed.view_mut().selection = sel;
    }
}

fn trim_selections(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let sel = ed.view().selection.transform_many(s, |r| {
        let (mut a, mut b) = (r.from(), r.to(s));
        while a < b && s.char(a).is_whitespace() {
            a += 1;
        }
        while b > a && s.char(b - 1).is_whitespace() {
            b -= 1;
        }
        if a == b {
            vec![]
        } else if r.is_forward() {
            vec![Range::new(a, prev_grapheme_boundary(s, b))]
        } else {
            vec![Range::new(prev_grapheme_boundary(s, b), a)]
        }
    });
    match sel {
        Some(sel) => ed.view_mut().selection = sel,
        None => collapse_selection(ed),
    }
}

fn select_mode(ed: &mut Editor) {
    ed.mode = Mode::Select;
}

fn select_line_mode(ed: &mut Editor) {
    extend_to_line_bounds(ed);
    ed.mode = Mode::Select;
}

fn search(ed: &mut Editor) {
    prompt::open(ed, PromptKind::Search { backward: false });
}
fn rsearch(ed: &mut Editor) {
    prompt::open(ed, PromptKind::Search { backward: true });
}

pub fn search_next_impl(ed: &mut Editor, backward: bool) {
    let Some(pattern) = ed.last_search.clone() else {
        ed.error("no previous search");
        return;
    };
    let re = match crate::search::compile(&pattern) {
        Ok(re) => re,
        Err(e) => return ed.error(e),
    };
    let text = text_of(ed);
    let s = text.slice(..);
    let count = ed.count_or(1);
    let primary = ed.view().selection.primary();
    let mut head = if backward {
        primary.from()
    } else {
        primary.last()
    };
    let mut wrapped = false;
    let mut found = None;
    for _ in 0..count {
        match crate::search::find(&text, &re, head, backward) {
            Some(((st, e), w)) => {
                wrapped |= w;
                head = st;
                found = Some((st, e));
            }
            None => break,
        }
    }
    let Some((st, e)) = found else {
        ed.error(format!("pattern not found: {pattern}"));
        return;
    };
    let r = Range::new(st, e.saturating_sub(1).max(st));
    let sel = if ed.extending() {
        ed.view().selection.clone().push(r, s)
    } else {
        Selection::new(vec![r], 0)
    };
    ed.view_mut().selection = sel;
    if wrapped {
        ed.set_status(Severity::Info, "search wrapped around the document");
    }
}

fn search_next(ed: &mut Editor) {
    search_next_impl(ed, false);
}
fn search_prev(ed: &mut Editor) {
    search_next_impl(ed, true);
}

fn search_selection(ed: &mut Editor) {
    let text = text_of(ed);
    let frag = ed.view().selection.primary().fragment(text.slice(..));
    let pattern = regex::escape(&frag);
    ed.registers.set('/', vec![pattern.clone()]);
    ed.info(format!("search: {pattern}"));
    ed.last_search = Some(pattern);
}

fn match_brackets(ed: &mut Editor) {
    let extend = ed.extending();
    move_each(ed, |s, r| match motion::match_bracket(s, r.head) {
        Some(p) => r.put_head(p, extend),
        None => r,
    });
}

fn surround_add(ed: &mut Editor) {
    ed.on_next_char("surround with", |ed, c| {
        let (open, close) = motion::matching_pair(c).unwrap_or((c, c));
        let text = text_of(ed);
        let s = text.slice(..);
        let mut changes = Vec::new();
        for r in ed.view().selection.iter() {
            changes.push(Change::insert(r.from(), open.to_string()));
            changes.push(Change::insert(r.to(s), close.to_string()));
        }
        let cs = changes_from(&text, changes);
        let sel = ed
            .view()
            .selection
            .transform(s, |r| r)
            .iter()
            .map(|r| {
                Range::new(
                    cs.map_pos(r.from(), maestro_core::Assoc::Before),
                    cs.map_pos(r.to(s), maestro_core::Assoc::After) - 1,
                )
            })
            .collect::<Vec<_>>();
        let primary = ed.view().selection.primary_index();
        ed.apply_or_report(Transaction::new(cs).with_selection(Selection::new(sel, primary)));
    });
}

fn surround_pairs(ed: &Editor, c: char) -> Option<Vec<(usize, usize)>> {
    let (open, close) = motion::matching_pair(c)?;
    let text = ed.doc().text().slice(..);
    let mut pairs: Vec<(usize, usize)> = ed
        .view()
        .selection
        .iter()
        .filter_map(|r| motion::surrounding_pair(text, r.head, open, close))
        .collect();
    pairs.sort();
    pairs.dedup();
    if pairs.is_empty() { None } else { Some(pairs) }
}

fn surround_delete(ed: &mut Editor) {
    ed.on_next_char("delete surrounding", |ed, c| {
        let Some(pairs) = surround_pairs(ed, c) else {
            return ed.error(format!("no surrounding {c} found"));
        };
        let mut changes: Vec<Change> = pairs
            .iter()
            .flat_map(|&(a, b)| [Change::delete(a, a + 1), Change::delete(b, b + 1)])
            .collect();
        changes.sort_by_key(|c| c.from);
        changes.dedup();
        let text = text_of(ed);
        let cs = changes_from(&text, changes);
        ed.apply_or_report(Transaction::new(cs));
    });
}

fn surround_replace(ed: &mut Editor) {
    ed.on_next_char("replace surrounding", |ed, from| {
        ed.on_next_char("replace with", move |ed, to| {
            let Some(pairs) = surround_pairs(ed, from) else {
                return ed.error(format!("no surrounding {from} found"));
            };
            let (open, close) = motion::matching_pair(to).unwrap_or((to, to));
            let mut changes: Vec<Change> = pairs
                .iter()
                .flat_map(|&(a, b)| {
                    [
                        Change::replace(a, a + 1, open.to_string()),
                        Change::replace(b, b + 1, close.to_string()),
                    ]
                })
                .collect();
            changes.sort_by_key(|c| c.from);
            changes.dedup();
            let text = text_of(ed);
            let cs = changes_from(&text, changes);
            ed.apply_or_report(Transaction::new(cs));
        });
    });
}

fn select_textobject(ed: &mut Editor, kind: ObjectKind) {
    let label = if kind == ObjectKind::Inside {
        "inside"
    } else {
        "around"
    };
    ed.on_next_char(label, move |ed, c| {
        let mut missing = false;
        move_each(ed, |s, r| match motion::text_object(s, r, kind, c) {
            Some(new) => new,
            None => {
                missing = true;
                r
            }
        });
        if missing {
            ed.set_status(
                Severity::Warning,
                format!("no {c} object around every cursor"),
            );
        }
    });
}
fn select_textobject_inner(ed: &mut Editor) {
    select_textobject(ed, ObjectKind::Inside);
}
fn select_textobject_around(ed: &mut Editor) {
    select_textobject(ed, ObjectKind::Around);
}

// ----- modes and inserts ---------------------------------------------------

fn insert_mode(ed: &mut Editor) {
    enter_insert(ed, |_, r| r.from());
}

fn append_mode(ed: &mut Editor) {
    enter_insert(ed, |s, r| {
        let last = r.last();
        if last < s.len_chars() && is_line_ending_char(s.char(last)) {
            last
        } else {
            r.to(s)
        }
    });
}

fn insert_at_line_start(ed: &mut Editor) {
    enter_insert(ed, |s, r| motion::first_non_blank(s, r.from()));
}

fn insert_at_line_end(ed: &mut Editor) {
    enter_insert(ed, |s, r| {
        line_end_excluding_terminator(s, s.char_to_line(r.last()))
    });
}

fn open_line(ed: &mut Editor, below: bool) {
    let text = text_of(ed);
    let s = text.slice(..);
    let le = ed.doc().line_ending().as_str();
    let count = ed.count_or(1);
    let mut changes = Vec::new();
    let mut cursor_offsets = Vec::new();
    let mut lines: Vec<usize> = ed
        .view()
        .selection
        .iter()
        .map(|r| s.char_to_line(if below { r.last() } else { r.from() }))
        .collect();
    lines.dedup();
    for line in lines {
        let indent = line_indent(s, line);
        let ind_len = indent.chars().count();
        if below {
            let at = line_end_excluding_terminator(s, line);
            let piece = format!("{le}{indent}");
            changes.push(Change::insert(at, piece.repeat(count)));
            cursor_offsets.push((at, le.chars().count() + ind_len));
        } else {
            let at = s.line_to_char(line);
            let piece = format!("{indent}{le}");
            changes.push(Change::insert(at, piece.repeat(count)));
            cursor_offsets.push((at, ind_len));
        }
    }
    let cs = changes_from(&text, changes);
    let primary = ed.view().selection.primary_index();
    let ranges: Vec<Range> = cursor_offsets
        .iter()
        .map(|&(at, off)| Range::point(cs.map_pos(at, maestro_core::Assoc::Before) + off))
        .collect();
    let primary = primary.min(ranges.len() - 1);
    ed.apply_or_report(Transaction::new(cs).with_selection(Selection::new(ranges, primary)));
    ed.mode = Mode::Insert;
}

fn open_below(ed: &mut Editor) {
    open_line(ed, true);
}
fn open_above(ed: &mut Editor) {
    open_line(ed, false);
}

fn normal_mode(ed: &mut Editor) {
    if ed.mode == Mode::Insert {
        // Leaving insert at the end of the text moves back onto the last
        // character; elsewhere the cursor stays put.
        ed.mode = Mode::Normal;
        return;
    }
    if ed.mode == Mode::Select {
        ed.mode = Mode::Normal;
        return;
    }
    // Escape in normal mode cancels counts and pending state (handled by the
    // dispatcher) and collapses multi-character selections.
    collapse_selection(ed);
}

fn command_mode(ed: &mut Editor) {
    prompt::open(ed, PromptKind::Command);
}

pub fn insert_char(ed: &mut Editor, c: char) {
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| Change::insert(r.head, c.to_string()))
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAfter);
}

fn insert_newline(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let le = ed.doc().line_ending().as_str().to_string();
    let unit = ed.config.editor.indent_unit();
    let mut changes = Vec::new();
    let mut offsets = Vec::new();
    for r in ed.view().selection.iter() {
        let p = r.head;
        let line = s.char_to_line(p);
        let line_start = s.line_to_char(line);
        let indent = line_indent(s, line);
        // Never copy more indentation than precedes the cursor.
        let indent: String = indent.chars().take(p - line_start).collect();
        let prev = (line_start..p)
            .rev()
            .map(|i| s.char(i))
            .find(|c| !c.is_whitespace());
        let next = (p < s.len_chars()).then(|| s.char(p));
        let opens = matches!(prev, Some('{' | '(' | '['));
        let closes_pair = opens
            && matches!(
                (prev, next),
                (Some('{'), Some('}')) | (Some('('), Some(')')) | (Some('['), Some(']'))
            );
        // Drop trailing whitespace before the cursor on the line being split.
        let mut ws_start = p;
        while ws_start > line_start && matches!(s.char(ws_start - 1), ' ' | '\t') {
            ws_start -= 1;
        }
        let inner = if opens {
            format!("{indent}{unit}")
        } else {
            indent.clone()
        };
        let mut ins = format!("{le}{inner}");
        let cursor = ins.chars().count();
        if closes_pair {
            ins.push_str(&le);
            ins.push_str(&indent);
        }
        // Only drop it when the line has content before it.
        let from = if ws_start > line_start { ws_start } else { p };
        changes.push(Change::replace(from, p, ins));
        offsets.push((from, cursor));
    }
    let cs = changes_from(&text, changes);
    let primary = ed.view().selection.primary_index();
    let ranges: Vec<Range> = offsets
        .iter()
        .map(|&(at, off)| Range::point(cs.map_pos(at, maestro_core::Assoc::Before) + off))
        .collect();
    ed.apply_or_report(Transaction::new(cs).with_selection(Selection::new(ranges, primary)));
}

fn insert_tab(ed: &mut Editor) {
    let unit = ed.config.editor.indent_unit();
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| Change::insert(r.head, unit.clone()))
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAfter);
}

fn delete_char_backward(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let width = ed.config.editor.indent_width.max(1);
    let spaces = ed.config.editor.indent == maestro_config::IndentStyle::Spaces;
    let count = ed.count_or(1);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| {
            let p = r.head;
            let line_start = s.line_to_char(s.char_to_line(p));
            let before: String = s.slice(line_start..p).to_string();
            if spaces && count == 1 && !before.is_empty() && before.chars().all(|c| c == ' ') {
                // In leading indentation, delete back to the previous stop.
                let n = before.len();
                let back = match n % width {
                    0 => width,
                    m => m,
                };
                return Change::delete(p - back.min(n), p);
            }
            let mut from = p;
            for _ in 0..count {
                from = prev_grapheme_boundary(s, from);
            }
            Change::delete(from, p)
        })
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
}

fn delete_char_forward(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let count = ed.count_or(1);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| {
            let mut to = r.head;
            for _ in 0..count {
                to = next_grapheme_boundary(s, to);
            }
            Change::delete(r.head, to)
        })
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
}

fn delete_word_backward(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| {
            // Walk whole graphemes so CRLF and combining sequences are never
            // split; a grapheme's class is the class of its first char.
            let p = r.head;
            let class_before = |i: usize| char_class(s.char(prev_grapheme_boundary(s, i)));
            let mut i = p;
            while i > 0 && class_before(i) == CharClass::Whitespace {
                i = prev_grapheme_boundary(s, i);
            }
            if i > 0 {
                let cls = class_before(i);
                while i > 0 && class_before(i) == cls {
                    i = prev_grapheme_boundary(s, i);
                    if cls == CharClass::Eol {
                        break;
                    }
                }
            }
            Change::delete(i, p)
        })
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
}

fn kill_to_line_start(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| Change::delete(motion::line_start(s, r.head), r.head))
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
}

fn delete_to_line_end(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let name = register_name(ed);
    let values = ed
        .view()
        .selection
        .iter()
        .map(|r| {
            let end = line_end_excluding_terminator(s, s.char_to_line(r.head));
            s.slice(r.head..end.max(r.head)).to_string()
        })
        .collect();
    write_register(ed, name, values);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| {
            let end = line_end_excluding_terminator(s, s.char_to_line(r.head));
            Change::delete(r.head, end.max(r.head))
        })
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
}

fn delete_line(ed: &mut Editor) {
    let n = ed.count_or(1);
    move_each(ed, |s, r| {
        let first = s.char_to_line(r.from());
        let last = (s.char_to_line(r.last()) + n - 1).min(motion::last_line(s));
        motion::line_range(s, Range::new(s.line_to_char(first), s.line_to_char(last)))
    });
    ed.count = None;
    delete_impl(ed, true);
    move_heads(ed, motion::first_non_blank);
}

fn yank_line(ed: &mut Editor) {
    let saved = ed.view().selection.clone();
    extend_to_line_bounds(ed);
    let name = register_name(ed);
    yank_impl(ed, name);
    ed.view_mut().selection = saved;
}

fn change_line(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| {
            let line = s.char_to_line(r.head);
            let start = motion::first_non_blank(s, s.line_to_char(line));
            Change::delete(start, line_end_excluding_terminator(s, line))
        })
        .collect();
    apply_changes(ed, changes, AfterEdit::CursorAtStart);
    ed.mode = Mode::Insert;
}

fn delete_selection(ed: &mut Editor) {
    delete_impl(ed, true);
}
fn delete_selection_noyank(ed: &mut Editor) {
    delete_impl(ed, false);
}
fn change_selection(ed: &mut Editor) {
    delete_impl(ed, true);
    ed.mode = Mode::Insert;
}
fn change_selection_noyank(ed: &mut Editor) {
    delete_impl(ed, false);
    ed.mode = Mode::Insert;
}

// ----- yank and paste --------------------------------------------------------

fn yank(ed: &mut Editor) {
    let name = register_name(ed);
    yank_impl(ed, name);
}

fn paste_impl(ed: &mut Editor, name: char, before: bool) {
    let Some(values) = read_register(ed, name) else {
        if ed.status.is_none() {
            ed.error(format!("register {name} is empty"));
        }
        return;
    };
    let text = text_of(ed);
    let s = text.slice(..);
    let count = ed.count_or(1);
    let le = ed.doc().line_ending().as_str();
    let mut changes = Vec::new();
    for (i, r) in ed.view().selection.iter().enumerate() {
        let value = values[i.min(values.len() - 1)].repeat(count);
        let linewise = value.ends_with('\n');
        if linewise {
            let line = s.char_to_line(if before { r.from() } else { r.last() });
            if before {
                changes.push(Change::insert(s.line_to_char(line), value));
            } else if line + 1 < s.len_lines() {
                changes.push(Change::insert(s.line_to_char(line + 1), value));
            } else {
                // Last line without a terminator: add one first and drop
                // exactly one from the end, so blank lines in the block survive.
                let value = value
                    .strip_suffix("\r\n")
                    .or_else(|| value.strip_suffix('\n'))
                    .unwrap_or(&value)
                    .to_string();
                changes.push(Change::insert(s.len_chars(), format!("{le}{value}")));
            }
        } else {
            let at = if before { r.from() } else { r.to(s) };
            changes.push(Change::insert(at, value));
        }
    }
    changes.sort_by_key(|c| c.from);
    apply_changes(ed, changes, AfterEdit::SelectInserted);
}

fn paste_after(ed: &mut Editor) {
    let name = register_name(ed);
    paste_impl(ed, name, false);
}
fn paste_before(ed: &mut Editor) {
    let name = register_name(ed);
    paste_impl(ed, name, true);
}
fn yank_to_clipboard(ed: &mut Editor) {
    yank_impl(ed, '+');
}
fn paste_clipboard_after(ed: &mut Editor) {
    paste_impl(ed, '+', false);
}
fn paste_clipboard_before(ed: &mut Editor) {
    paste_impl(ed, '+', true);
}

fn replace_with_register(ed: &mut Editor, name: char) {
    let Some(values) = read_register(ed, name) else {
        if ed.status.is_none() {
            ed.error(format!("register {name} is empty"));
        }
        return;
    };
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = ed
        .view()
        .selection
        .iter()
        .enumerate()
        .map(|(i, r)| Change::replace(r.from(), r.to(s), values[i.min(values.len() - 1)].clone()))
        .collect();
    apply_changes(ed, changes, AfterEdit::SelectInserted);
}

fn replace_with_yanked(ed: &mut Editor) {
    let name = register_name(ed);
    replace_with_register(ed, name);
}
fn replace_selections_with_clipboard(ed: &mut Editor) {
    replace_with_register(ed, '+');
}

fn replace(ed: &mut Editor) {
    ed.on_next_char("replace with", |ed, c| {
        let text = text_of(ed);
        let s = text.slice(..);
        let changes = ed
            .view()
            .selection
            .iter()
            .map(|r| {
                let replaced: String = maestro_core::text::graphemes(s.slice(r.from()..r.to(s)))
                    .map(|g| {
                        if maestro_core::text::is_line_ending_str(&g) {
                            g.into_owned()
                        } else {
                            c.to_string()
                        }
                    })
                    .collect();
                Change::replace(r.from(), r.to(s), replaced)
            })
            .collect();
        apply_changes(ed, changes, AfterEdit::SelectInserted);
    });
}

// ----- history -----------------------------------------------------------------

fn history_step(ed: &mut Editor, redo: bool) {
    let count = ed.count_or(1);
    for _ in 0..count {
        let sel = ed.view().selection.clone();
        let doc_id = ed.view().doc;
        let focus = ed.focus;
        let old = text_of(ed);
        let step = {
            let buf = ed.buffer_mut();
            if redo {
                buf.doc.redo(&sel)
            } else {
                buf.doc.undo(&sel)
            }
        };
        let Some(step) = step else {
            if let Some(r) = ed.doc().read_only() {
                ed.error(format!("read-only: {}", r.describe()));
            } else {
                ed.info(if redo {
                    "already at newest change"
                } else {
                    "already at oldest change"
                });
            }
            return;
        };
        ed.on_text_changed(doc_id, &old, &step.applied);
        let text = text_of(ed);
        for v in ed.views.values_mut().filter(|v| v.doc == doc_id) {
            v.selection = if v.id == focus {
                step.selection.clone()
            } else {
                step.applied
                    .iter()
                    .fold(v.selection.clone(), |sel, cs| sel.map(cs))
            }
            .clamp_normal(text.slice(..));
        }
        crate::lsp::document_changed(ed, doc_id, &step.applied);
    }
}

fn undo(ed: &mut Editor) {
    history_step(ed, false);
}
fn redo(ed: &mut Editor) {
    history_step(ed, true);
}

// ----- line and case transforms ----------------------------------------------

fn selected_lines(ed: &Editor) -> Vec<usize> {
    let s = ed.doc().text().slice(..);
    let mut lines: Vec<usize> = ed
        .view()
        .selection
        .iter()
        .flat_map(|r| s.char_to_line(r.from())..=s.char_to_line(r.last()))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn indent(ed: &mut Editor) {
    let unit = ed.config.editor.indent_unit().repeat(ed.count_or(1));
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = selected_lines(ed)
        .into_iter()
        .filter(|&l| {
            let start = s.line_to_char(l);
            line_end_excluding_terminator(s, l) > start
        })
        .map(|l| Change::insert(s.line_to_char(l), unit.clone()))
        .collect();
    let cs = changes_from(&text, changes);
    ed.apply_or_report(Transaction::new(cs));
}

fn unindent(ed: &mut Editor) {
    let width = ed.config.editor.indent_width.max(1) * ed.count_or(1);
    let tab = ed.tab_width();
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = selected_lines(ed)
        .into_iter()
        .filter_map(|l| {
            let start = s.line_to_char(l);
            let mut i = start;
            let mut col = 0;
            while col < width && i < s.len_chars() {
                match s.char(i) {
                    ' ' => col += 1,
                    '\t' => col += tab,
                    _ => break,
                }
                i += 1;
            }
            (i > start).then(|| Change::delete(start, i))
        })
        .collect();
    let cs = changes_from(&text, changes);
    ed.apply_or_report(Transaction::new(cs));
}

fn map_case(ed: &mut Editor, f: fn(&str) -> String) {
    let text = text_of(ed);
    let s = text.slice(..);
    let changes = ed
        .view()
        .selection
        .iter()
        .map(|r| Change::replace(r.from(), r.to(s), f(&r.fragment(s))))
        .collect();
    let sel = ed.view().selection.clone();
    let cs = changes_from(&text, changes);
    // Case changes can alter lengths (ß -> SS); map the old selection.
    let new_sel = sel.map(&cs);
    ed.apply_or_report(Transaction::new(cs).with_selection(new_sel));
}

fn switch_case(ed: &mut Editor) {
    map_case(ed, |s| {
        s.chars()
            .flat_map(|c| {
                if c.is_uppercase() {
                    c.to_lowercase().collect::<Vec<_>>()
                } else {
                    c.to_uppercase().collect::<Vec<_>>()
                }
            })
            .collect()
    });
}
fn switch_to_lowercase(ed: &mut Editor) {
    map_case(ed, str::to_lowercase);
}
fn switch_to_uppercase(ed: &mut Editor) {
    map_case(ed, str::to_uppercase);
}

fn join_selections(ed: &mut Editor) {
    let text = text_of(ed);
    let s = text.slice(..);
    let last_line = motion::last_line(s);
    let mut lines: Vec<usize> = Vec::new();
    for r in ed.view().selection.iter() {
        let (a, b) = (s.char_to_line(r.from()), s.char_to_line(r.last()));
        let end = if a == b { a + 1 } else { b };
        lines.extend(a..end.min(last_line));
    }
    lines.sort_unstable();
    lines.dedup();
    let changes = lines
        .into_iter()
        .map(|l| {
            let from = line_end_excluding_terminator(s, l);
            let mut to = s.line_to_char(l + 1);
            while to < s.len_chars() && matches!(s.char(to), ' ' | '\t') {
                to += 1;
            }
            let next_empty = to >= s.len_chars() || is_line_ending_char(s.char(to));
            Change::replace(from, to, if next_empty { "" } else { " " })
        })
        .collect();
    let cs = changes_from(&text, changes);
    ed.apply_or_report(Transaction::new(cs));
}

// ----- repeat, macros, marks -------------------------------------------------

fn repeat_last_change(ed: &mut Editor) {
    ed.repeat_last_change();
}

fn record_macro(ed: &mut Editor) {
    if let Some((reg, mut keys)) = ed.recording.take() {
        keys.pop(); // the key that stopped recording
        let n = keys.len();
        let notation: Vec<String> = keys.iter().map(ToString::to_string).collect();
        ed.registers.set(reg, vec![notation.join(" ")]);
        ed.info(format!("recorded {n} keys into register {reg}"));
    } else {
        let reg = ed.selected_register.unwrap_or('@');
        ed.recording = Some((reg, Vec::new()));
        ed.info(format!("recording into register {reg}; Q stops"));
    }
}

fn replay_macro(ed: &mut Editor) {
    let reg = ed.selected_register.unwrap_or('@');
    if ed.recording.as_ref().is_some_and(|(r, _)| *r == reg) {
        return ed.error("cannot replay a macro while recording it");
    }
    let Some(seq) = ed.registers.get(reg).and_then(|v| v.first()).cloned() else {
        return ed.error(format!("register {reg} is empty"));
    };
    let keys = match parse_sequence(&seq) {
        Ok(k) => k,
        Err(e) => return ed.error(format!("register {reg}: {e}")),
    };
    // A macro may replay itself or another macro; cap the nesting so a
    // self-invoking macro stops with an error instead of overflowing the stack.
    const MAX_DEPTH: usize = 16;
    if ed.macro_depth >= MAX_DEPTH {
        ed.error(format!("macro nesting deeper than {MAX_DEPTH}; stopped"));
        return;
    }
    let count = ed.count_or(1);
    ed.count = None;
    ed.selected_register = None;
    ed.macro_depth += 1;
    for _ in 0..count {
        ed.feed_keys(&keys);
        if ed
            .status
            .as_ref()
            .is_some_and(|(_, m)| m.starts_with("macro nesting"))
        {
            break;
        }
    }
    ed.macro_depth -= 1;
}

fn earlier(ed: &mut Editor) {
    goto_revision_step(ed, false);
}

fn later(ed: &mut Editor) {
    goto_revision_step(ed, true);
}

/// Moves to the chronologically previous or next state of the document,
/// across undo branches (Vim's g- and g+).
fn goto_revision_step(ed: &mut Editor, forward: bool) {
    let count = ed.count_or(1);
    let doc_id = ed.view().doc;
    let focus = ed.focus;
    let (current, len) = {
        let h = ed.doc().history();
        (h.current(), h.len())
    };
    let target = if forward {
        (current + count).min(len - 1)
    } else {
        current.saturating_sub(count)
    };
    let sel = ed.view().selection.clone();
    let old = text_of(ed);
    let Some(step) = ed.buffer_mut().doc.goto_revision(target, &sel) else {
        ed.info(if forward {
            "already at the newest state"
        } else {
            "already at the oldest state"
        });
        return;
    };
    let text = text_of(ed);
    for v in ed.views.values_mut().filter(|v| v.doc == doc_id) {
        v.selection = if v.id == focus {
            step.selection.clone()
        } else {
            step.applied
                .iter()
                .fold(v.selection.clone(), |sel, cs| sel.map(cs))
        }
        .clamp_normal(text.slice(..));
    }
    ed.on_text_changed(doc_id, &old, &step.applied);
    crate::lsp::document_changed(ed, doc_id, &step.applied);
    ed.info(format!("state {target} of {}", len - 1));
}

fn set_mark(ed: &mut Editor) {
    ed.on_next_char("set mark", |ed, c| {
        let doc = ed.view().doc;
        let head = ed.view().selection.primary().head;
        ed.marks.insert(c, (doc, head));
        ed.info(format!("mark {c} set"));
    });
}

fn goto_mark(ed: &mut Editor) {
    ed.on_next_char("go to mark", |ed, c| {
        let Some(&(doc, pos)) = ed.marks.get(&c) else {
            return ed.error(format!("mark {c} is not set"));
        };
        if !ed.buffers.contains_key(&doc) {
            return ed.error(format!("mark {c} points at a closed buffer"));
        }
        ed.push_jump();
        ed.switch_to(doc);
        let len = ed.doc().text().len_chars();
        ed.set_selection(Selection::point(pos.min(len)));
    });
}

// ----- views and buffers -----------------------------------------------------

fn vsplit(ed: &mut Editor) {
    ed.split(Direction::Vertical);
}
fn hsplit(ed: &mut Editor) {
    ed.split(Direction::Horizontal);
}
fn rotate_view(ed: &mut Editor) {
    ed.focus_next(1);
}
fn wclose(ed: &mut Editor) {
    if !ed.close_view() {
        ed.error("cannot close the last view; use :q");
    }
}
fn wonly(ed: &mut Editor) {
    ed.only_view();
}
fn jump_view_left(ed: &mut Editor) {
    ed.focus_direction(-1, 0);
}
fn jump_view_right(ed: &mut Editor) {
    ed.focus_direction(1, 0);
}
fn jump_view_up(ed: &mut Editor) {
    ed.focus_direction(0, -1);
}
fn jump_view_down(ed: &mut Editor) {
    ed.focus_direction(0, 1);
}

fn cycle_buffer(ed: &mut Editor, delta: isize) {
    let ids: Vec<_> = ed.buffers.keys().copied().collect();
    let cur = ids.iter().position(|i| *i == ed.view().doc).unwrap_or(0) as isize;
    let next = ids[((cur + delta).rem_euclid(ids.len() as isize)) as usize];
    ed.switch_to(next);
}
fn buffer_next(ed: &mut Editor) {
    cycle_buffer(ed, 1);
}
fn buffer_prev(ed: &mut Editor) {
    cycle_buffer(ed, -1);
}

fn buffer_picker(ed: &mut Editor) {
    let current = ed.view().doc;
    let items = ed
        .buffers
        .values()
        .map(|b| PickerItem {
            label: b.doc.display_name(),
            detail: format!(
                "{}{}",
                if b.doc.is_modified() { "modified" } else { "" },
                if b.id == current { " current" } else { "" }
            )
            .trim()
            .to_string(),
            action: PickerAction::Buffer(b.id),
        })
        .collect();
    ed.picker = Some(Picker::new("buffers", items));
}

fn command_palette(ed: &mut Editor) {
    let mode = if ed.mode == Mode::Insert {
        Mode::Normal
    } else {
        ed.mode
    };
    let mut items: Vec<PickerItem> = all()
        .map(|c| {
            let key = ed.keymaps.key_for(mode, c.name).unwrap_or_default();
            PickerItem {
                label: format!("{}  {}", c.name, c.doc),
                detail: key,
                action: PickerAction::Command(c.name.to_string()),
            }
        })
        .collect();
    for t in crate::typed::all() {
        items.push(PickerItem {
            label: format!(":{}  {}", t.name, t.doc),
            detail: String::new(),
            action: PickerAction::Command(t.name.to_string()),
        });
    }
    ed.picker = Some(Picker::new("commands", items));
}

fn toggle_focus_mode(ed: &mut Editor) {
    ed.presentation = if ed.presentation == Presentation::Focus {
        Presentation::Engineering
    } else {
        Presentation::Focus
    };
    ed.relayout();
}
