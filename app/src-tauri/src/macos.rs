//! macOS: the official client is signed with the hardened runtime, so its memory cannot be written from outside
//! (task_for_pid is refused) and its binary does not hold the server keys in clear. The launcher keeps a copy of
//! the game app next to the original, re-signed ad hoc without the hardened runtime, and starts it with
//! keypatch.dylib (app/macos/keypatch.c) loaded: the library writes the server key from inside the game and
//! logs what it does to a file this module follows. The login ticket goes to the net.battle preferences.

use crate::config::{config_dir, Settings, MAC_COPY_NAME};
use crate::game::WTF_NAME;
use crate::i18n::{t, tf};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, UNIX_EPOCH};

const DYLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libforever_keypatch.dylib"));

/// Those of the official app, plus what the copy needs: no library validation and DYLD_* variables allowed
/// (to load keypatch.dylib), get-task-allow (debugging).
const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.cs.allow-jit</key><true/>
<key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
<key>com.apple.security.cs.disable-executable-page-protection</key><true/>
<key>com.apple.security.cs.disable-library-validation</key><true/>
<key>com.apple.security.cs.allow-dyld-environment-variables</key><true/>
<key>com.apple.security.device.audio-input</key><true/>
<key>com.apple.security.get-task-allow</key><true/>
</dict></plist>
"#;

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let out = Command::new(program).args(args).output().map_err(|e| format!("{program} : {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(tf("mac_failed", &[&program, &String::from_utf8_lossy(&out.stderr).trim()]))
    }
}

fn os(p: &Path) -> &std::ffi::OsStr {
    p.as_os_str()
}

/// Main executable of an app bundle (CFBundleExecutable).
fn main_executable(app: &Path) -> Option<PathBuf> {
    let plist = app.join("Contents/Info.plist");
    let out = Command::new("plutil").args(["-extract", "CFBundleExecutable", "raw", "-o", "-"]).arg(&plist).output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then(|| app.join("Contents/MacOS").join(name))
}

/// Identifies the original app's build: the copy is redone when Battle.net updates the game.
fn stamp(original: &Path) -> Option<String> {
    let meta = std::fs::metadata(main_executable(original)?).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}|{}|{}", original.display(), meta.len(), mtime))
}

