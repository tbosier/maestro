//! `maestro`: the executable.

use std::path::PathBuf;
use std::process::ExitCode;

use maestro_config::Config;
use maestro_editor::{Editor, Rect, Severity};

const USAGE: &str = "\
usage: maestro [options] [file[:line]]...

options:
  -c, --config <path>   use this config file instead of the default
      --clean           ignore the user config
  -h, --help            show this help
      --headless        no terminal: serve the automation API on stdin/stdout
      --listen <path>   also serve the automation API on a Unix socket
                        (auto: $XDG_RUNTIME_DIR/maestro/<pid>.sock)
      --remote <socket> <json>
                        send one automation request to a running editor
  -V, --version         show the version

environment:
  MAESTRO_COLOR=truecolor|256    override color detection
  MAESTRO_LATENCY_LOG=<path>     append per-event timing samples to <path>
";

struct Args {
    files: Vec<(PathBuf, Option<usize>)>,
    config: Option<PathBuf>,
    clean: bool,
    first_frame_only: bool,
    // Automation API (docs/automation.md).
    headless: bool,
    listen: Option<String>,
    remote: Option<(PathBuf, String)>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        files: Vec::new(),
        config: None,
        clean: false,
        first_frame_only: false,
        headless: false,
        listen: None,
        remote: None,
    };
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        let s = a.to_string_lossy();
        match s.as_ref() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("maestro {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => {
                let p = it.next().ok_or("--config needs a path")?;
                args.config = Some(PathBuf::from(p));
            }
            "--clean" => args.clean = true,
            // Automation API flags.
            "--headless" => args.headless = true,
            "--listen" => {
                let p = it
                    .next()
                    .ok_or("--listen needs a socket path or \"auto\"")?;
                args.listen = Some(p.to_string_lossy().into_owned());
            }
            "--remote" => {
                let (Some(sock), Some(json)) = (it.next(), it.next()) else {
                    return Err("--remote needs a socket path and a JSON request".into());
                };
                args.remote = Some((PathBuf::from(sock), json.to_string_lossy().into_owned()));
            }
            // Undocumented: draw one frame and exit, for startup benchmarks.
            "--first-frame-only" => args.first_frame_only = true,
            "--" => {
                args.files
                    .extend(it.by_ref().map(|p| (PathBuf::from(p), None)));
            }
            _ if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            _ => args.files.push(split_line_suffix(&s, PathBuf::from(&a))),
        }
    }
    Ok(Some(args))
}

/// `path:42` opens at line 42 when `path:42` itself does not exist.
fn split_line_suffix(s: &str, raw: PathBuf) -> (PathBuf, Option<usize>) {
    if raw.exists() {
        return (raw, None);
    }
    if let Some((p, n)) = s.rsplit_once(':')
        && let Ok(n) = n.parse::<usize>()
    {
        return (PathBuf::from(p), Some(n));
    }
    (raw, None)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("maestro: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Some((socket, request)) = &args.remote {
        return maestro_automation::cli::remote_main(socket, request);
    }

    let mut startup_errors = Vec::new();
    let config = if args.clean {
        Config::default()
    } else {
        let path = args
            .config
            .clone()
            .or_else(|| maestro_config::config::config_dir().map(|d| d.join("config.toml")));
        match path.map(|p| Config::load(&p)) {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                startup_errors.push(e.to_string());
                Config::default()
            }
            None => Config::default(),
        }
    };

    let (w, h) = match args.headless {
        true => (80, 24),
        false => crossterm::terminal::size().unwrap_or((80, 24)),
    };
    let mut editor = Editor::new(config, Rect::new(0, 1, w, h.saturating_sub(2)));
    editor.recovery_dir = maestro_config::config::state_dir().map(|d| d.join("recovery"));
    startup_errors.extend(editor.keymaps.errors.clone());

    for (path, line) in &args.files {
        match editor.open(path) {
            Ok(_) => {
                if let Some(n) = line {
                    editor.execute_line(&n.to_string());
                }
            }
            Err(e) => startup_errors.push(e),
        }
    }
    if args.headless {
        return maestro_automation::cli::headless_main(editor, &startup_errors);
    }
    // Removes the socket file when dropped, after the editor quits.
    let _listener = match &args.listen {
        Some(spec) => match maestro_automation::cli::start_listener(&mut editor, spec) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("maestro: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    if !startup_errors.is_empty() {
        editor.set_status(Severity::Error, startup_errors.join("; "));
    }

    let opts = maestro_tui::RunOptions {
        exit_after_first_frame: args.first_frame_only,
    };
    match maestro_tui::run(editor, opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("maestro: terminal error: {e}");
            ExitCode::FAILURE
        }
    }
}
