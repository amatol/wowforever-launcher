//! Обновление клиента с сервера Дримворлд Навсегда.

use crate::config::Settings;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub http_url: String,
    /// Сверять SHA-256 локального файла, а не только размер.
    #[serde(default)]
    pub verify_sha256: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientManifest {
    pub version: String,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub product: String,
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub removed_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEvent {
    pub kind: String,
    pub file: String,
    pub current: usize,
    pub total: usize,
    pub downloaded: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientStatus {
    pub version: String,
    pub build: String,
    pub local_version: String,
    pub local_build: String,
    pub total_files: usize,
    pub total_bytes: u64,
    pub changed_files: usize,
    pub changed_bytes: u64,
    pub up_to_date: bool,
}

#[derive(Clone)]
struct DownloadEntry {
    path: String,
    url: String,
    size: u64,
    sha256: String,
}

fn base_url(settings: &Settings) -> Result<String, String> {
    let base = settings.server.website.trim_end_matches('/');
    if base.is_empty() {
        return Err("В настройках не указан сайт сервера".into());
    }
    Ok(base.to_string())
}

fn client_root(settings: &Settings) -> Result<PathBuf, String> {
    let raw = Path::new(&settings.game_dir);
    if raw.as_os_str().is_empty() {
        return Err("Папка клиента не выбрана".into());
    }
    let mut candidates = vec![raw.to_path_buf()];
    if let Some(parent) = raw.parent() {
        candidates.push(parent.to_path_buf());
    }
    for candidate in candidates {
        if candidate.join("_forever_").join("Wow.exe").is_file() {
            return Ok(candidate);
        }
    }
    Err("Не найдена папка _forever_ с Wow.exe".into())
}

fn safe_relative(path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(format!("Недопустимый путь: {path}"));
    }
    if relative
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err(format!("Недопустимый путь: {path}"));
    }
    Ok(relative.to_path_buf())
}

async fn get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|error| format!("Не удалось открыть {url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Сервер вернул HTTP {status} для {url}"));
    }
    response
        .json::<T>()
        .await
        .map_err(|error| format!("Некорректный ответ {url}: {error}"))
}

async fn fetch_client_manifest(settings: &Settings) -> Result<ClientManifest, String> {
    let url = format!("{}/manifest.json", base_url(settings)?);
    get_json(&url).await
}

fn local_version(root: &Path) -> String {
    std::fs::read_to_string(root.join(".launcher_version"))
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

fn file_matches(path: &Path, size: u64, expected_sha: &str, verify_sha256: bool) -> bool {
    if file_size(path) != Some(size) {
        return false;
    }
    if !verify_sha256 {
        return true;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return false,
        }
    }
    hex::encode(hasher.finalize()) == expected_sha
}

pub async fn client_status(settings: &Settings) -> Result<ClientStatus, String> {
    let manifest = fetch_client_manifest(settings).await?;
    let root = client_root(settings)?;
    let local = local_version(&root);
    let local_build = crate::config::installed_version(&root.join("_forever_")).unwrap_or_default();
    let build_differs = !manifest.build.is_empty() && local_build != manifest.build;
    let mut changed_files = 0usize;
    let mut changed_bytes = 0u64;
    for file in &manifest.files {
        let path = root.join(safe_relative(&file.path)?);
        if !file_matches(&path, file.size, &file.sha256, file.verify_sha256) {
            changed_files += 1;
            changed_bytes = changed_bytes.saturating_add(file.size);
        }
    }
    for removed in &manifest.removed_files {
        if root.join(safe_relative(removed)?).exists() {
            changed_files += 1;
        }
    }
    if build_differs {
        changed_files += 1;
        changed_bytes = changed_bytes.saturating_add(
            manifest.files.iter().find(|file| file.path == ".build.info").map(|file| file.size).unwrap_or(0),
        );
    }
    // Отсутствие .launcher_version у готового клиента не делает его устаревшим:
    // сравниваем реальные размеры файлов и build из .build.info.
    let up_to_date = changed_files == 0;
    Ok(ClientStatus {
        version: manifest.version,
        build: manifest.build,
        local_version: local,
        local_build,
        total_files: manifest.files.len(),
        total_bytes: manifest.files.iter().map(|file| file.size).sum(),
        changed_files,
        changed_bytes,
        up_to_date,
    })
}

pub async fn update_client(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    update_client_inner(app, settings, false).await
}

/// Полная проверка: SHA-256 каждого файла и восстановление несовпавших.
pub async fn verify_client(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    update_client_inner(app, settings, true).await
}

