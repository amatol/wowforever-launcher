//! Самообновление лаунчера через `launcher_manifest.json`.
//!
//! Манифест публикуется на нашем сайте рядом с EXE. Лаунчер сравнивает версии,
//! скачивает новый файл, проверяет размер и SHA-256 и заменяет себя:
//! Windows — небольшой скрипт ждёт выхода процесса, заменяет EXE и перезапускает
//! лаунчер; Linux — новый AppImage атомарно переименовывается поверх старого
//! (работающий файл можно заменить) и запускается сразу.
//! Ключ подписи не нужен: целостность проверяется SHA-256 из HTTPS-манифеста.

use crate::config::Settings;
use serde::{Deserialize, Serialize};
#[cfg(any(windows, target_os = "linux"))]
use std::path::Path;
use std::time::Duration;
use tauri::AppHandle;
#[cfg(any(windows, target_os = "linux"))]
use tauri::Emitter;

#[cfg(any(windows, target_os = "linux"))]
#[path = "launcher_download.rs"]
mod download;

/// Linux updates only the AppImage build; it has its own manifest next to the Windows one.
const SUPPORTED: bool = cfg!(windows) || cfg!(target_os = "linux");

#[derive(Clone, Debug, Deserialize)]
pub struct LauncherManifest {
    pub version: String,
    pub download_url: String,
    pub size: u64,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub sha256: String,
    #[serde(default)]
    pub changelog: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherUpdateStatus {
    pub current_version: String,
    pub version: String,
    pub available: bool,
    pub size: u64,
    pub changelog: String,
    pub download_url: String,
    pub supported: bool,
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherUpdateEvent {
    pub kind: String,
    pub percent: f64,
    pub message: String,
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Числовые компоненты версии; хвостовые нули не считаются новой версией.
fn version_parts(value: &str) -> Vec<u64> {
    let mut parts: Vec<u64> = value
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect();
    while parts.len() > 1 && parts.last() == Some(&0) {
        parts.pop();
    }
    parts
}

fn is_newer(remote: &str, local: &str) -> bool {
    version_parts(remote) > version_parts(local)
}

fn manifest_url(settings: &Settings) -> Result<String, String> {
    let base = settings.server.website.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("В настройках не указан сайт сервера".into());
    }
    let name = if cfg!(target_os = "linux") { "launcher_manifest_linux.json" } else { "launcher_manifest.json" };
    Ok(format!("{base}/{name}"))
}

async fn fetch_manifest(settings: &Settings) -> Result<LauncherManifest, String> {
    let url = manifest_url(settings)?;
    let response = reqwest::Client::builder()
        .user_agent("ForeverAI-Launcher")
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("Не удалось проверить обновление лаунчера: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Сервер вернул HTTP {status} для {url}"));
    }
    response
        .json::<LauncherManifest>()
        .await
        .map_err(|error| format!("Некорректный манифест лаунчера: {error}"))
}

pub async fn status(settings: &Settings) -> Result<LauncherUpdateStatus, String> {
    let manifest = fetch_manifest(settings).await?;
    Ok(LauncherUpdateStatus {
        current_version: current_version().into(),
        available: is_newer(&manifest.version, current_version()),
        version: manifest.version,
        size: manifest.size,
        changelog: manifest.changelog,
        download_url: manifest.download_url,
        supported: supported(),
    })
}

/// Linux: only a launcher started as an AppImage can replace itself.
fn supported() -> bool {
    #[cfg(target_os = "linux")]
    if crate::config::appimage_path().is_none() {
        return false;
    }
    SUPPORTED
}

#[cfg(any(windows, target_os = "linux"))]
fn emit(app: &AppHandle, kind: &str, percent: f64, message: impl Into<String>) {
    let _ = app.emit(
        "launcher-update",
        LauncherUpdateEvent {
            kind: kind.into(),
            percent,
            message: message.into(),
        },
    );
}

