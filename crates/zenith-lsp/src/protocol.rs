//! JSON-RPC framing: `Content-Length: N\r\n\r\n<json>`.

use std::io::{self, BufRead, Write};

use serde_json::Value;

pub fn write_message(w: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one message. `Ok(None)` at end of stream.
pub fn read_message(r: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0u8; len.unwrap_or(0)];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"a": "салом"})).unwrap();
        write_message(&mut buf, &json!({"b": 2})).unwrap();
        let mut r = io::BufReader::new(&buf[..]);
        assert_eq!(read_message(&mut r).unwrap().unwrap()["a"], "салом");
        assert_eq!(read_message(&mut r).unwrap().unwrap()["b"], 2);
        assert!(read_message(&mut r).unwrap().is_none());
    }
}
