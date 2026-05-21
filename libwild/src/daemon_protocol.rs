//! Wire protocol for the wild linker daemon.
//!
//! Lives in its own dependency-free file so the thin `wild-client`
//! binary can include it via `#[path = "..."]` without pulling in the
//! rest of libwild. Errors are plain [`std::io::Error`] — the caller
//! decides whether to wrap them in a richer type.
//!
//! Request (client → server):
//!   `[u32 argc][per arg: u32 len + bytes]`
//!   `[u32 envc][per (k,v): u32 klen + bytes + u32 vlen + bytes]`
//!   `[u32 cwd_len][cwd bytes]`
//!
//! Response (server → client):
//!   `[u32 stderr_len][stderr bytes]`
//!   `[u32 stdout_len][stdout bytes]`
//!   `[i32 exit_code]`
//!
//! All multi-byte integers are little-endian.

use std::io::Error;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Result;
use std::io::Write;
use std::path::PathBuf;

pub struct Request {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

pub struct Response {
    pub stderr_bytes: Vec<u8>,
    pub stdout_bytes: Vec<u8>,
    pub exit_code: i32,
}

pub const MAX_ARG_LEN: usize = 64 * 1024;
pub const MAX_ENV_LEN: usize = 64 * 1024;
pub const MAX_PATH_LEN: usize = 64 * 1024;
pub const MAX_ARGS: usize = 1 << 16;
pub const MAX_ENV_ENTRIES: usize = 1 << 16;
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

fn invalid<T: Into<Box<dyn std::error::Error + Send + Sync>>>(msg: T) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

fn read_lp_bytes<R: Read>(r: &mut R, cap: usize, what: &str) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .map_err(|e| invalid(format!("daemon: short read on {what} length: {e}")))?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > cap {
        return Err(invalid(format!(
            "daemon: {what} length {len} exceeds cap {cap}"
        )));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .map_err(|e| invalid(format!("daemon: short read on {what} body ({len}b): {e}")))?;
    Ok(buf)
}

fn read_lp_string<R: Read>(r: &mut R, cap: usize, what: &str) -> Result<String> {
    let bytes = read_lp_bytes(r, cap, what)?;
    String::from_utf8(bytes).map_err(|_| invalid(format!("daemon: {what} not valid UTF-8")))
}

fn write_lp_bytes<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| invalid("daemon: payload exceeds u32::MAX bytes"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

pub fn read_request<R: Read>(r: &mut R) -> Result<Request> {
    let mut count_buf = [0u8; 4];
    r.read_exact(&mut count_buf)
        .map_err(|e| invalid(format!("daemon: short read on argc: {e}")))?;
    let argc = u32::from_le_bytes(count_buf) as usize;
    if argc > MAX_ARGS {
        return Err(invalid(format!(
            "daemon: argc {argc} exceeds cap {MAX_ARGS}"
        )));
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        argv.push(read_lp_string(r, MAX_ARG_LEN, "argv entry")?);
    }

    r.read_exact(&mut count_buf)
        .map_err(|e| invalid(format!("daemon: short read on envc: {e}")))?;
    let envc = u32::from_le_bytes(count_buf) as usize;
    if envc > MAX_ENV_ENTRIES {
        return Err(invalid(format!(
            "daemon: envc {envc} exceeds cap {MAX_ENV_ENTRIES}"
        )));
    }
    let mut env = Vec::with_capacity(envc);
    for _ in 0..envc {
        let k = read_lp_string(r, MAX_ENV_LEN, "env key")?;
        let v = read_lp_string(r, MAX_ENV_LEN, "env value")?;
        env.push((k, v));
    }

    let cwd = read_lp_string(r, MAX_PATH_LEN, "cwd")?;
    Ok(Request {
        argv,
        env,
        cwd: PathBuf::from(cwd),
    })
}

pub fn write_request<W: Write>(w: &mut W, req: &Request) -> Result<()> {
    let argc = u32::try_from(req.argv.len())
        .map_err(|_| invalid("daemon: argv exceeds u32::MAX entries"))?;
    w.write_all(&argc.to_le_bytes())?;
    for arg in &req.argv {
        write_lp_bytes(w, arg.as_bytes())?;
    }
    let envc = u32::try_from(req.env.len())
        .map_err(|_| invalid("daemon: env exceeds u32::MAX entries"))?;
    w.write_all(&envc.to_le_bytes())?;
    for (k, v) in &req.env {
        write_lp_bytes(w, k.as_bytes())?;
        write_lp_bytes(w, v.as_bytes())?;
    }
    write_lp_bytes(w, req.cwd.as_os_str().as_encoded_bytes())?;
    w.flush()?;
    Ok(())
}

pub fn read_response<R: Read>(r: &mut R) -> Result<Response> {
    let stderr_bytes = read_lp_bytes(r, MAX_STREAM_BYTES, "stderr")?;
    let stdout_bytes = read_lp_bytes(r, MAX_STREAM_BYTES, "stdout")?;
    let mut code_buf = [0u8; 4];
    r.read_exact(&mut code_buf)
        .map_err(|e| invalid(format!("daemon: short read on exit code: {e}")))?;
    Ok(Response {
        stderr_bytes,
        stdout_bytes,
        exit_code: i32::from_le_bytes(code_buf),
    })
}

pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    write_lp_bytes(w, &resp.stderr_bytes)?;
    write_lp_bytes(w, &resp.stdout_bytes)?;
    w.write_all(&resp.exit_code.to_le_bytes())?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_round_trip() {
        let req = Request {
            argv: vec!["wild".into(), "-o".into(), "/tmp/out".into()],
            env: vec![("A".into(), "1".into()), ("B".into(), "two".into())],
            cwd: PathBuf::from("/proj"),
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_request(&mut c).unwrap();
        assert_eq!(r.argv, req.argv);
        assert_eq!(r.env, req.env);
        assert_eq!(r.cwd, req.cwd);
    }

    #[test]
    fn response_round_trip() {
        let resp = Response {
            stderr_bytes: b"oops\n".to_vec(),
            stdout_bytes: b"ok\n".to_vec(),
            exit_code: 7,
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_response(&mut c).unwrap();
        assert_eq!(r.stderr_bytes, resp.stderr_bytes);
        assert_eq!(r.stdout_bytes, resp.stdout_bytes);
        assert_eq!(r.exit_code, resp.exit_code);
    }

    #[test]
    fn rejects_argc_overflow() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut c = Cursor::new(&buf);
        assert!(read_request(&mut c).is_err());
    }
}
