//! Terminal front end. Owns the terminal, converts input into editor key
//! events and draws frames; all editing logic lives in `maestro-editor`.

mod agents;
pub mod clipboard;
mod git;
mod preview;
mod proposals;
pub mod render;
pub mod surface;
pub mod terminal;

use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
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

/// What the event loop waits on: terminal input from the reader thread, or
/// a wake-up from a background producer.
enum Msg {
    Input(Event),
    Wake,
    Failed(io::Error),
}

/// Starts the thread that reads terminal input and installs the editor's
/// waker. Both feed one channel, so the loop sleeps until either arrives.
/// Wakes are coalesced: at most one is queued until the loop drains it.
fn start_input_reader(editor: &Editor) -> (mpsc::Receiver<Msg>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel();
    let pending = Arc::new(AtomicBool::new(false));
    let wake_tx = tx.clone();
    let flag = pending.clone();
    editor.set_waker(move || {
        if !flag.swap(true, Ordering::SeqCst) {
            let _ = wake_tx.send(Msg::Wake);
        }
    });
    // The thread blocks in `event::read` for the life of the process; it
    // ends when the receiver is dropped and the next event fails to send.
    let _ = std::thread::Builder::new()
        .name("maestro-input".into())
        .spawn(move || {
            loop {
                let msg = match event::read() {
                    Ok(ev) => Msg::Input(ev),
                    Err(e) => {
                        let _ = tx.send(Msg::Failed(e));
                        return;
                    }
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });
    (rx, pending)
}

/// Applies one terminal event to the editor.
fn handle_event(
    editor: &mut Editor,
    ev: Event,
    w: &mut u16,
    h: &mut u16,
    prev: &mut Option<Surface>,
    out: &mut impl Write,
) -> io::Result<()> {
    match ev {
        Event::Key(k) if k.kind != KeyEventKind::Release => {
            if let Some(key) = convert_key(k) {
                editor.handle_key(key);
            }
        }
        Event::Paste(text) => editor.paste_text(&text),
        Event::Resize(nw, nh) => {
            (*w, *h) = (nw, nh);
            *prev = None;
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
    Ok(())
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
    let (msgs, wake_pending) = start_input_reader(&editor);

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
            handle_time = Duration::ZERO;
            prev = Some(surface);
            dirty = false;
            if opts.exit_after_first_frame {
                return Ok(());
            }
        }
        if editor.should_quit {
            return Ok(());
        }

        // Clear the flag before draining, so a wake sent while jobs run
        // queues another pass instead of being lost.
        wake_pending.store(false, Ordering::SeqCst);
        if editor.poll_background() {
            dirty = true;
            continue;
        }
        let timeout = tick_every.saturating_sub(last_tick.elapsed());
        let first = match msgs.recv_timeout(timeout) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("terminal input reader stopped"));
            }
        };
        if let Some(first) = first {
            let start = Instant::now();
            let mut had_input = false;
            // Drain everything already queued before drawing, so a burst of
            // input (fast typing, key repeat) costs one frame.
            let mut next = Some(first);
            while let Some(msg) = next {
                match msg {
                    Msg::Wake => {}
                    Msg::Failed(e) => return Err(e),
                    Msg::Input(ev) => {
                        had_input = true;
                        handle_event(&mut editor, ev, &mut w, &mut h, &mut prev, &mut out)?;
                    }
                }
                if editor.should_quit {
                    break;
                }
                next = msgs.try_recv().ok();
            }
            if had_input {
                handle_time = start.elapsed();
                dirty = true;
            }
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
