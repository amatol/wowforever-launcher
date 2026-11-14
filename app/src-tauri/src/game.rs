//! Подготовка и запуск игры: WTF-конфиг, Launch Options\WoW (автовход), Wow.exe -launcherlogin.
//! macOS starts a copy of the game app instead: see macos.rs.

use crate::config::Settings;
use crate::i18n::tf;
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::process::Command;
#[cfg(windows)]
use std::process::Child;

#[cfg(not(target_os = "macos"))]
const LAUNCH_OPTIONS: &str = r"Software\Blizzard Entertainment\Battle.net\Launch Options\WoW";

/// Writes the portal into the player's normal WTF/Config.wtf. The old
/// ForeverLauncher.wtf is removed: the client now starts without `-config`
/// and uses its standard Config.wtf, exactly like the Battle.net client.
/// Only `SET portal` is touched; all other CVars are left alone.
pub fn write_wtf(settings: &Settings) -> Result<(), String> {
    let dir = Path::new(&settings.game_dir).join("WTF");
    std::fs::create_dir_all(&dir).map_err(|e| tf("wtf_folder", &[&e]))?;
    let config = dir.join("Config.wtf");
    let _ = std::fs::remove_file(dir.join("ForeverLauncher.wtf"));
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    rewrite_values(&config, &text, &[("portal", settings.server.portal.clone())])?;
    // Read back: the client must see exactly our portal (a stale/foreign Config.wtf would send it to
    // the old or the official server).
    let written = std::fs::read_to_string(&config).map_err(|e| format!("{} : {e}", config.display()))?;
    let expected = format!("SET portal \"{}\"", settings.server.portal);
    if !written.lines().any(|line| line.trim_end() == expected) {
        return Err(tf("wtf_portal", &[&config.display().to_string()]));
    }
    Ok(())
}

fn rewrite_values(path: &Path, text: &str, wanted: &[(&str, String)]) -> Result<(), String> {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !wanted.iter().any(|(key, _)| line.starts_with(&format!("SET {key} "))))
        .map(str::to_string)
        .collect();
    for (key, value) in wanted {
        lines.push(format!("SET {key} \"{value}\""));
    }
    std::fs::write(path, lines.join("\r\n") + "\r\n")
        .map_err(|error| format!("{} : {error}", path.display()))
}

/// One-time cleanup of the client ADB cache after the 0.1.34 hotfix import.
/// The DB2/hotfix cache can contain records without localized strings and
/// crash the client on entering the world; the client rebuilds it itself.
pub fn clear_cache_once(settings: &Settings) -> Result<(), String> {
    let marker = crate::config::config_dir().join("cache-cleared-20261110-hotfixes");
    if marker.exists() {
        return Ok(());
    }
    let cache = Path::new(&settings.game_dir).join("Cache");
    if cache.exists() {
        std::fs::remove_dir_all(&cache)
            .map_err(|error| format!("{} : {error}", cache.display()))?;
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&marker, b"1").map_err(|error| error.to_string())
}

/// Stores the login ticket where the client reads it with -launcherlogin.
#[cfg(windows)]
pub fn write_launch_options(_settings: &Settings, portal: &str, ticket: &str, game_account: &str) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::*;
    let blob = crate::dpapi::protect(ticket.as_bytes())?;
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(LAUNCH_OPTIONS), 0, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None)
            .ok()
            .map_err(|e| tf("registry", &[&e]))?;
        let set_str = |name: &str, value: &str| {
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            RegSetValueExW(key, &HSTRING::from(name), 0, REG_SZ, Some(bytes)).ok()
        };
        let r = set_str("CONNECTION_STRING", portal)
            .and_then(|_| set_str("GAME_ACCOUNT", game_account))
            .and_then(|_| RegSetValueExW(key, &HSTRING::from("WEB_TOKEN"), 0, REG_BINARY, Some(&blob)).ok());
        let _ = RegCloseKey(key);
        r.map_err(|e| tf("registry", &[&e]))
    }
}

/// Linux: the same values in the prefix registry, through `reg import` (the Wine DPAPI blob is built here).
#[cfg(target_os = "linux")]
pub fn write_launch_options(settings: &Settings, portal: &str, ticket: &str, game_account: &str) -> Result<(), String> {
    use crate::dpapi::{wine, WEB_TOKEN_ENTROPY};
    let blob = wine::protect(ticket.as_bytes(), wine::PROTON_USER, &WEB_TOKEN_ENTROPY);
    let hexdata: Vec<String> = blob.iter().map(|b| format!("{b:02x}")).collect();
    let content = format!(
        "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\{LAUNCH_OPTIONS}]\r\n\"WEB_TOKEN\"=hex:{}\r\n\"GAME_ACCOUNT\"=\"{game_account}\"\r\n\"CONNECTION_STRING\"=\"{portal}\"\r\n",
        hexdata.join(",")
    );
    // regedit files are UTF-16 LE with a BOM; Wine sees the Linux file system as Z:
    let mut bytes = vec![0xFF, 0xFE];
    for u in content.encode_utf16() {
        bytes.extend(u.to_le_bytes());
    }
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("launch-options.reg");
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    let wine_path = format!("Z:{}", file.to_string_lossy().replace('/', "\\"));
    let res = crate::system::run_in_prefix(settings, &["reg", "import", &wine_path]);
    let _ = std::fs::remove_file(&file);
    res?;
    // Read back: an old CONNECTION_STRING (trinity.actual.battle.net) would send the client to the
    // old portal or to the official server.
    let out = crate::system::run_in_prefix_output(settings, &["reg", "query", &format!(r"HKCU\{LAUNCH_OPTIONS}"), "/v", "CONNECTION_STRING"])?;
    if !out.contains(portal) {
        return Err(tf("launch_options_write", &[&portal]));
    }
    Ok(())
}