async fn update_client_inner(
    app: &AppHandle,
    settings: &Settings,
    full: bool,
) -> Result<(), String> {
    let manifest = fetch_client_manifest(settings).await?;
    let root = client_root(settings)?;
    let local = local_version(&root);
    let local_build = crate::config::installed_version(&root.join("_forever_")).unwrap_or_default();
    let build_differs = !manifest.build.is_empty() && local_build != manifest.build;
    let total_files = manifest.files.len();
    let total_bytes: u64 = manifest.files.iter().map(|file| file.size).sum();
    let mut entries = Vec::new();
    let mut checked_bytes = 0u64;
    let mut last_emit = Instant::now();
    if full {
        emit(
            app,
            "client",
            UpdateEvent {
                kind: "verify".into(),
                file: String::new(),
                current: 0,
                total: total_files,
                downloaded: 0,
                total_bytes,
                percent: 0.0,
                message: String::new(),
            },
        );
    }
    for (index, file) in manifest.files.iter().enumerate() {
        let relative = safe_relative(&file.path)?;
        let path = root.join(&relative);
        let matches = if full {
            let path = path.clone();
            let size = file.size;
            let sha = file.sha256.clone();
            tokio::task::spawn_blocking(move || file_matches(&path, size, &sha, true))
                .await
                .unwrap_or(false)
        } else {
            file_matches(&path, file.size, &file.sha256, file.verify_sha256)
        };
        if !matches {
            entries.push(DownloadEntry {
                path: file.path.clone(),
                url: file.http_url.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
            });
        }
        if full {
            checked_bytes = checked_bytes.saturating_add(file.size);
            if last_emit.elapsed() >= Duration::from_millis(200) || index + 1 == total_files {
                last_emit = Instant::now();
                let percent = if total_bytes > 0 {
                    (checked_bytes as f64 / total_bytes as f64 * 100.0).min(99.9)
                } else {
                    0.0
                };
                emit(
                    app,
                    "client",
                    UpdateEvent {
                        kind: "verify".into(),
                        file: file.path.clone(),
                        current: index + 1,
                        total: total_files,
                        downloaded: checked_bytes,
                        total_bytes,
                        percent,
                        message: String::new(),
                    },
                );
            }
        }
    }
    if build_differs && !entries.iter().any(|entry| entry.path == ".build.info") {
        if let Some(file) = manifest.files.iter().find(|file| file.path == ".build.info") {
            entries.push(DownloadEntry {
                path: file.path.clone(),
                url: file.http_url.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
            });
        }
    }
    let has_stale = manifest
        .removed_files
        .iter()
        .any(|path| safe_relative(path).map(|value| root.join(value).exists()).unwrap_or(false));

    if entries.is_empty() && local == manifest.version && !has_stale {
        emit(app, "client", done_event(0, 0, 0, 0, "Клиент уже обновлён"));
        return Ok(());
    }

    download_entries(app, "client", &root, entries, 4).await?;

    for removed in &manifest.removed_files {
        let path = root.join(safe_relative(removed)?);
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
    std::fs::write(root.join(".launcher_version"), format!("{}\n", manifest.version))
        .map_err(|error| format!("Не удалось записать версию клиента: {error}"))?;
    emit(app, "client", done_event(0, 0, 0, 0, "Клиент обновлён"));
    Ok(())
}

/// Корень клиента для установки: если выбрали саму папку `_forever_`,
/// устанавливаем в её родителя (там лежат Data и .build.info).
pub fn normalize_install_root(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("Папка установки не выбрана".into());
    }
    let mut root = path.to_path_buf();
    let is_forever = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case("_forever_"))
        .unwrap_or(false);
    if is_forever {
        root = root
            .parent()
            .ok_or_else(|| "Не удалось определить папку установки".to_string())?
            .to_path_buf();
    }
    Ok(root)
}

/// Полная установка/докачка клиента в выбранный корень.
/// Скачивает все отсутствующие или не совпавшие по SHA-256 файлы.
pub async fn install_client(
    app: &AppHandle,
    settings: &Settings,
    root: &Path,
) -> Result<(), String> {
    let manifest = fetch_client_manifest(settings).await?;
    std::fs::create_dir_all(root)
        .map_err(|error| format!("Не удалось создать папку {}: {error}", root.display()))?;
    let mut entries = Vec::new();
    for file in &manifest.files {
        let relative = safe_relative(&file.path)?;
        let path = root.join(&relative);
        if !file_matches(&path, file.size, &file.sha256, file.verify_sha256) {
            entries.push(DownloadEntry {
                path: file.path.clone(),
                url: file.http_url.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
            });
        }
    }
    if entries.is_empty() {
        emit(app, "client", done_event(0, 0, 0, 0, "Клиент уже установлен"));
        return Ok(());
    }
    download_entries(app, "client", root, entries, 4).await?;
    std::fs::write(root.join(".launcher_version"), format!("{}\n", manifest.version))
        .map_err(|error| format!("Не удалось записать версию клиента: {error}"))?;
    emit(app, "client", done_event(0, 0, 0, 0, "Клиент установлен"));
    Ok(())
}

fn done_event(
    current: usize,
    total: usize,
    downloaded: u64,
    total_bytes: u64,
    message: &str,
) -> UpdateEvent {
    UpdateEvent {
        kind: "done".into(),
        file: String::new(),
        current,
        total,
        downloaded,
        total_bytes,
        percent: 100.0,
        message: message.into(),
    }
}

