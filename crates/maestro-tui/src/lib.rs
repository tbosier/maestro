//! Terminal front end. Owns the terminal, converts input into editor key
//! events and draws frames; all editing logic lives in `maestro-editor`.

pub mod clipboard;
mod preview;
pub mod render;
pub mod surface;
pub mod terminal;

use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode as CtKeyCode, KeyEvent as CtKeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use maestro_editor::{Editor, KeyCode, KeyEvent, Modifiers, MouseKind};

use crate::render::split_screen;
use crate::surface::Surface;
use crate::terminal::{ColorMode, TerminalGuard};

/// Converts a crossterm key event. Returns `None` for keys the editor does
/// not model (media keys, modifier-only presses).
pub fn convert_key(k: CtKeyEvent) -> Option<KeyEvent> {
    let mods = Modifiers {
        ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
        alt: k.modifiers.contains(KeyModifiers::ALT),
        shift: k.modifiers.contains(KeyModifiers::SHIFT),
    };
    let code = match k.code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Esc => KeyCode::Esc,
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Delete => KeyCode::Delete,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::BackTab => KeyCode::BackTab,
        CtKeyCode::Left => KeyCode::Left,
        CtKeyCode::Right => KeyCode::Right,
        CtKeyCode::Up => KeyCode::Up,
        CtKeyCode::Down => KeyCode::Down,
        CtKeyCode::Home => KeyCode::Home,
        CtKeyCode::End => KeyCode::End,
        CtKeyCode::PageUp => KeyCode::PageUp,
        CtKeyCode::PageDown => KeyCode::PageDown,
        CtKeyCode::Insert => KeyCode::Insert,
        CtKeyCode::F(n) => KeyCode::F(n),
        _ => return None,
    };
    Some(KeyEvent::new(code, mods))
}

/// Per-event latency samples, written when `MAESTRO_LATENCY_LOG` names a
/// file. Each line: `handle_us render_us flush_us cells_written`.
struct LatencyLog {
    out: Option<BufWriter<std::fs::File>>,
}

impl LatencyLog {
    fn from_env() -> Self {
        let out = std::env::var_os("MAESTRO_LATENCY_LOG")
            .and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            })
            .map(BufWriter::new);
        Self { out }
    }

    fn record(&mut self, handle: Duration, render: Duration, flush: Duration, cells: usize) {
        if let Some(out) = &mut self.out {
            let _ = writeln!(
                out,
                "{} {} {} {}",
                handle.as_micros(),
                render.as_micros(),
                flush.as_micros(),
                cells
            );
        }
    }
}

pub struct RunOptions {
    /// Draw one frame and exit; used to measure startup time.
    pub exit_after_first_frame: bool,
}

/// Runs the editor until it quits.
pub fn run(mut editor: Editor, opts: RunOptions) -> io::Result<()> {
    let mouse = editor.config.editor.mouse;
    terminal::install_panic_hook(mouse);
    let _guard = TerminalGuard::enter(mouse)?;
    editor.clipboard = clipboard::detect();
    let color = ColorMode::detect();
    let mut out = BufWriter::with_capacity(1 << 16, io::stdout());
    let (mut w, mut h) = crossterm::terminal::size()?;
    let mut prev: Option<Surface> = None;
    let mut log = LatencyLog::from_env();
    let mut last_tick = Instant::now();
    let tick_every = Duration::from_millis(1000);
    let mut handle_time = Duration::ZERO;
    let mut dirty = true;

    loop {
        // Presentation changes (focus mode) move the view area.
        let (_, views, _) = split_screen(w, h, &editor);
        if editor.area != views {
            editor.resize(views);
            dirty = true;
        }
        if dirty {
            let t0 = Instant::now();
            let mut surface = Surface::new(w, h, editor.theme.ui.text);
            let cursor = render::render(&editor, &mut surface);
            let t1 = Instant::now();
            let cells = terminal::flush(&mut out, prev.as_ref(), &surface, color, cursor)?;
            let t2 = Instant::now();
            log.record(handle_time, t1 - t0, t2 - t1, cells);
            prev = Some(surface);
            dirty = false;
            if opts.exit_after_first_frame {
                return Ok(());
            }
        }
        if editor.should_quit {
            return Ok(());
        }

        if editor.poll_background() {
            dirty = true;
            continue;
        }
        // Poll quickly while background work may deliver results.
        let mut timeout = tick_every.saturating_sub(last_tick.elapsed());
        if editor.has_background_work() {
            timeout = timeout.min(Duration::from_millis(16));
        }
        if event::poll(timeout)? {
            let start = Instant::now();
            // Drain everything already queued before drawing, so a burst of
            // input (fast typing, key repeat) costs one frame.
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if let Some(key) = convert_key(k) {
                            editor.handle_key(key);
                        }
                    }
                    Event::Paste(text) => editor.paste_text(&text),
                    Event::Resize(nw, nh) => {
                        (w, h) = (nw, nh);
                        prev = None;
                        crossterm::execute!(
                            out,
                            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
                        )?;
                    }
                    Event::Mouse(m) => {
                        let kind = match m.kind {
                            MouseEventKind::Down(MouseButton::Left) => Some(MouseKind::Down),
                            MouseEventKind::Drag(MouseButton::Left) => Some(MouseKind::Drag),
                            MouseEventKind::ScrollUp => Some(MouseKind::ScrollUp),
                            MouseEventKind::ScrollDown => Some(MouseKind::ScrollDown),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            editor.handle_mouse(kind, m.column, m.row);
                        }
                    }
                    Event::FocusGained => editor.tick(Instant::now()),
                    _ => {}
                }
                if editor.should_quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            handle_time = start.elapsed();
            dirty = true;
        }
        if last_tick.elapsed() >= tick_every {
            // Reloads and recovery messages change state without a key
            // press; redraw only when they did.
            let fingerprint = |ed: &Editor| {
                (
                    ed.status.clone(),
                    ed.buffers.values().map(|b| b.doc.version().0).sum::<u64>(),
                )
            };
            let before = fingerprint(&editor);
            editor.tick(Instant::now());
            last_tick = Instant::now();
            if fingerprint(&editor) != before {
                dirty = true;
            }
        }
    }
}
