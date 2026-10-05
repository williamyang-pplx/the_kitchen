//! External processes started by plugins: local or on the workspace's
//! devbox, with a timeout that kills the child (for devboxes, the ssh).

use std::io::{Read, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

use kitchen_core::host::Host;
use serde::Serialize;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub host: Host,
    /// argv, or `sh -c <script>` when built from `sh = "…"`.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub fn run(spec: &JobSpec) -> JobResult {
    let fail = |msg: String| JobResult { code: -1, stdout: String::new(), stderr: msg, timed_out: false };
    if spec.argv.is_empty() {
        return fail("empty command".into());
    }
    let argv: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
    let mut cmd = spec.host.command(&argv, spec.cwd.as_deref());
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("cannot start {}: {e}", spec.argv[0])),
    };
    if let (Some(input), Some(mut pipe)) = (spec.stdin.clone(), child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    // Drain both pipes on threads so a chatty child can't block on a full pipe.
    let drain = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code().unwrap_or(-1), false),
            Ok(None) if start.elapsed() > spec.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (-1, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return fail(format!("wait: {e}")),
        }
    };
    JobResult { code, stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default(), timed_out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_with_stdin_and_times_out() {
        let spec = |argv: &[&str], timeout| JobSpec {
            host: Host::Local,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            stdin: Some(b"hello".to_vec()),
            timeout,
        };
        let r = run(&spec(&["cat"], DEFAULT_TIMEOUT));
        assert_eq!((r.code, r.stdout.as_str(), r.timed_out), (0, "hello", false));
        let r = run(&spec(&["sleep", "5"], Duration::from_millis(200)));
        assert!(r.timed_out);
        assert!(run(&spec(&["definitely-not-a-command"], DEFAULT_TIMEOUT)).code != 0);
    }
}