#[cfg(any(windows, target_os = "linux"))]
pub async fn install(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    // В один .part не должны одновременно писать два вызова обновления.
    static UPDATING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if UPDATING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("Обновление лаунчера уже выполняется".into());
    }
    struct Unlock;
    impl Drop for Unlock {
        fn drop(&mut self) { UPDATING.store(false, std::sync::atomic::Ordering::SeqCst); }
    }
    let _unlock = Unlock;

    let mut retries = 0;
    let manifest = loop {
        match fetch_manifest(settings).await {
            Ok(manifest) => break manifest,
            Err(error) if retries < download::POLICY.retries => {
                retries += 1;
                emit(app, "retry", 0.0, format!("{error} · повтор получения манифеста {}/{} через 5 с", retries, download::POLICY.retries));
                tokio::time::sleep(download::POLICY.delay).await;
            }
            Err(error) => return Err(error),
        }
    };
    if !is_newer(&manifest.version, current_version()) {
        emit(app, "done", 100.0, "Лаунчер уже обновлён");
        return Ok(());
    }
    if manifest.size == 0 || manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("В манифесте лаунчера нет корректного размера или SHA-256".into());
    }
    if !manifest.download_url.starts_with("https://") {
        return Err("Манифест лаунчера должен ссылаться на HTTPS".into());
    }

    if !supported() {
        return Err("Автообновление доступно только для AppImage-версии лаунчера".into());
    }
    let current = crate::config::launcher_path().ok_or_else(|| "Не удалось найти файл лаунчера".to_string())?;
    let parent = current
        .parent()
        .ok_or_else(|| "У EXE лаунчера нет родительской папки".to_string())?;

    // Проверяем, что рядом с лаунчером можно писать: иначе helper не сможет
    // заменить EXE после выхода.
    let probe = parent.join(format!(".forever-launcher-write-{}", std::process::id()));
    std::fs::write(&probe, b"forever")
        .map_err(|error| format!("Нет прав на запись в папку лаунчера {}: {error}", parent.display()))?;
    let _ = std::fs::remove_file(&probe);

    let temporary = download::partial_path(&current, &manifest.sha256);

    let client = reqwest::Client::builder()
        .user_agent("ForeverAI-Launcher")
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    emit(
        app,
        "start",
        0.0,
        format!("Скачивание лаунчера {}", manifest.version),
    );
    download::download(&client, &manifest, &temporary, download::POLICY,
        |kind, percent, message| emit(app, kind, percent, message)).await?;
    // Даже при ошибке установки проверенная загрузка остаётся для следующего запуска.
    spawn_replacer(&current, &temporary)?;
    emit(app, "done", 100.0, "Обновление загружено, лаунчер перезапустится");
    app.exit(0);
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn install(_app: &AppHandle, _settings: &Settings) -> Result<(), String> {
    Err("Автообновление лаунчера доступно в Windows- и Linux-версии".into())
}

/// Linux: the running AppImage is replaced in place (its mount keeps the old inode), then the new one is started.
#[cfg(target_os = "linux")]
fn spawn_replacer(current: &Path, new: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("Не удалось сделать AppImage исполняемым: {error}"))?;
    std::fs::rename(new, current).map_err(|error| format!("Не удалось заменить AppImage: {error}"))?;
    let args: Vec<String> = std::env::args().skip(1).filter(|arg| arg == "--play").collect();
    crate::system::clean_env(&mut std::process::Command::new(current))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Обновление установлено, но перезапуск не удался: {error}"))?;
    Ok(())
}

#[cfg(windows)]
fn spawn_replacer(current: &Path, new: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let script = std::env::temp_dir().join(format!(
        "forever-launcher-update-{}.cmd",
        std::process::id()
    ));
    let body = r#"@echo off
setlocal
:wait
tasklist /FI "PID eq %FOREVER_PID%" 2>nul | findstr /C:"%FOREVER_PID%" >nul
if not errorlevel 1 (
  ping -n 2 127.0.0.1 >nul
  goto wait
)
if exist "%FOREVER_OLD%.old" del /F /Q "%FOREVER_OLD%.old" >nul 2>&1
move /Y "%FOREVER_OLD%" "%FOREVER_OLD%.old" >nul
if errorlevel 1 goto fail
move /Y "%FOREVER_NEW%" "%FOREVER_OLD%" >nul
if errorlevel 1 goto fail
if "%FOREVER_RESTART_PLAY%"=="1" (
  start "" "%FOREVER_OLD%" --play
) else (
  start "" "%FOREVER_OLD%"
)
del "%~f0" >nul
exit /b 0
:fail
echo %date% %time% update failed> "%TEMP%\forever-launcher-update-error.txt"
exit /b 1
"#;
    std::fs::write(&script, body.replace('\n', "\r\n"))
        .map_err(|error| format!("Не удалось создать helper обновления: {error}"))?;

    let restart_play = std::env::args().any(|arg| arg == "--play");
    std::process::Command::new("cmd")
        .arg("/C")
        .arg(&script)
        .env("FOREVER_PID", std::process::id().to_string())
        .env("FOREVER_OLD", current)
        .env("FOREVER_NEW", new)
        .env("FOREVER_RESTART_PLAY", if restart_play { "1" } else { "0" })
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Не удалось запустить helper обновления: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_numerically() {
        assert!(is_newer("1.1.2", "1.1.1"));
        assert!(is_newer("1.1.10", "1.1.2"));
        assert!(!is_newer("1.1.1", "1.1.1"));
        assert!(!is_newer("1.0.9", "1.1.0"));
        assert!(!is_newer("1.1.1.0", "1.1.1"));
        assert!(is_newer("v2.0.0", "1.9.9"));
    }
}
