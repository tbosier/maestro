//! The base protocol's message framing: a `Content-Length` header block,
//! an empty line, then exactly that many bytes of UTF-8 JSON.

use std::io::{self, BufRead, Read, Write};

use serde_json::Value;

/// Bodies larger than this are refused, so a broken or hostile server
/// cannot make the editor allocate without bound.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// Header lines are short; a longer one means a broken stream.
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Writes one framed message and flushes.
pub fn write_message(out: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

/// Reads one framed message. Returns `Ok(None)` on a clean end of stream
/// between messages.
pub fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    let mut saw_header = false;
    loop {
        line.clear();
        let n = input
            .by_ref()
            .take(MAX_HEADER_LINE_BYTES as u64)
            .read_line(&mut line)?;
        if n == MAX_HEADER_LINE_BYTES && !line.ends_with('\n') {
            return Err(invalid(format!(
                "header line longer than {MAX_HEADER_LINE_BYTES} bytes"
            )));
        }
        if n == 0 {
            if saw_header {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a header block",
                ));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if saw_header {
                break;
            }
            // Tolerate blank lines between messages.
            continue;
        }
        saw_header = true;
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(invalid(format!("malformed header line {trimmed:?}")));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| invalid(format!("bad Content-Length {:?}", value.trim())))?;
            length = Some(n);
        }
        // Content-Type and unknown headers are ignored.
    }
    let n = length.ok_or_else(|| invalid("header block without Content-Length".into()))?;
    if n > MAX_MESSAGE_BYTES {
        return Err(invalid(format!(
            "message of {n} bytes exceeds the {MAX_MESSAGE_BYTES} byte limit"
        )));
    }
    let mut body = vec![0u8; n];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| invalid(format!("invalid JSON body: {e}")))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_several_messages() {
        let msgs = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
            json!({"jsonrpc": "2.0", "method": "x", "params": {"text": "a😀é\r\nb"}}),
            json!({"jsonrpc": "2.0", "id": "s", "result": null}),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_message(&mut buf, m).unwrap();
        }
        let mut r = io::BufReader::new(&buf[..]);
        for m in &msgs {
            assert_eq!(read_message(&mut r).unwrap().as_ref(), Some(m));
        }
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn content_length_counts_bytes_not_chars() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!("😀")).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // "😀" is 4 bytes plus two quotes.
        assert!(s.starts_with("Content-Length: 6\r\n\r\n"), "{s}");
    }

    #[test]
    fn headers_are_case_insensitive_and_extra_headers_ignored() {
        let raw = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        let mut r = io::BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut r).unwrap(), Some(json!({})));
    }

    #[test]
    fn rejects_missing_length_truncation_and_oversize() {
        let mut r = io::BufReader::new(&b"Content-Type: x\r\n\r\n{}"[..]);
        assert!(read_message(&mut r).is_err());
        let mut r = io::BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        assert!(read_message(&mut r).is_err());
        let big = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        let mut r = io::BufReader::new(big.as_bytes());
        assert!(read_message(&mut r).is_err());
        let mut r = io::BufReader::new(&b"Content-Length: 2\r\n"[..]);
        assert!(read_message(&mut r).is_err());
        // A header line that never ends is refused without reading it all.
        let endless = vec![b'x'; MAX_HEADER_LINE_BYTES * 4];
        let mut r = io::BufReader::new(&endless[..]);
        let err = read_message(&mut r).unwrap_err();
        assert!(err.to_string().contains("header line longer"), "{err}");
    }
}
