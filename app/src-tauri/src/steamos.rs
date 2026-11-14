//! Native launcher / Wine helper boundary. The key patcher runs inside the game's
//! Wine session; there is no host-side preload or network hook.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/forever-keypatch.exe"));

pub struct Launch {
    pub child: Child,
    pub log: PathBuf,
    pub runner_log: PathBuf,
}

pub fn windows_path(path: &Path) -> Result<String, String> {
    let absolute = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = absolute.to_str().ok_or("Wine requires a UTF-8 path")?;
    if !text.starts_with('/') || text.contains(['\\', '"', '\n', '\r']) {
        return Err(format!("Unsupported Wine path: {}", absolute.display()));
    }
    // Wine/Proton's standard Z: mapping. Paths are individual argv entries, never shell text.
    Ok(format!("Z:{}", text.replace('/', "\\")))
}

fn install_helper() -> Result<PathBuf, String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = crate::config::data_dir().join("bin");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let path = dir.join("forever-keypatch.exe");
    if std::fs::read(&path).ok().as_deref() == Some(HELPER) {
        return Ok(path);
    }
    // Atomic replacement: the helper may still be running from a previous launch.
    let temporary = dir.join("forever-keypatch.exe.new");
    let _ = std::fs::remove_file(&temporary);
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&temporary)
        .and_then(|mut f| std::io::Write::write_all(&mut f, HELPER))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Remove files created by older versions. Only known launcher-owned paths are touched.
fn cleanup_legacy() {
    let config = crate::config::config_dir();
    let _ = std::fs::remove_dir_all(config.join("runs"));
    for name in ["libforever_keypatch.so", "keypatch.log", "keystore-scan.txt"] {
        let _ = std::fs::remove_file(config.join(name));
    }
    if let Ok(entries) = std::fs::read_dir(&config) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("cache-cleared-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

pub fn prepare() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    cleanup_legacy();
    let helper = install_helper()?;
    let logs = crate::config::data_dir().join("logs");
    std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let log = logs.join("keypatch.log");
    let runner_log = logs.join("proton.log");
    for path in [&log, &runner_log] {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| e.to_string())?;
    }
    Ok((helper, log, runner_log))
}

#[derive(Default)]
struct Protocol {
    pending: String,
    started: bool,
    running: bool,
    closed: bool,
}