/// Copies and re-signs the game app if the copy is missing or older than the original. Returns the copy.
pub fn prepare_copy(settings: &Settings, mut progress: impl FnMut(&str)) -> Result<PathBuf, String> {
    let original = settings.exe();
    let dir = Path::new(&settings.game_dir);
    let copy = dir.join(MAC_COPY_NAME);
    let want = stamp(&original).ok_or_else(|| format!("{} illisible", original.display()))?;
    let stamp_file = config_dir().join("game-copy.stamp");
    if main_executable(&copy).is_some_and(|e| e.exists()) && std::fs::read_to_string(&stamp_file).ok().as_deref() == Some(want.as_str()) {
        return Ok(copy);
    }

    progress(t("mac_copying"));
    let tmp = dir.join(".forever-copy.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    run("ditto", &[os(&original), os(&tmp)])?;
    run("xattr", &["-cr".as_ref(), os(&tmp)])?;

    progress(t("mac_signing"));
    // nested code first: helper apps, then the files next to the executable (icudt*.dat: codesign refuses the
    // app while they are unsigned), then the app itself
    if let Ok(helpers) = std::fs::read_dir(tmp.join("Contents/Helpers")) {
        for h in helpers.flatten() {
            run("codesign", &["--force".as_ref(), "--deep".as_ref(), "-s".as_ref(), "-".as_ref(), os(&h.path())])?;
        }
    }
    let exe_name = main_executable(&tmp).and_then(|e| e.file_name().map(|n| n.to_owned()));
    if let Ok(files) = std::fs::read_dir(tmp.join("Contents/MacOS")) {
        for f in files.flatten() {
            if Some(f.file_name()) != exe_name && f.path().is_file() {
                run("codesign", &["--force".as_ref(), "-s".as_ref(), "-".as_ref(), os(&f.path())])?;
            }
        }
    }
    let ent = config_dir().join("game-entitlements.plist");
    std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(&ent, ENTITLEMENTS).map_err(|e| e.to_string())?;
    run("codesign", &["--force".as_ref(), "-s".as_ref(), "-".as_ref(), "--entitlements".as_ref(), os(&ent), os(&tmp)])?;

    let _ = std::fs::remove_dir_all(&copy);
    std::fs::rename(&tmp, &copy).map_err(|e| format!("{} : {e}", copy.display()))?;
    std::fs::write(&stamp_file, want).map_err(|e| e.to_string())?;
    Ok(copy)
}

/// Writes keypatch.dylib next to the settings (only when it changed) and returns its path.
fn install_dylib() -> Result<PathBuf, String> {
    let path = config_dir().join("libforever_keypatch.dylib");
    if std::fs::read(&path).ok().as_deref() != Some(DYLIB) {
        std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
        std::fs::write(&path, DYLIB).map_err(|e| format!("{} : {e}", path.display()))?;
        run("codesign", &["--force".as_ref(), "-s".as_ref(), "-".as_ref(), os(&path)])?;
    }
    // a quarantined library could be refused by dyld
    let _ = run("xattr", &["-c".as_ref(), os(&path)]);
    Ok(path)
}

// CommonCrypto (libSystem)
extern "C" {
    fn CCKeyDerivationPBKDF(algorithm: u32, password: *const u8, password_len: usize, salt: *const u8, salt_len: usize,
                            prf: u32, rounds: u32, derived_key: *mut u8, derived_key_len: usize) -> i32;
    fn CCCrypt(op: u32, alg: u32, options: u32, key: *const u8, key_len: usize, iv: *const u8, data_in: *const u8,
               data_in_len: usize, data_out: *mut u8, data_out_available: usize, data_out_moved: *mut usize) -> i32;
}

/// WEB_TOKEN as the official client reads it from the net.battle preferences (same scheme as
/// Shauren/tc-client-launcher MacLauncher.cpp; "someSalt" sits next to "net.battle" in the client): AES-128-CBC,
/// zero IV, PKCS#7, key = PBKDF2-HMAC-SHA1(entropy XOR user name, "someSalt", 1000 rounds).
fn encrypt_ticket(ticket: &str, user: &str) -> Result<Vec<u8>, String> {
    let mut password = crate::dpapi::WEB_TOKEN_ENTROPY;
    for (p, u) in password.iter_mut().zip(user.bytes()) {
        *p ^= u;
    }
    let salt = b"someSalt";
    let mut key = [0u8; 16];
    // kCCPBKDF2 = 2, kCCPRFHmacAlgSHA1 = 1
    if unsafe { CCKeyDerivationPBKDF(2, password.as_ptr(), 16, salt.as_ptr(), salt.len(), 1, 1000, key.as_mut_ptr(), 16) } != 0 {
        return Err(t("mac_key").into());
    }
    let data = ticket.as_bytes();
    let mut out = vec![0u8; data.len() + 16];
    let mut moved = 0usize;
    // kCCEncrypt = 0, kCCAlgorithmAES = 0, kCCOptionPKCS7Padding = 1, null IV = zeros
    let status = unsafe {
        CCCrypt(0, 0, 1, key.as_ptr(), 16, std::ptr::null(), data.as_ptr(), data.len(), out.as_mut_ptr(), out.len(), &mut moved)
    };
    if status != 0 {
        return Err(tf("mac_encrypt", &[&status]));
    }
    out.truncate(moved);
    Ok(out)
}

/// Stores the login ticket where the client reads it with -launcherlogin (net.battle preferences).
pub fn write_launch_options(portal: &str, ticket: &str, game_account: &str) -> Result<(), String> {
    let user = std::env::var("USER").map_err(|_| t("mac_user").to_string())?;
    let token = hex::encode(encrypt_ticket(ticket, &user)?);
    for (key, kind, value) in [
        ("Launch Options/WoW/WEB_TOKEN", "-data", token.as_str()),
        ("Launch Options/WoW/GAME_ACCOUNT", "-string", game_account),
        ("Launch Options/WoW/CONNECTION_STRING", "-string", portal),
    ] {
        run("defaults", &["write".as_ref(), "net.battle".as_ref(), key.as_ref(), kind.as_ref(), value.as_ref()])?;
    }
    Ok(())
}

/// Starts the copy with keypatch.dylib. `open -W` lives as long as the game: waiting for it tells when it closes.
pub fn spawn(settings: &Settings, copy: &Path) -> Result<(Child, PathBuf), String> {
    let dylib = install_dylib()?;
    let log = config_dir().join("keypatch.log");
    std::fs::write(&log, b"").map_err(|e| e.to_string())?;
    let server = &settings.server;
    let child = Command::new("open")
        .args(["-n", "-W"])
        .arg("--env").arg(format!("DYLD_INSERT_LIBRARIES={}", dylib.display()))
        .arg("--env").arg(format!("FOREVER_SERVER_KEY={}", server.public_key))
        .arg("--env").arg(format!("FOREVER_REGION_GROUP={}", server.region_group))
        .arg("--env").arg(format!("FOREVER_PATCH_LOG={}", log.display()))
        .arg("-a").arg(copy)
        .args(["--args", "-launcherlogin", "-config", WTF_NAME])
        .spawn()
        .map_err(|e| tf("launch_failed", &[&"World of Warcraft", &e]))?;
    Ok((child, log))
}

/// Follows keypatch.log until the game closes. `emit(kind, message)`: "log", "ready".
pub fn watch(mut child: Child, log: &Path, mut emit: impl FnMut(&'static str, String)) -> Result<(), String> {
    let mut pos = 0u64;
    let mut pending = String::new();
    let mut loaded = false;
    loop {
        let done = child.try_wait().map_err(|e| e.to_string())?;
        if let Ok(mut f) = std::fs::File::open(log) {
            if f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = String::new();
                if let Ok(n) = f.read_to_string(&mut buf) {
                    pos += n as u64;
                    pending.push_str(&buf);
                }
            }
        }
        while let Some(end) = pending.find('\n') {
            let line: String = pending.drain(..=end).collect();
            // "HH:MM:SS [forever] message"
            let msg = line.split_once("[forever] ").map(|(_, m)| m).unwrap_or(&line).trim().to_string();
            loaded = true;
            if msg.starts_with("server key written") || msg.starts_with("server key put back") {
                emit("log", msg);
                emit("ready", t("ready").into());
            } else {
                emit("log", msg);
            }
        }
        if let Some(status) = done {
            if !status.success() {
                return Err(tf("mac_not_started", &[&status]));
            }
            if !loaded {
                return Err(t("mac_module").into());
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::encrypt_ticket;

    // reference: PBKDF2 from Python hashlib + `openssl enc -aes-128-cbc`, the pair validated in game
    #[test]
    fn ticket_matches_reference() {
        assert_eq!(hex::encode(encrypt_ticket("TC-0123456789abcdef", "alice").unwrap()),
                   "c648fac45e970e5481a51d09a47d3edf8d3fc929ff471e6fa7d8e6d900163ea6");
        // only the first 16 bytes of the user name count
        assert_eq!(hex::encode(encrypt_ticket("TC-0123456789abcdef", "a-very-long-user-name-xyz").unwrap()),
                   "83d39aad9ede31437dc7fe9083fdf5f88d1da8a4c1f527c9d7d8ef665964c200");
    }
}
