//! `kitchenctl __relay`: the devbox end of the API tunnel.
//!
//! The devbox ssh gateway refuses port and socket forwarding, so the app runs
//! this over a plain `ssh <alias> kitchenctl __relay`. It listens on the
//! devbox's `~/.kitchen/kitchen.sock` (so `kitchenctl` there works exactly as
//! it does on the mac), wraps every request line in a [`RelayFrame`] on
//! stdout, and routes response frames from stdin back to the right client.
//! When stdin closes (the app went away or ssh dropped) the relay removes its
//! socket and exits.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kitchen_proto::RelayFrame;

/// rid 0 carries the hello line; requests start at 1.
pub const HELLO_RID: u64 = 0;

type Pending = Arc<Mutex<HashMap<u64, Sender<String>>>>;

/// Run the relay until `input` reaches EOF.
pub fn run<R, W>(socket: &Path, input: R, output: W) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A newer relay always wins: an older one (from a dropped ssh session)
    // exits on its own once its stdin closes.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).with_context(|| format!("bind {}", socket.display()))?;
    let our_ino = listener_ino(socket);

    let out = Arc::new(Mutex::new(output));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let next = Arc::new(AtomicU64::new(1));

    let hello = serde_json::json!({ "relay": env!("CARGO_PKG_VERSION"), "protocol": kitchen_proto::PROTOCOL_VERSION });
    write_frame(&out, &RelayFrame { rid: HELLO_RID, line: hello.to_string() })?;

    {
        let (out, pending, next) = (out.clone(), pending.clone(), next.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (out, pending, next) = (out.clone(), pending.clone(), next.clone());
                std::thread::spawn(move || serve_client(stream, &out, &pending, &next));
            }
        });
    }

    for line in input.lines() {
        let Ok(line) = line else { break };
        let Ok(frame) = serde_json::from_str::<RelayFrame>(&line) else { continue };
        if let Some(tx) = pending.lock().unwrap().remove(&frame.rid) {
            let _ = tx.send(frame.line);
        }
    }
    // Wake clients still waiting: their answers will never come.
    pending.lock().unwrap().clear();
    // Only remove the socket if it is still ours (a newer relay may own it now).
    remove_if_ours(socket, &our_ino);
    Ok(())
}

fn serve_client<W: Write>(stream: UnixStream, out: &Arc<Mutex<W>>, pending: &Pending, next: &AtomicU64) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut writer = stream;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let rid = next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        pending.lock().unwrap().insert(rid, tx);
        if write_frame(out, &RelayFrame { rid, line }).is_err() {
            pending.lock().unwrap().remove(&rid);
            let _ = writeln!(writer, "{}", error_response("relay lost its connection to the kitchen app"));
            break;
        }
        let reply = match rx.recv() {
            Ok(r) => r,
            Err(_) => error_response("the kitchen app disconnected before answering"),
        };
        if writeln!(writer, "{reply}").is_err() {
            break;
        }
    }
}

fn write_frame<W: Write>(out: &Arc<Mutex<W>>, frame: &RelayFrame) -> Result<()> {
    let mut s = serde_json::to_string(frame)?;
    s.push('\n');
    let mut w = out.lock().unwrap();
    w.write_all(s.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn error_response(msg: &str) -> String {
    serde_json::to_string(&kitchen_proto::Response::err(0, msg)).unwrap_or_default()
}

fn listener_ino(socket: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(socket).ok().map(|m| m.ino())
}

fn remove_if_ours(socket: &Path, ours: &Option<u64>) {
    if ours.is_some() && listener_ino(socket) == *ours {
        let _ = std::fs::remove_file(socket);
    }
}

pub fn default_socket() -> PathBuf {
    kitchen_proto::default_socket_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitchen_proto::{Request, Response};

    /// Full loop: a client on the relay socket, the "app" answering frames.
    #[test]
    fn frames_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("kitchen.sock");
        let (app_reads, relay_writes) = std::io::pipe().unwrap();
        let (relay_reads, mut app_writes) = std::io::pipe().unwrap();
        let sock2 = sock.clone();
        let relay = std::thread::spawn(move || run(&sock2, BufReader::new(relay_reads), relay_writes));

        let mut app_in = BufReader::new(app_reads);
        let mut hello = String::new();
        app_in.read_line(&mut hello).unwrap();
        let hello: RelayFrame = serde_json::from_str(&hello).unwrap();
        assert_eq!(hello.rid, HELLO_RID);

        // Fake app: echo the method name back as the result.
        std::thread::spawn(move || {
            for line in app_in.lines() {
                let frame: RelayFrame = serde_json::from_str(&line.unwrap()).unwrap();
                let req: Request = serde_json::from_str(&frame.line).unwrap();
                let resp = Response::ok(req.id, serde_json::json!({"method": req.method, "tab": req.caller.tab_id}));
                let out = RelayFrame { rid: frame.rid, line: serde_json::to_string(&resp).unwrap() };
                writeln!(app_writes, "{}", serde_json::to_string(&out).unwrap()).unwrap();
            }
        });

        // Two concurrent clients, two requests each.
        let clients: Vec<_> = (0..2)
            .map(|i| {
                let sock = sock.clone();
                std::thread::spawn(move || {
                    let s = UnixStream::connect(&sock).unwrap();
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut w = s;
                    for j in 0..2 {
                        let req = Request {
                            id: j,
                            method: format!("m{i}.{j}"),
                            params: serde_json::Value::Null,
                            caller: kitchen_proto::Caller { tab_id: Some(format!("t{i}")), ..Default::default() },
                        };
                        writeln!(w, "{}", serde_json::to_string(&req).unwrap()).unwrap();
                        let mut line = String::new();
                        r.read_line(&mut line).unwrap();
                        let resp: Response = serde_json::from_str(&line).unwrap();
                        assert!(resp.ok);
                        assert_eq!(resp.result["method"], format!("m{i}.{j}"));
                        assert_eq!(resp.result["tab"], format!("t{i}"));
                    }
                })
            })
            .collect();
        for c in clients {
            c.join().unwrap();
        }
        drop(relay); // the relay thread lives until the fake app's pipe closes
    }

    #[test]
    fn stdin_eof_removes_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("k.sock");
        std::fs::write(&sock, "stale").unwrap();
        run(&sock, BufReader::new(std::io::empty()), std::io::sink()).unwrap();
        assert!(!sock.exists());
    }
}
