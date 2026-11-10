//! Самообновление лаунчера через `launcher_manifest.json`.
//!
//! Манифест публикуется на нашем сайте рядом с EXE. Лаунчер сравнивает версии,
//! скачивает новый EXE, проверяет размер и SHA-256 и запускает небольшой
//! Windows-скрипт, который ждёт выхода текущего процесса, заменяет файл и
//! перезапускает лаунчер. Ключ подписи не нужен: целостность проверяется
//! SHA-256 из HTTPS-манифеста.

use crate::config::Settings;
#[cfg(windows)]
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::path::Path;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;
use tauri::AppHandle;
#[cfg(windows)]
use tauri::Emitter;

#[derive(Clone, Debug, Deserialize)]
pub struct LauncherManifest {
    pub version: String,
    pub download_url: String,
    pub size: u64,
    #[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg(windows)]
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
    Ok(format!("{base}/launcher_manifest.json"))
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
        supported: cfg!(windows),
    })
}

#[cfg(windows)]
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

#[cfg(windows)]
pub async fn install(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let manifest = fetch_manifest(settings).await?;
    if !is_newer(&manifest.version, current_version()) {
        emit(app, "done", 100.0, "Лаунчер уже обновлён");
        return Ok(());
    }
    if manifest.size == 0 || manifest.sha256.len() != 64 {
        return Err("В манифесте лаунчера нет корректного размера или SHA-256".into());
    }
    if !manifest.download_url.starts_with("https://") {
        return Err("Манифест лаунчера должен ссылаться на HTTPS".into());
    }

    let current = std::env::current_exe().map_err(|error| format!("Не удалось найти EXE лаунчера: {error}"))?;
    let parent = current
        .parent()
        .ok_or_else(|| "У EXE лаунчера нет родительской папки".to_string())?;

    // Проверяем, что рядом с лаунчером можно писать: иначе helper не сможет
    // заменить EXE после выхода.
    let probe = parent.join(format!(".forever-launcher-write-{}", std::process::id()));
    std::fs::write(&probe, b"forever")
        .map_err(|error| format!("Нет прав на запись в папку лаунчера {}: {error}", parent.display()))?;
    let _ = std::fs::remove_file(&probe);

    let temporary = std::env::temp_dir().join(format!(
        "forever-ai-launcher-update-{}.exe",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temporary);

    let client = reqwest::Client::builder()
        .user_agent("ForeverAI-Launcher")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(&manifest.download_url)
        .send()
        .await
        .map_err(|error| format!("Не удалось скачать лаунчер: {error}"))?;
    let http_status = response.status();
    if !http_status.is_success() {
        return Err(format!("Сервер вернул HTTP {http_status} для {}", manifest.download_url));
    }

    emit(
        app,
        "start",
        0.0,
        format!("Скачивание лаунчера {}", manifest.version),
    );
    let mut file = tokio::fs::File::create(&temporary)
        .await
        .map_err(|error| format!("Не удалось создать временный файл: {error}"))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut last_emit = Instant::now();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(format!("Ошибка загрузки лаунчера: {error}"));
            }
        };
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("Не удалось записать лаунчер: {error}"))?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
        if last_emit.elapsed() >= Duration::from_millis(200) {
            last_emit = Instant::now();
            let percent = if manifest.size > 0 {
                (downloaded as f64 / manifest.size as f64 * 100.0).min(99.9)
            } else {
                0.0
            };
            emit(app, "progress", percent, String::new());
        }
    }
    file.flush()
        .await
        .map_err(|error| format!("Не удалось сохранить лаунчер: {error}"))?;
    drop(file);

    if downloaded != manifest.size {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(format!(
            "Лаунчер скачан не полностью: ожидалось {} байт, получено {}",
            manifest.size, downloaded
        ));
    }
    let digest = hex::encode(hasher.finalize());
    if !digest.eq_ignore_ascii_case(&manifest.sha256) {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err("SHA-256 скачанного лаунчера не совпала с манифестом".into());
    }

    emit(app, "done", 100.0, "Обновление загружено, лаунчер перезапустится");
    spawn_replacer(&current, &temporary)?;
    app.exit(0);
    Ok(())
}

#[cfg(not(windows))]
pub async fn install(_app: &AppHandle, _settings: &Settings) -> Result<(), String> {
    Err("Автообновление лаунчера доступно в Windows-версии".into())
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
