//! Self-detaching load. A load closes every window on the desktop, including the terminal
//! the user typed `hyprsession load` in; the terminal's SIGHUP would kill the load half way.
//! So the process that was started re-executes itself in a new session (no controlling
//! terminal, SIGHUP ignored, output to a log file) and only follows the log. Closing the
//! terminal ends the follower, never the load.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Set in the detached copy so it does not detach again
pub const DETACHED_ENV: &str = "HYPRSESSION_DETACHED";
/// Set to run a load in the foreground (debugging)
pub const NO_DETACH_ENV: &str = "HYPRSESSION_NO_DETACH";

/// Whether this process should detach: it has not, and nobody asked it not to
pub fn should_detach() -> bool {
    std::env::var_os(DETACHED_ENV).is_none() && std::env::var_os(NO_DETACH_ENV).is_none()
}

/// Start `cmd` in its own session with SIGHUP ignored, stdin closed and stdout/stderr in `log`
pub fn spawn_detached(mut cmd: Command, log: &Path) -> std::io::Result<Child> {
    let out = File::create(log)?;
    cmd.env(DETACHED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    // SAFETY: only async-signal-safe calls (setsid, signal) between fork and exec
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    cmd.spawn()
}

/// Copy the log to `out` as it grows until the child exits; returns the child's exit code.
/// Write errors (the terminal went away) are ignored: the load does not depend on this process.
pub fn follow(child: &mut Child, log: &Path, out: &mut dyn Write) -> i32 {
    let mut position = 0u64;
    let pump = |position: &mut u64, out: &mut dyn Write| {
        if let Ok(mut file) = File::open(log) {
            if file.seek(SeekFrom::Start(*position)).is_ok() {
                let mut chunk = vec![];
                if file.read_to_end(&mut chunk).is_ok() && !chunk.is_empty() {
                    *position += chunk.len() as u64;
                    let _ = out.write_all(&chunk);
                    let _ = out.flush();
                }
            }
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                pump(&mut position, out);
                return status.code().unwrap_or(1);
            }
            Ok(None) => {
                pump(&mut position, out);
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid_of(pid: u32) -> String {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat[stat.rfind(')').unwrap() + 1..]
            .split_whitespace()
            .nth(3)
            .unwrap()
            .to_string()
    }

    #[test]
    fn test_the_detached_copy_leads_its_own_session_and_ignores_sighup() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("load.log");
        let mut cmd = Command::new("sh");
        // report its session id, whether HUP is ignored (SigIgn bit 0 in /proc/self/status), and the marker
        cmd.arg("-c").arg(
            r#"awk '{print "sid", $6}' /proc/self/stat; grep SigIgn /proc/self/status; echo "env=$HYPRSESSION_DETACHED""#,
        );
        let mut child = spawn_detached(cmd, &log).unwrap();
        let pid = child.id();
        let mut shown = vec![];
        let code = follow(&mut child, &log, &mut shown);
        assert_eq!(code, 0);
        let text = String::from_utf8(shown).unwrap();
        assert!(
            text.contains(&format!("sid {pid}")),
            "its own session leader: {text}"
        );
        assert_ne!(sid_of(std::process::id()), pid.to_string());
        let ignored = text.lines().find(|l| l.starts_with("SigIgn")).unwrap();
        let mask = u64::from_str_radix(ignored.split_whitespace().nth(1).unwrap(), 16).unwrap();
        assert!(mask & 1 != 0, "SIGHUP is ignored: {ignored}");
        assert!(text.contains("env=1"));
        // the log holds the same text
        assert_eq!(std::fs::read_to_string(&log).unwrap(), text);
    }

    #[test]
    fn test_follow_returns_the_childs_exit_code_and_survives_a_closed_output() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("load.log");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello; exit 3");
        let mut child = spawn_detached(cmd, &log).unwrap();
        assert_eq!(follow(&mut child, &log, &mut Closed), 3);
    }

    #[test]
    fn test_detach_can_be_switched_off_and_does_not_recurse() {
        // pure environment logic; the two variables are only read here and in main
        let detached =
            std::env::var_os(DETACHED_ENV).is_some() || std::env::var_os(NO_DETACH_ENV).is_some();
        assert_eq!(should_detach(), !detached);
    }
}
