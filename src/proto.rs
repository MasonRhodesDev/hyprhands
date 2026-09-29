//! Framing on the wire (stdio, so it rides an ssh session unchanged).
//!
//! Request:  u32 big-endian length, then that many bytes of JSON: `{"op": "...", ...args}`.
//! Reply:    u32 big-endian length, then JSON: `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.
//!           When the JSON has `"blob": n`, exactly n raw bytes follow it (a capture), so images
//!           never pass through base64 or JSON.

use anyhow::{Result, bail};
use serde_json::Value;
use std::io::{ErrorKind, Read, Write};

const MAX_REQUEST: usize = 16 << 20;

/// The next request, or None at a clean end of input.
pub fn read_request(r: &mut impl Read) -> Result<Option<Value>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_REQUEST {
        bail!("request of {len} bytes is over the {MAX_REQUEST} limit");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

pub fn write_reply(w: &mut impl Write, mut header: Value, blob: Option<&[u8]>) -> Result<()> {
    if let Some(b) = blob {
        header["blob"] = b.len().into();
    }
    let body = serde_json::to_vec(&header)?;
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    if let Some(b) = blob {
        w.write_all(b)?;
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_round_trips_and_eof_ends_cleanly() {
        let body = serde_json::to_vec(&json!({"op": "state"})).unwrap();
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend(body);
        let mut r = wire.as_slice();
        assert_eq!(read_request(&mut r).unwrap(), Some(json!({"op": "state"})));
        assert_eq!(read_request(&mut r).unwrap(), None);
    }

    #[test]
    fn a_blob_follows_its_header_raw() {
        let mut out = Vec::new();
        write_reply(&mut out, json!({"ok": true}), Some(b"PIX")).unwrap();
        let len = u32::from_be_bytes(out[..4].try_into().unwrap()) as usize;
        let header: Value = serde_json::from_slice(&out[4..4 + len]).unwrap();
        assert_eq!(header["blob"], 3);
        assert_eq!(&out[4 + len..], b"PIX");
    }
}