#[cfg(windows)]
pub fn spawn(settings: &Settings) -> Result<Child, String> {
    Command::new(settings.exe())
        .args(["-launcherlogin"])
        .current_dir(&settings.game_dir)
        .spawn()
        .map_err(|e| tf("launch_failed", &[&"Wow.exe", &e]))
}

#[cfg(target_os = "linux")]
pub fn spawn(settings: &Settings) -> Result<crate::steamos::Launch, String> {
    // Validate before starting either the helper or the game.
    crate::patcher::Patcher::new(&settings.server.public_key, settings.server.region_group)?;
    let (helper, log, runner_log, netdelay, ready) = crate::steamos::prepare()?;
    let (runner, mut env) = crate::system::runner(settings)?;
    let mut cmd: Vec<String> = runner;
    cmd.push(crate::steamos::windows_path(&helper)?);
    cmd.push(crate::steamos::windows_path(&log)?);
    cmd.push(crate::steamos::windows_path(&settings.exe())?);
    cmd.push(settings.server.public_key.clone());
    cmd.push(settings.server.region_group.to_string());
    let mut preload = netdelay.to_string_lossy().into_owned();
    if let Ok(existing) = std::env::var("LD_PRELOAD") {
        if !existing.trim().is_empty() {
            preload = format!("{preload}:{existing}");
        }
    }
    env.push(("LD_PRELOAD".into(), preload));
    env.push(("FOREVER_READY_FILE".into(), ready.to_string_lossy().into_owned()));
    env.push(("FOREVER_READY_WIN".into(), crate::steamos::windows_path_allow_missing(&ready)?));
    env.push(("FOREVER_WORLD_PORT".into(), settings.server.world_port.to_string()));
    env.push(("FOREVER_NET_TIMEOUT".into(), "15000".into()));
    let l = &settings.linux;
    if l.gamescope && crate::system::which("gamescope") {
        // gamescope starts its own Xwayland with a US keyboard unless told otherwise
        let (layout, variant) = l.keyboard.split_once('+').unwrap_or((l.keyboard.as_str(), ""));
        if !layout.is_empty() {
            env.push(("XKB_DEFAULT_LAYOUT".into(), layout.into()));
        }
        if !variant.is_empty() {
            env.push(("XKB_DEFAULT_VARIANT".into(), variant.into()));
        }
        let mut wrapped = vec!["gamescope".to_string()];
        wrapped.extend(l.gamescope_args.split_whitespace().map(String::from));
        wrapped.push("--".into());
        wrapped.extend(cmd);
        cmd = wrapped;
    }
    let output = std::fs::OpenOptions::new().append(true).open(&runner_log).map_err(|e| e.to_string())?;
    let errors = output.try_clone().map_err(|e| e.to_string())?;
    let child = crate::system::clean_env(&mut Command::new(&cmd[0]))
        .args(&cmd[1..])
        .envs(env)
        .stdout(output)
        .stderr(errors)
        .current_dir(&settings.game_dir)
        .spawn()
        .map_err(|e| tf("launch_failed", &[&cmd[0], &e]))?;
    Ok(crate::steamos::Launch { child, log, runner_log })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_portal_into_config_wtf() {
        let dir = std::env::temp_dir().join(format!("forever-wtf-test-{}", std::process::id()));
        let game = dir.join("_forever_");
        std::fs::create_dir_all(game.join("WTF")).unwrap();
        std::fs::write(game.join("WTF/Config.wtf"), "SET portal \"trinity.actual.battle.net\"\r\nSET gxApi \"D3D11\"\r\n").unwrap();
        let mut settings = Settings::default();
        settings.game_dir = game.to_string_lossy().into_owned();
        settings.server.portal = "trinity.actual.mmolabs.ru".into();
        write_wtf(&settings).unwrap();
        let text = std::fs::read_to_string(game.join("WTF/Config.wtf")).unwrap();
        assert!(text.contains("SET portal \"trinity.actual.mmolabs.ru\""));
        assert!(!text.contains("trinity.actual.battle.net"));
        assert!(text.contains("SET gxApi \"D3D11\""));
        let _ = std::fs::remove_dir_all(dir);
    }
}