fn emit(app: &AppHandle, event: &str, payload: UpdateEvent) {
    let _ = app.emit(event, payload);
}

async fn download_entries(
    app: &AppHandle,
    event: &str,
    root: &Path,
    entries: Vec<DownloadEntry>,
    concurrency: usize,
) -> Result<(), String> {
    let total = entries.len();
    if total == 0 {
        return Ok(());
    }
    let total_bytes: u64 = entries.iter().map(|entry| entry.size).sum();
    let current = Arc::new(AtomicUsize::new(0));
    let downloaded = Arc::new(AtomicU64::new(0));
    emit(
        app,
        event,
        UpdateEvent {
            kind: "start".into(),
            file: String::new(),
            current: 0,
            total,
            downloaded: 0,
            total_bytes,
            percent: 0.0,
            message: format!("Файлов к загрузке: {total}"),
        },
    );

    let client = reqwest::Client::builder()
        .user_agent("ForeverAI-Launcher")
        .build()
        .map_err(|error| error.to_string())?;
    let root = root.to_path_buf();
    let app = app.clone();
    let event_name = event.to_string();

    let tasks = entries.into_iter().map(|entry| {
        let client = client.clone();
        let root = root.clone();
        let app = app.clone();
        let event_name = event_name.clone();
        let current = Arc::clone(&current);
        let downloaded = Arc::clone(&downloaded);
        async move {
            let relative = safe_relative(&entry.path)?;
            let destination = root.join(&relative);
            let parent = destination
                .parent()
                .ok_or_else(|| format!("Нет родительской папки для {}", entry.path))?;
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("Папка {}: {error}", parent.display()))?;
            let file_name = destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file");
            let temporary = destination.with_file_name(format!(".{file_name}.part"));

            let response = client
                .get(&entry.url)
                .send()
                .await
                .map_err(|error| format!("{}: {error}", entry.path))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("{}: HTTP {status}", entry.path));
            }
            let mut stream = response.bytes_stream();
            let mut file = tokio::fs::File::create(&temporary)
                .await
                .map_err(|error| format!("{}: {error}", entry.path))?;
            use tokio::io::AsyncWriteExt;
            let mut hasher = Sha256::new();
            let mut written = 0u64;
            let mut last_emit = Instant::now();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| format!("{}: {error}", entry.path))?;
                file.write_all(&chunk)
                    .await
                    .map_err(|error| format!("{}: {error}", entry.path))?;
                hasher.update(&chunk);
                written += chunk.len() as u64;
                let all = downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed)
                    + chunk.len() as u64;
                if last_emit.elapsed() >= Duration::from_millis(300) {
                    last_emit = Instant::now();
                    let done = current.load(Ordering::Relaxed);
                    emit(
                        &app,
                        &event_name,
                        UpdateEvent {
                            kind: "progress".into(),
                            file: entry.path.clone(),
                            current: done,
                            total,
                            downloaded: all,
                            total_bytes,
                            percent: if total_bytes > 0 {
                                (all as f64 / total_bytes as f64 * 100.0).min(99.9)
                            } else {
                                0.0
                            },
                            message: String::new(),
                        },
                    );
                }
            }
            file.flush()
                .await
                .map_err(|error| format!("{}: {error}", entry.path))?;
            drop(file);
            if written != entry.size {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(format!(
                    "{}: ожидалось {} байт, получено {}",
                    entry.path, entry.size, written
                ));
            }
            let digest = hex::encode(hasher.finalize());
            if digest != entry.sha256 {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(format!("{}: неверная SHA-256", entry.path));
            }
            if destination.exists() {
                tokio::fs::remove_file(&destination)
                    .await
                    .map_err(|error| format!("{}: {error}", entry.path))?;
            }
            tokio::fs::rename(&temporary, &destination)
                .await
                .map_err(|error| format!("{}: {error}", entry.path))?;
            let done = current.fetch_add(1, Ordering::Relaxed) + 1;
            let all = downloaded.load(Ordering::Relaxed);
            emit(
                &app,
                &event_name,
                UpdateEvent {
                    kind: "progress".into(),
                    file: entry.path.clone(),
                    current: done,
                    total,
                    downloaded: all,
                    total_bytes,
                    percent: if total_bytes > 0 {
                        (all as f64 / total_bytes as f64 * 100.0).min(99.9)
                    } else {
                        0.0
                    },
                    message: String::new(),
                },
            );
            Ok::<(), String>(())
        }
    });

    let results: Vec<Result<(), String>> = stream::iter(tasks).buffer_unordered(concurrency).collect().await;
    if let Some(error) = results.into_iter().find_map(|result| result.err()) {
        emit(
            &app,
            event,
            UpdateEvent {
                kind: "error".into(),
                file: String::new(),
                current: current.load(Ordering::Relaxed),
                total,
                downloaded: downloaded.load(Ordering::Relaxed),
                total_bytes,
                percent: 0.0,
                message: error.clone(),
            },
        );
        return Err(error);
    }
    Ok(())
}
