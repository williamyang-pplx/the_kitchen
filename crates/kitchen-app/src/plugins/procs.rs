//! Long-lived processes started by plugins (`kitchen.spawn`): language
//! servers, watchers, REPLs. Output streams back to the plugin's thread as
//! messages; writes go through a writer thread so a child that stops reading
//! can never block plugin code. Dropping a [`Proc`] kills the child (for
//! devboxes, the ssh — the remote side then sees EOF on stdin).

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kitchen_core::host::Host;

use super::ToPlugin;

/// Most live processes per plugin.
pub const MAX_PROCS: usize = 8;
const CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

pub struct ProcSpec {
    pub host: Host,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
}

pub struct Proc {
    child: Arc<Mutex<Child>>,
    stdin: Option<Sender<Vec<u8>>>,
}

impl Proc {
    /// Start `spec`; output and exit arrive on `tx` tagged with `id`/`generation`.
    pub fn spawn(spec: &ProcSpec, id: u64, generation: u64, tx: Sender<ToPlugin>) -> Result<Proc, String> {
        if spec.argv.is_empty() {
            return Err("empty command".into());
        }
        let argv: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
        let mut cmd = spec.host.command(&argv, spec.cwd.as_deref());
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", spec.argv[0]))?;
        let (win, rin) = channel::<Vec<u8>>();
        if let Some(mut pipe) = child.stdin.take() {
            std::thread::spawn(move || {
                for data in rin {
                    if pipe.write_all(&data).and_then(|_| pipe.flush()).is_err() {
                        break;
                    }
                }
            });
        }
        let stderr = child.stderr.take();
        let stdout = child.stdout.take();
        let child = Arc::new(Mutex::new(child));
        let pump = |pipe: Option<Box<dyn Read + Send>>, stream: Stream, tx: Sender<ToPlugin>| {
            std::thread::spawn(move || {
                let Some(mut pipe) = pipe else { return };
                let mut buf = vec![0u8; CHUNK];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(ToPlugin::ProcOut { generation, proc: id, stream, data: buf[..n].to_vec() }).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
        };
        let err = pump(stderr.map(|p| Box::new(p) as Box<dyn Read + Send>), Stream::Stderr, tx.clone());
        let out = pump(stdout.map(|p| Box::new(p) as Box<dyn Read + Send>), Stream::Stdout, tx.clone());
        let waiter = child.clone();
        std::thread::spawn(move || {
            let _ = out.join();
            let _ = err.join();
            // Poll rather than wait() so kill() can take the lock meanwhile.
            let code = loop {
                match super::lock(&waiter).try_wait() {
                    Ok(Some(s)) => break s.code().unwrap_or(-1),
                    Ok(None) => {}
                    Err(_) => break -1,
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let _ = tx.send(ToPlugin::ProcExit { generation, proc: id, code });
        });
        Ok(Proc { child, stdin: Some(win) })
    }

    pub fn write(&self, data: Vec<u8>) -> bool {
        self.stdin.as_ref().is_some_and(|s| s.send(data).is_ok())
    }

    /// Close stdin (servers that exit on EOF then exit cleanly).
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub fn kill(&self) {
        let mut c = super::lock(&self.child);
        let _ = c.kill();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.stdin = None;
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_and_exits() {
        let (tx, rx) = channel();
        let spec = ProcSpec { host: Host::Local, argv: vec!["sh".into(), "-c".into(), "read x; echo got $x; echo oops >&2; exit 3".into()], cwd: None };
        let p = Proc::spawn(&spec, 7, 1, tx).unwrap();
        assert!(p.write(b"hi\n".to_vec()));
        let (mut out, mut err, mut code) = (Vec::new(), Vec::new(), None);
        while code.is_none() {
            match rx.recv_timeout(Duration::from_secs(10)).expect("message") {
                ToPlugin::ProcOut { stream: Stream::Stdout, data, proc: 7, .. } => out.extend(data),
                ToPlugin::ProcOut { stream: Stream::Stderr, data, .. } => err.extend(data),
                ToPlugin::ProcExit { code: c, .. } => code = Some(c),
                _ => {}
            }
        }
        assert_eq!((out.as_slice(), err.as_slice(), code), (&b"got hi\n"[..], &b"oops\n"[..], Some(3)));
    }

    #[test]
    fn drop_kills() {
        let (tx, rx) = channel();
        let spec = ProcSpec { host: Host::Local, argv: vec!["sleep".into(), "30".into()], cwd: None };
        let p = Proc::spawn(&spec, 1, 1, tx).unwrap();
        drop(p);
        loop {
            if let ToPlugin::ProcExit { .. } = rx.recv_timeout(Duration::from_secs(5)).expect("exit after kill") {
                break;
            }
        }
    }
}
