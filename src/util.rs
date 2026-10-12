//! Helpers shared by several commands: running system tools with a time limit
//! and moving things to the Trash (recoverable) instead of deleting them.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Cmd {
    pub ok: bool,
    pub timed_out: bool,
    pub stdout: String,
}

impl Cmd {
    fn failed() -> Self {
        Cmd { ok: false, timed_out: false, stdout: String::new() }
    }
}

/// Run a program and capture its output, killing it after `timeout`.
/// Output is drained on separate threads so a chatty tool cannot deadlock on a
/// full pipe. A program that cannot start yields `ok == false`.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Cmd {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Cmd::failed();
    };
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            s.push_str(&String::from_utf8_lossy(&buf));
            s
        })
    };
    let out_t = child.stdout.take().map(|s| drain(Box::new(s)));

    let start = Instant::now();
    let (ok, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.success(), false),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break (false, false),
        }
    };
    Cmd {
        ok,
        timed_out,
        stdout: out_t.and_then(|t| t.join().ok()).unwrap_or_default(),
    }
}

/// Move `path` into `~/.Trash` (same volume rename, so it is instant and
/// recoverable from Finder). Returns where it ended up.
pub fn move_to_trash(path: &Path, home: &Path) -> std::io::Result<PathBuf> {
    let trash = home.join(".Trash");
    std::fs::create_dir_all(&trash)?;
    let name = path.file_name().ok_or_else(|| std::io::Error::other("path has no file name"))?;
    let mut dest = trash.join(name);
    if dest.symlink_metadata().is_ok() {
        // Like Finder: keep both, disambiguate with a timestamp.
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let stem = path.file_stem().unwrap_or(name).to_string_lossy().to_string();
        let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        dest = trash.join(format!("{stem} {secs}{ext}"));
    }
    std::fs::rename(path, &dest).map_err(|e| {
        if e.raw_os_error() == Some(libc::EXDEV) {
            std::io::Error::other("on a different volume than your home; move it to the Trash from Finder")
        } else {
            e
        }
    })?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_status() {
        let c = run("/bin/echo", &["hello"], Duration::from_secs(5));
        assert!(c.ok);
        assert_eq!(c.stdout.trim(), "hello");
        assert!(!run("/usr/bin/false", &[], Duration::from_secs(5)).ok);
    }

    #[test]
    fn missing_program_is_a_failure_not_a_panic() {
        assert!(!run("/definitely/not/here", &[], Duration::from_secs(1)).ok);
    }

    #[test]
    fn slow_command_is_killed_at_the_deadline() {
        let t = Instant::now();
        let c = run("/bin/sleep", &["30"], Duration::from_millis(300));
        assert!(c.timed_out && !c.ok);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // ~1 MB, far above a 64 KB pipe buffer.
        let c = run("/usr/bin/yes", &[], Duration::from_millis(300));
        assert!(c.stdout.len() > 100_000);
    }

    #[test]
    fn trash_moves_and_disambiguates() {
        let home = std::env::temp_dir().join(format!("sweepr-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("a")).unwrap();
        std::fs::write(home.join("a/f.txt"), "1").unwrap();
        let d1 = move_to_trash(&home.join("a/f.txt"), &home).unwrap();
        assert!(d1.exists() && !home.join("a/f.txt").exists());
        std::fs::write(home.join("a/f.txt"), "2").unwrap();
        let d2 = move_to_trash(&home.join("a/f.txt"), &home).unwrap();
        assert_ne!(d1, d2);
        assert_eq!(std::fs::read_to_string(d1).unwrap(), "1"); // first one untouched
        let _ = std::fs::remove_dir_all(&home);
    }
}
