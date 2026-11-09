//! Обновление клиента и аддонов с сервера Forever AI.

use crate::config::Settings;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AddonFile {
    pub path: String,
    pub download_url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Addon {
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub folders: Vec<String>,
    pub files: Vec<AddonFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AddonsManifest {
    pub addons: Vec<Addon>,
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddonsStatus {
    pub addons: usize,
    pub total_files: usize,
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

fn addons_root(settings: &Settings) -> Result<PathBuf, String> {
    let root = client_root(settings)?;
    Ok(root.join("_forever_").join("Interface").join("AddOns"))
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

async fn fetch_addons_manifest(settings: &Settings) -> Result<AddonsManifest, String> {
    let url = format!("{}/addons_manifest.json", base_url(settings)?);
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
        if file_size(&path) != Some(file.size) {
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
    let manifest = fetch_client_manifest(settings).await?;
    let root = client_root(settings)?;
    let local = local_version(&root);
    let local_build = crate::config::installed_version(&root.join("_forever_")).unwrap_or_default();
    let build_differs = !manifest.build.is_empty() && local_build != manifest.build;
    let mut entries = Vec::new();
    for file in &manifest.files {
        let relative = safe_relative(&file.path)?;
        let path = root.join(&relative);
        if file_size(&path) != Some(file.size) {
            entries.push(DownloadEntry {
                path: file.path.clone(),
                url: file.http_url.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
            });
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

pub async fn addons_status(settings: &Settings) -> Result<AddonsStatus, String> {
    let manifest = fetch_addons_manifest(settings).await?;
    let root = addons_root(settings)?;
    let mut total_files = 0usize;
    let mut changed_files = 0usize;
    let mut changed_bytes = 0u64;
    for addon in &manifest.addons {
        for file in &addon.files {
            total_files += 1;
            let path = root.join(safe_relative(&file.path)?);
            if file_size(&path) != Some(file.size) {
                changed_files += 1;
                changed_bytes = changed_bytes.saturating_add(file.size);
            }
        }
    }
    Ok(AddonsStatus {
        addons: manifest.addons.len(),
        total_files,
        changed_files,
        changed_bytes,
        up_to_date: changed_files == 0,
    })
}

pub async fn update_addons(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let manifest = fetch_addons_manifest(settings).await?;
    let root = addons_root(settings)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("Не удалось создать папку аддонов: {error}"))?;
    let mut entries = Vec::new();
    for addon in &manifest.addons {
        for file in &addon.files {
            let relative = safe_relative(&file.path)?;
            if file_size(&root.join(&relative)) != Some(file.size) {
                entries.push(DownloadEntry {
                    path: file.path.clone(),
                    url: file.download_url.clone(),
                    size: file.size,
                    sha256: file.sha256.clone(),
                });
            }
        }
    }
    if entries.is_empty() {
        emit(app, "addons", done_event(0, 0, 0, 0, "Аддоны уже обновлены"));
        return Ok(());
    }
    download_entries(app, "addons", &root, entries, 6).await?;
    std::fs::write(root.join(".launcher_addons"), format!("{}\n", manifest.addons.len()))
        .map_err(|error| format!("Не удалось записать состояние аддонов: {error}"))?;
    emit(app, "addons", done_event(0, 0, 0, 0, "Аддоны обновлены"));
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
