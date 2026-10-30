//! System clipboard providers.
//!
//! Uses the platform's command-line tools when present (`pbcopy` on macOS,
//! `wl-copy` on Wayland, `xclip` or `xsel` on X11). Otherwise copying uses
//! the OSC 52 escape sequence, which works over SSH and in tmux with
//! `set-clipboard on`, and pasting falls back to text copied in this session.

use std::io::Write;
use std::process::{Command, Stdio};

use maestro_editor::registers::Clipboard;

pub struct CommandClipboard {
    name: String,
    copy: Vec<String>,
    paste: Vec<String>,
}

fn has(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(cmd).is_file()))
}

fn argv(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

pub fn detect() -> Box<dyn Clipboard> {
    let env = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let pick = |name: &str, copy: &[&str], paste: &[&str]| -> Box<dyn Clipboard> {
        Box::new(CommandClipboard {
            name: name.to_string(),
            copy: argv(copy),
            paste: argv(paste),
        })
    };
    if cfg!(target_os = "macos") && has("pbcopy") {
        return pick("pbcopy", &["pbcopy"], &["pbpaste"]);
    }
    if env("WAYLAND_DISPLAY") && has("wl-copy") && has("wl-paste") {
        return pick(
            "wl-copy",
            &["wl-copy", "--type", "text/plain"],
            &["wl-paste", "--no-newline"],
        );
    }
    if env("DISPLAY") && has("xclip") {
        return pick(
            "xclip",
            &["xclip", "-selection", "clipboard", "-in"],
            &["xclip", "-selection", "clipboard", "-out"],
        );
    }
    if env("DISPLAY") && has("xsel") {
        return pick(
            "xsel",
            &["xsel", "--clipboard", "--input"],
            &["xsel", "--clipboard", "--output"],
        );
    }
    Box::new(Osc52Clipboard::default())
}

impl Clipboard for CommandClipboard {
    fn get(&mut self) -> Result<String, String> {
        let out = Command::new(&self.paste[0])
            .args(&self.paste[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", self.paste[0]))?;
        if !out.status.success() {
            return Err(format!("{} exited with {}", self.paste[0], out.status));
        }
        String::from_utf8(out.stdout).map_err(|_| "clipboard is not UTF-8 text".into())
    }

    fn set(&mut self, text: &str) -> Result<(), String> {
        let mut child = Command::new(&self.copy[0])
            .args(&self.copy[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{}: {e}", self.copy[0]))?;
        child
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
        let status = child.wait().map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{} exited with {status}", self.copy[0]))
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Default)]
pub struct Osc52Clipboard {
    last: String,
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

impl Clipboard for Osc52Clipboard {
    fn get(&mut self) -> Result<String, String> {
        // Reading via OSC 52 needs a terminal round trip most terminals
        // disable; use what was copied in this session.
        Ok(self.last.clone())
    }

    fn set(&mut self, text: &str) -> Result<(), String> {
        self.last = text.to_string();
        let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let mut out = std::io::stdout();
        out.write_all(seq.as_bytes()).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())
    }

    fn name(&self) -> &str {
        "OSC 52"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        // RFC 4648 test vectors.
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }
}