impl Protocol {
    fn feed(&mut self, text: &str, emit: &mut impl FnMut(&'static str, String)) -> bool {
        self.pending.push_str(text);
        let mut activity = false;
        while let Some(end) = self.pending.find('\n') {
            let line = self.pending[..end].trim_end_matches('\r').to_owned();
            self.pending.drain(..=end);
            let (kind, message) = line.split_once(' ').unwrap_or((&line, ""));
            match kind {
                "START" => {
                    self.started = true;
                    activity = true;
                    emit("log", line);
                }
                "HEARTBEAT" => activity = true,
                "PROCESS" => {
                    activity = true;
                    emit("log", line);
                    if !self.running {
                        self.running = true;
                        emit("running", crate::i18n::t("running").into());
                    }
                }
                "READY" => {
                    activity = true;
                    emit("log", line);
                    emit("ready", crate::i18n::t("ready").into());
                }
                "WARN" => {
                    activity = true;
                    emit("warning", message.into());
                }
                "SCAN" | "EXIT" => {
                    activity = true;
                    emit("log", line);
                }
                "ERROR" => {
                    self.closed = true;
                    emit("error", message.into());
                }
                "CLOSED" => {
                    self.closed = true;
                    emit("closed", crate::i18n::t("closed").into());
                }
                _ => emit("log", line),
            }
        }
        // A broken helper must not grow launcher memory without bound.
        if self.pending.len() > 8192 {
            self.pending.clear();
        }
        activity
    }
}

/// True only when a terminal helper record confirms that the launch has ended.
pub fn watch(mut launch: Launch, mut emit: impl FnMut(&'static str, String)) -> bool {
    let mut protocol = Protocol::default();
    let began = Instant::now();
    let mut activity = began;
    let mut offset = 0;
    loop {
        // Reopen so that helper creation/flush timing and a partial last line are harmless.
        if let Ok(mut file) = File::open(&launch.log) {
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let mut bytes = Vec::new();
                if file.take(64 * 1024).read_to_end(&mut bytes).is_ok() {
                    offset += bytes.len() as u64;
                    if protocol.feed(&String::from_utf8_lossy(&bytes), &mut emit) {
                        activity = Instant::now();
                    }
                }
            }
        }
        if protocol.closed {
            break;
        }
        // Reap the wrapper, but do NOT interpret its exit as the game's exit.
        let _ = launch.child.try_wait();
        let timed_out = if protocol.started {
            activity.elapsed() > Duration::from_secs(30)
        } else {
            began.elapsed() > Duration::from_secs(60)
        };
        if timed_out {
            emit(
                "warning",
                format!(
                    "{}\n{}\n{}",
                    crate::i18n::t("steamos_helper_timeout"),
                    launch.log.display(),
                    launch.runner_log.display()
                ),
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    // Proton can wait for wineserver shutdown after CLOSED. Reap without blocking the UI.
    std::thread::spawn(move || {
        let _ = launch.child.wait();
    });
    protocol.closed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_records_and_wrapper_exit_do_not_close_the_game() {
        let mut p = Protocol::default();
        let mut events = Vec::new();
        p.feed("START protocol=1\nPROCESS pid=12\nREA", &mut |k, s| {
            events.push((k, s))
        });
        assert!(p.running && !p.closed);
        assert!(!events.iter().any(|(k, _)| *k == "ready"));
        p.feed(
            "DY verified=1\nWARN unknown layout\nHEARTBEAT 1\n",
            &mut |k, s| events.push((k, s)),
        );
        assert!(events.iter().any(|(k, _)| *k == "ready"));
        assert!(events.iter().any(|(k, _)| *k == "warning"));
        assert!(!p.closed);
        p.feed("CLOSED exit=0\n", &mut |k, s| events.push((k, s)));
        assert!(p.closed);
        assert_eq!(events.last().unwrap().0, "closed");
    }

    #[test]
    fn helper_errors_are_visible() {
        let mut p = Protocol::default();
        let mut events = Vec::new();
        p.feed("ERROR access denied\n", &mut |k, s| events.push((k, s)));
        assert!(p.closed);
        assert_eq!(events, vec![("error", "access denied".into())]);
    }

    #[test]
    fn watcher_ignores_wrapper_exit_and_waits_for_helper_closed() {
        let dir = std::env::temp_dir().join(format!(
            "forever-watch-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("keypatch.log");
        std::fs::write(&log, "START protocol=1\nPROCESS pid=12\n").unwrap();
        let child = std::process::Command::new("true").spawn().unwrap();
        let writer_log = log.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            use std::io::Write;
            let mut f = OpenOptions::new().append(true).open(writer_log).unwrap();
            f.write_all(b"READY verified=1\nCLOSED exit=0\n").unwrap();
        });
        let launch = Launch {
            child,
            log,
            runner_log: dir.join("proton.log"),
        };
        let mut events = Vec::new();
        assert!(watch(launch, |k, s| events.push((k, s))));
        writer.join().unwrap();
        assert_eq!(events.last().unwrap().0, "closed");
        assert!(events.iter().any(|(k, _)| *k == "ready"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn paths_with_spaces_and_unicode_are_single_windows_paths() {
        let dir = std::env::temp_dir().join(format!(
            "forever-path-{}-Игра с пробелами",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = windows_path(&dir).unwrap();
        assert!(path.starts_with("Z:\\"));
        assert!(path.ends_with("Игра с пробелами"));
        std::fs::remove_dir(&dir).unwrap();
    }
}
