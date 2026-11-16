//! Обновление клиента с сервера Дримворлд Навсегда.

use crate::config::Settings;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const DOWNLOAD_CONCURRENCY: usize = 8;
/// A stalled source must fail and be retried instead of hanging the whole update.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// Автоматические повторы одного файла: пауза и максимум попыток.
const RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_FILE_RETRIES: u32 = 50;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub http_url: String,
    /// Сверять SHA-256 локального файла, а не только размер.
    #[serde(default)]
    pub verify_sha256: bool,
    /// Проверять файл при быстром обновлении. false — только наличие:
    /// клиент сам переписывает индексы CASC, их размер и SHA нестабильны.
    #[serde(default = "default_true")]
    pub quick_check: bool,
    /// Проверять файл при полной проверке SHA-256.
    #[serde(default = "default_true")]
    pub full_check: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CdnInfo {
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub build_config: String,
    #[serde(default)]
    pub cdn_config: String,
    #[serde(default)]
    pub cdn_path: String,
    #[serde(default)]
    pub cdn_hosts: Vec<String>,
}

/// Small server manifest: the build the realm currently runs.
/// `manifest.json` remains the file list and the fallback source of truth.
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct YandexInfo {
    #[serde(default)]
    pub enabled: bool,
    /// Полный перевод клиента включается отдельно после проверки всех объектов.
    #[serde(default)]
    pub all_files: bool,
    #[serde(default)]
    pub public_url: String,
    /// Folder of the release inside the public share, e.g. /1.60.1.70938.
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct BuildManifest {
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub build_id: u64,
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub cdn: Option<CdnInfo>,
    #[serde(default)]
    pub yandex: Option<YandexInfo>,
    #[serde(default)]
    pub overlay: Vec<String>,
    /// Полная переустановка клиента при каждом обновлении: скачать весь набор
    /// заново, сохранить только WTF и Interface, старую папку удалить.
    #[serde(default)]
    pub full_reinstall: bool,
    #[serde(default)]
    pub fallback_manifest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientManifest {
    pub version: String,
    /// Ревизия неизменяемого набора CASC: меняется даже при том же build.
    #[serde(default)]
    pub data_revision: String,
    #[serde(default)]
    pub reuse_manifest: Option<ReuseManifestRef>,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub product: String,
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub removed_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReuseManifestRef {
    pub url: String,
    pub size: u64,
    pub sha256: String,
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
    /// Blizzard CDN URLs tried before `url`. Overlay files never get one.
    cdn_urls: Vec<String>,
    /// Stable Yandex public-API URL resolved to a temporary href at download time.
    yandex_url: Option<String>,
    /// В режиме Yandex ни один файл клиента не имеет серверного fallback.
    allow_server_fallback: bool,
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

/// The build manifest is optional so old servers without it keep working.
async fn fetch_build_manifest(settings: &Settings) -> Option<BuildManifest> {
    let Ok(base) = base_url(settings) else { return None };
    get_json::<BuildManifest>(&format!("{base}/client_build.json")).await.ok()
}

fn required_build<'a>(build: Option<&'a BuildManifest>, manifest: &'a ClientManifest) -> &'a str {
    build
        .map(|value| value.build.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(manifest.build.as_str())
}

fn is_overlay(path: &str, build: Option<&BuildManifest>) -> bool {
    // Our files must never come from Blizzard. The prefix is a hard guard, not a setting.
    if path.starts_with("_forever_/") || path == ".build.info" {
        return true;
    }
    build
        .map(|value| value.overlay.iter().any(|item| item == path))
        .unwrap_or(false)
}

/// Only CDN objects with a content hash in the path can be fetched directly.
/// Архивы data.* — CASC-контейнеры, а не объекты CDN. В режиме Yandex
/// этот старый CDN-механизм отключён для всего клиента.
fn cdn_urls_for(path: &str, build: Option<&BuildManifest>) -> Vec<String> {
    if is_overlay(path, build) {
        return Vec::new();
    }
    let Some(build) = build else { return Vec::new() };
    let Some(cdn) = build.cdn.as_ref() else { return Vec::new() };
    if cdn.cdn_hosts.is_empty() || cdn.cdn_path.trim_matches('/').is_empty() {
        return Vec::new();
    }
    let name = path.rsplit('/').next().unwrap_or("");
    let (kind, hash) = if path.starts_with("Data/indices/") && name.ends_with(".index") {
        ("data", name.trim_end_matches(".index"))
    } else if path.starts_with("Data/config/") && name.len() == 32 {
        ("config", name)
    } else {
        return Vec::new();
    };
    if hash.len() != 32 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Vec::new();
    }
    let directory = if kind == "data" { "data" } else { "config" };
    let suffix = if kind == "data" { ".index" } else { "" };
    let relative = format!(
        "{}/{}/{}/{}/{hash}{suffix}",
        cdn.cdn_path.trim_matches('/'),
        directory,
        &hash[0..2],
        &hash[2..4]
    );
    cdn.cdn_hosts
        .iter()
        .filter(|host| !host.trim().is_empty())
        .map(|host| {
            let host = host.trim().trim_end_matches('/');
            if host.contains("://") {
                format!("{host}/{relative}")
            } else {
                format!("https://{host}/{relative}")
            }
        })
        .collect()
}

fn entry_for(file: &ManifestFile, build: Option<&BuildManifest>) -> DownloadEntry {
    let yandex_url = yandex_url_for(&file.path, build);
    let overlay = is_overlay(&file.path, build);
    // В режиме Yandex весь клиент берётся только оттуда, включая EXE/DLL.
    // Даже неполная конфигурация не должна незаметно включать VDS/CDN.
    let yandex_enabled = build.and_then(|b| b.yandex.as_ref()).map(|y| y.enabled).unwrap_or(false);
    let all_files = build.and_then(|b| b.yandex.as_ref()).map(|y| y.all_files).unwrap_or(false);
    let allow_server_fallback = !yandex_enabled || (overlay && !all_files);
    let cdn_urls = if overlay || yandex_enabled {
        Vec::new()
    } else {
        cdn_urls_for(&file.path, build)
    };
    DownloadEntry {
        path: file.path.clone(),
        url: file.http_url.clone(),
        cdn_urls,
        yandex_url,
        allow_server_fallback,
        size: file.size,
        sha256: file.sha256.clone(),
    }
}

/// Stable public-API URL. The API returns a short-lived `href`; it is resolved
/// immediately before the actual download, so the manifest stays valid.
fn yandex_url_for(path: &str, build: Option<&BuildManifest>) -> Option<String> {
    let yandex = build?.yandex.as_ref()?;
    if !yandex.all_files && is_overlay(path, build) {
        return None;
    }
    if !yandex.enabled || yandex.public_url.trim().is_empty() || yandex.path.trim().is_empty() {
        return None;
    }
    let root = yandex.path.trim_end_matches('/');
    let full = if root.is_empty() {
        format!("/{}", path.trim_start_matches('/'))
    } else {
        format!("{root}/{}", path.trim_start_matches('/'))
    };
    let mut url = reqwest::Url::parse("https://cloud-api.yandex.net/v1/disk/public/resources/download").ok()?;
    url.query_pairs_mut()
        .append_pair("public_key", &yandex.public_url)
        .append_pair("path", &full);
    Some(url.into())
}

fn local_version(root: &Path) -> String {
    std::fs::read_to_string(root.join(".launcher_version"))
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

const FULL_REINSTALL_MARKER: &str = ".forever_full_reinstall";

fn full_reinstall_is_done(root: &Path, version: &str) -> bool {
    std::fs::read_to_string(root.join(FULL_REINSTALL_MARKER))
        .map(|value| value.trim() == version)
        .unwrap_or(false)
}

fn needs_full_reinstall(root: &Path, manifest: &ClientManifest, build: Option<&BuildManifest>) -> bool {
    let Some(build) = build else { return false };
    if !build.full_reinstall {
        return false;
    }
    let local = local_version(root);
    let local_build = crate::config::installed_version(&root.join("_forever_")).unwrap_or_default();
    let wanted_build = required_build(Some(build), manifest);
    let build_differs = !wanted_build.is_empty() && local_build != wanted_build;
    local != manifest.version
        || build_differs
        || !full_reinstall_is_done(root, &manifest.version)
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

/// Проверяет файл по политике манифеста: для клиентских файлов CASC
/// достаточно наличия, для остальных — размер, а в полном режиме SHA-256.
fn file_is_current(path: &Path, file: &ManifestFile, full: bool) -> bool {
    let enabled = if full { file.full_check } else { file.quick_check };
    if !enabled {
        return path.is_file();
    }
    file_matches(path, file.size, &file.sha256, full || file.verify_sha256)
}

/// Игра меняет не только содержимое, но и имена поколений индексов.
/// После установки снимка отдельные .idx нельзя проверять или восстанавливать.
fn is_snapshot_index(manifest: &ClientManifest, path: &str) -> bool {
    !manifest.data_revision.is_empty() && path.starts_with("Data/") && path.ends_with(".idx")
}

pub async fn client_status(settings: &Settings) -> Result<ClientStatus, String> {
    let manifest = fetch_client_manifest(settings).await?;
    let build_manifest = fetch_build_manifest(settings).await;
    let root = client_root(settings)?;
    let local = local_version(&root);
    let local_build = crate::config::installed_version(&root.join("_forever_")).unwrap_or_default();
    let wanted_build = required_build(build_manifest.as_ref(), &manifest).to_string();
    let build_differs = !wanted_build.is_empty() && local_build != wanted_build;
    let mut changed_files = 0usize;
    let mut changed_bytes = 0u64;
    for file in &manifest.files {
        if is_snapshot_index(&manifest, &file.path) { continue; }
        let path = root.join(safe_relative(&file.path)?);
        if !file_is_current(&path, file, false) {
            changed_files += 1;
            changed_bytes = changed_bytes.saturating_add(file.size);
        }
    }
    for removed in &manifest.removed_files {
        if !manifest.data_revision.is_empty() && removed.starts_with("Data/") { continue; }
        if root.join(safe_relative(removed)?).exists() {
            changed_files += 1;
        }
    }
    if build_differs || needs_data_bundle(&root, &manifest) {
        // A new game build can change any CASC archive. Only the .build.info
        // marker is not enough: request a full client replacement.
        changed_files = manifest.files.len();
        changed_bytes = manifest.files.iter().map(|file| file.size).sum();
    }
    if needs_full_reinstall(&root, &manifest, build_manifest.as_ref()) {
        // Полная переустановка: показываем реальный объём, даже если
        // отдельные файлы уже совпали по размеру.
        changed_files = manifest.files.len();
        changed_bytes = manifest.files.iter().map(|file| file.size).sum();
    }
    // Отсутствие .launcher_version у готового клиента не делает его устаревшим:
    // сравниваем реальные размеры файлов и build из .build.info.
    let up_to_date = changed_files == 0;
    Ok(ClientStatus {
        version: manifest.version,
        build: wanted_build,
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
    let build_manifest = fetch_build_manifest(settings).await;
    let root = client_root(settings)?;
    let local = local_version(&root);
    let local_build = crate::config::installed_version(&root.join("_forever_")).unwrap_or_default();
    let wanted_build = required_build(build_manifest.as_ref(), &manifest).to_string();
    let build_differs = !wanted_build.is_empty() && local_build != wanted_build;
    let full_reinstall = build_manifest
        .as_ref()
        .map(|build| build.full_reinstall)
        .unwrap_or(false);
    if full_reinstall && (full || needs_full_reinstall(&root, &manifest, build_manifest.as_ref())) {
        return reinstall_client(app, &root, &manifest, build_manifest.as_ref()).await;
    }
    if needs_data_bundle(&root, &manifest) || (full && !manifest.data_revision.is_empty()) {
        return replace_client_bundle(app, &root, &manifest, build_manifest.as_ref()).await;
    }
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
        if is_snapshot_index(&manifest, &file.path) { continue; }
        let relative = safe_relative(&file.path)?;
        let path = root.join(&relative);
        let matches = if full && file.full_check {
            let path = path.clone();
            let size = file.size;
            let sha = file.sha256.clone();
            tokio::task::spawn_blocking(move || file_matches(&path, size, &sha, true))
                .await
                .unwrap_or(false)
        } else {
            file_is_current(&path, file, full)
        };
        if !matches {
            entries.push(entry_for(file, build_manifest.as_ref()));
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
            entries.push(entry_for(file, build_manifest.as_ref()));
        }
    }
    // Новая сборка клиента: скачиваем весь набор файлов, иначе CASC
    // останется смешанным из старых и новых архивов.
    if build_differs {
        let mut present: HashSet<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        for file in &manifest.files {
            if present.insert(file.path.clone()) {
                entries.push(entry_for(file, build_manifest.as_ref()));
            }
        }
    }
    // Если повреждён хотя бы один архив Data, обновляем весь Data целиком:
    // архивы и индексы CASC должны быть из одного набора, иначе клиент
    // может получить несогласованную раскладку.
    if full && entries.iter().any(|entry| entry.path.starts_with("Data/")) {
        let mut present: HashSet<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        for file in &manifest.files {
            if file.path.starts_with("Data/") && present.insert(file.path.clone()) {
                entries.push(entry_for(file, build_manifest.as_ref()));
            }
        }
    }
    let has_stale = manifest
        .removed_files
        .iter()
        .filter(|path| manifest.data_revision.is_empty() || !path.starts_with("Data/"))
        .any(|path| safe_relative(path).map(|value| root.join(value).exists()).unwrap_or(false));

    if entries.is_empty() && local == manifest.version && !has_stale {
        emit(app, "client", done_event(0, 0, 0, 0, "Клиент уже обновлён"));
        return Ok(());
    }

    download_entries(app, "client", &root, entries, DOWNLOAD_CONCURRENCY).await?;

    for removed in &manifest.removed_files {
        if !manifest.data_revision.is_empty() && removed.starts_with("Data/") { continue; }
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
    let build_manifest = fetch_build_manifest(settings).await;
    std::fs::create_dir_all(root)
        .map_err(|error| format!("Не удалось создать папку {}: {error}", root.display()))?;
    let full_reinstall = build_manifest
        .as_ref()
        .map(|build| build.full_reinstall)
        .unwrap_or(false);
    if full_reinstall && (local_version(root) != manifest.version
        || needs_data_bundle(root, &manifest)
        || !full_reinstall_is_done(root, &manifest.version))
    {
        return reinstall_client(app, root, &manifest, build_manifest.as_ref()).await;
    }
    if needs_data_bundle(root, &manifest) {
        return replace_client_bundle(app, root, &manifest, build_manifest.as_ref()).await;
    }
    let mut entries = Vec::new();
    for file in &manifest.files {
        let relative = safe_relative(&file.path)?;
        let path = root.join(&relative);
        if !file_is_current(&path, file, false) {
            entries.push(entry_for(file, build_manifest.as_ref()));
        }
    }
    if entries.is_empty() {
        emit(app, "client", done_event(0, 0, 0, 0, "Клиент уже установлен"));
        return Ok(());
    }
    download_entries(app, "client", root, entries, DOWNLOAD_CONCURRENCY).await?;
    std::fs::write(root.join(".launcher_version"), format!("{}\n", manifest.version))
        .map_err(|error| format!("Не удалось записать версию клиента: {error}"))?;
    emit(app, "client", done_event(0, 0, 0, 0, "Клиент установлен"));
    Ok(())
}

/// Полная переустановка клиента: скачать весь набор заново, сохранить только
/// пользовательские `WTF` и `Interface`, старую папку удалить после успеха.
async fn reinstall_client(
    app: &AppHandle,
    root: &Path,
    manifest: &ClientManifest,
    build_manifest: Option<&BuildManifest>,
) -> Result<(), String> {
    if manifest.version.is_empty()
        || manifest.version.len() > 80
        || !manifest.version.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
    {
        return Err("Некорректная версия клиента для полной переустановки".into());
    }
    let stage = root.join(format!(".forever-update-{}", manifest.version));
    let old_data = root.join(format!(".forever-old-data-{}", manifest.version));
    let old_forever = root.join(format!(".forever-old-forever-{}", manifest.version));

    // Если прошлая версия лаунчера успела перенести WTF/Interface в staging,
    // возвращаем их в клиент до начала новой установки.
    if stage.exists() {
        for directory in ["WTF", "Interface"] {
            let source = stage.join(directory);
            if !source.exists() {
                continue;
            }
            let destination = root.join(directory);
            if destination.exists() {
                merge_directory(&source, &destination)?;
            } else {
                std::fs::rename(&source, &destination)
                    .map_err(|error| format!("Не удалось вернуть {}: {error}", directory))?;
            }
            emit(
                app,
                "client",
                UpdateEvent {
                    kind: "log".into(),
                    file: String::new(),
                    current: 0,
                    total: 0,
                    downloaded: 0,
                    total_bytes: 0,
                    percent: 0.0,
                    message: format!("Восстановлена пользовательская папка {directory} из staging"),
                },
            );
        }
    }

    // Восстанавливаем незавершённую замену Data/_forever_ или убираем
    // остатки уже успешной замены.
    if old_data.exists() {
        if root.join("Data").exists() {
            std::fs::remove_dir_all(&old_data)
                .map_err(|error| format!("Не удалось удалить старую Data: {error}"))?;
        } else {
            std::fs::rename(&old_data, root.join("Data"))
                .map_err(|error| format!("Не удалось восстановить старую Data: {error}"))?;
        }
    }
    if old_forever.exists() {
        if root.join("_forever_/Wow.exe").is_file() {
            return Err(format!("Сохранена предыдущая папка {}. Проверьте в ней WTF и Interface перед повторной установкой; автоматически она не удаляется.", old_forever.display()));
        } else {
            if root.join("_forever_").exists() {
                return Err(format!("Найдены две папки _forever_: текущая и {}. Сохраните WTF/Interface из обеих перед восстановлением; автоматически они не удаляются.", old_forever.display()));
            }
            std::fs::rename(&old_forever, root.join("_forever_"))
                .map_err(|error| format!("Не удалось восстановить старую _forever_: {error}"))?;
        }
    }

    // Чужие и старые staging-папки не удаляем: там могут остаться данные игрока.

    std::fs::create_dir_all(&stage)
        .map_err(|error| format!("Не удалось создать папку установки {}: {error}", stage.display()))?;

    emit(
        app,
        "client",
        UpdateEvent {
            kind: "start".into(),
            file: String::new(),
            current: 0,
            total: manifest.files.len(),
            downloaded: 0,
            total_bytes: manifest.files.iter().map(|file| file.size).sum(),
            percent: 0.0,
            message: "Полная переустановка клиента".into(),
        },
    );

    let mut entries = Vec::new();
    client_phase(app, "Проверяем ранее скачанные файлы…");
    for (index, file) in manifest.files.iter().enumerate() {
        let relative = safe_relative(&file.path)?;
        let path = stage.join(&relative);
        let size = file.size;
        let sha = file.sha256.clone();
        let valid = tokio::task::spawn_blocking(move || file_matches(&path, size, &sha, true))
            .await
            .map_err(|error| error.to_string())?;
        if !valid {
            if is_data_archive(&file.path) {
                client_phase(app, &format!("Сравниваем локальный архив по SHA-256: {}…", file.path));
                let source = root.join(&relative);
                let destination = stage.join(&relative);
                let file = file.clone();
                let reused = tokio::task::spawn_blocking(move || reuse_matching_archive(&source, &destination, &file))
                    .await.map_err(|e| e.to_string())??;
                if reused {
                    client_phase(app, &format!("{}: SHA-256 совпадает, повторная загрузка не нужна", relative.display()));
                    continue;
                }
            }
            entries.push(entry_for(file, build_manifest));
        }
        if index % 25 == 0 || index + 1 == manifest.files.len() {
            client_phase(app, &format!("Проверка загрузки: {}/{} · {}", index + 1, manifest.files.len(), file.path));
        }
    }
    download_entries(app, "client", &stage, entries, DOWNLOAD_CONCURRENCY).await?;

    client_phase(app, "Загрузка завершена. Проверяем комплектность клиента…");
    for file in &manifest.files {
        let path = stage.join(safe_relative(&file.path)?);
        if !path.is_file() {
            return Err(format!("После загрузки отсутствует файл {}", file.path));
        }
    }
    if !stage.join("Data/data").is_dir() || !stage.join("_forever_/Wow.exe").is_file() {
        return Err("Полный набор клиента не содержит Data/data или _forever_/Wow.exe".into());
    }

    client_phase(app, "Сохраняем настройки и аддоны из _forever_/WTF и _forever_/Interface…");
    preserve_user_directories(root, &stage)?;

    // Меняем только игровые каталоги. Саму папку клиента не переименовываем:
    // в ней может лежать запущенный лаунчер, Explorer или антивирус.
    emit(
        app,
        "client",
        UpdateEvent {
            kind: "log".into(),
            file: String::new(),
            current: manifest.files.len(),
            total: manifest.files.len(),
            downloaded: 0,
            total_bytes: 0,
            percent: 99.0,
            message: "Загрузка завершена, заменяем Data и _forever_…".into(),
        },
    );

    let had_data = root.join("Data").exists();
    if had_data {
        std::fs::rename(root.join("Data"), &old_data)
            .map_err(|error| format!("Не удалось переместить старую Data: {error}"))?;
    }
    let had_forever = root.join("_forever_").exists();
    if had_forever {
        if let Err(error) = std::fs::rename(root.join("_forever_"), &old_forever) {
            if had_data {
                let _ = std::fs::rename(&old_data, root.join("Data"));
            }
            return Err(format!("Не удалось переместить старую _forever_: {error}"));
        }
    }
    if let Err(error) = std::fs::rename(stage.join("Data"), root.join("Data")) {
        if had_forever {
            let _ = std::fs::rename(&old_forever, root.join("_forever_"));
        }
        if had_data {
            let _ = std::fs::rename(&old_data, root.join("Data"));
        }
        return Err(format!("Не удалось установить новую Data: {error}"));
    }
    if let Err(error) = std::fs::rename(stage.join("_forever_"), root.join("_forever_")) {
        let _ = std::fs::rename(root.join("Data"), stage.join("Data"));
        if had_forever {
            let _ = std::fs::rename(&old_forever, root.join("_forever_"));
        }
        if had_data {
            let _ = std::fs::rename(&old_data, root.join("Data"));
        }
        return Err(format!("Не удалось установить новую _forever_: {error}"));
    }

    // Остальные файлы манифеста (например, .build.info) копируем поверх.
    for file in &manifest.files {
        if file.path.starts_with("Data/") || file.path.starts_with("_forever_/") {
            continue;
        }
        let relative = safe_relative(&file.path)?;
        let source = stage.join(&relative);
        let destination = root.join(&relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::copy(&source, &destination)
            .map_err(|error| format!("Не удалось установить {}: {error}", file.path))?;
    }

    if !manifest.data_revision.is_empty() {
        std::fs::write(root.join(".forever_data_revision"), format!("{}\n", manifest.data_revision))
            .map_err(|error| format!("Не удалось записать ревизию клиента: {error}"))?;
    }
    std::fs::write(root.join(".launcher_version"), format!("{}\n", manifest.version))
        .map_err(|error| format!("Не удалось записать версию клиента: {error}"))?;
    std::fs::write(root.join(FULL_REINSTALL_MARKER), format!("{}\n", manifest.version))
        .map_err(|error| format!("Не удалось записать маркер полной установки: {error}"))?;

    client_phase(app, "Клиент установлен. Удаляем старые игровые файлы; резерв настроек остаётся в .forever-user-backups…");
    if old_data.exists() {
        if let Err(error) = std::fs::remove_dir_all(&old_data) {
            eprintln!("не удалось удалить старую Data {}: {error}", old_data.display());
        }
    }
    if old_forever.exists() {
        if let Err(error) = std::fs::remove_dir_all(&old_forever) {
            eprintln!("не удалось удалить старую _forever_ {}: {error}", old_forever.display());
        }
    }
    // Старые служебные каталоги клиента; WTF и Interface не трогаем.
    for directory in ["Cache", "Errors", "Logs"] {
        let path = root.join(directory);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    let _ = std::fs::remove_dir_all(&stage);
    emit(app, "client", done_event(0, 0, 0, 0, "Клиент переустановлен полностью"));
    Ok(())
}

/// Переносит содержимое каталога в целевой, не теряя уже существующие файлы.
/// Нужен для восстановления WTF/Interface из staging после неудачной замены.
fn merge_directory(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir_all(destination)
        .map_err(|error| format!("Не удалось создать {}: {error}", destination.display()))?;
    let entries = std::fs::read_dir(source)
        .map_err(|error| format!("Не удалось прочитать {}: {error}", source.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            if target.exists() && !target.is_dir() {
                std::fs::remove_file(&target).map_err(|error| error.to_string())?;
            }
            merge_directory(&path, &target)?;
        } else {
            if target.exists() {
                if target.is_dir() {
                    std::fs::remove_dir_all(&target).map_err(|error| error.to_string())?;
                } else {
                    std::fs::remove_file(&target).map_err(|error| error.to_string())?;
                }
            }
            std::fs::rename(&path, &target)
                .map_err(|error| format!("Не удалось перенести {}: {error}", path.display()))?;
        }
    }
    let _ = std::fs::remove_dir(source);
    Ok(())
}

/// Копирование без удаления источника; ссылки не разыменовываем за пределы клиента.
fn copy_user_directory(source: &Path, destination: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(source).map_err(|e| format!("{}: {e}", source.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("Папка настроек содержит ссылку {}. Исходные данные сохранены; требуется ручная проверка ссылки.", source.display()));
    }
    if meta.is_dir() {
        std::fs::create_dir_all(destination).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_user_directory(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(source, destination).map_err(|e| format!("{}: {e}", source.display()))?;
    }
    Ok(())
}

fn preserve_user_directories(root: &Path, stage: &Path) -> Result<(), String> {
    // Уникальный резерв не входит ни в staging, ни в заменяемый _forever_.
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?.as_nanos();
    let backup = root.join(".forever-user-backups").join(stamp.to_string());
    for relative in ["WTF", "Interface", "_forever_/WTF", "_forever_/Interface"] {
        let source = root.join(relative);
        match std::fs::symlink_metadata(&source) {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("Не удалось проверить {}: {e}", source.display())),
        }
        let saved = backup.join(relative);
        if let Some(parent) = saved.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        copy_user_directory(&source, &saved)?;
        if relative.starts_with("_forever_/") {
            let target = stage.join(relative);
            // Копируем из сохранённого резерва; ошибка прерывает замену каталогов.
            copy_user_directory(&saved, &target)?;
        }
    }
    Ok(())
}

fn client_phase(app: &AppHandle, message: &str) {
    emit(app, "client", UpdateEvent {
        kind: "log".into(), file: String::new(), current: 0, total: 0,
        downloaded: 0, total_bytes: 0, percent: 0.0, message: message.into(),
    });
}

fn is_data_archive(path: &str) -> bool {
    path.strip_prefix("Data/data/data.")
        .map(|suffix| suffix.len() == 3 && suffix.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// Только целые совпавшие архивы. Копия в staging независима от изменяемого
/// клиентом источника; после копирования SHA-256 проверяется ещё раз.
fn reuse_matching_archive(source: &Path, destination: &Path, file: &ManifestFile) -> Result<bool, String> {
    if !is_data_archive(&file.path) || !file_matches(source, file.size, &file.sha256, true) {
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temporary = destination.with_extension("reuse-part");
    let result = (|| {
        std::fs::copy(source, &temporary).map_err(|e| format!("Копирование {}: {e}", file.path))?;
        if !file_matches(&temporary, file.size, &file.sha256, true) { return Ok(false); }
        if destination.exists() { std::fs::remove_file(destination).map_err(|e| e.to_string())?; }
        std::fs::rename(&temporary, destination).map_err(|e| e.to_string())?;
        Ok(true)
    })();
    if temporary.exists() { let _ = std::fs::remove_file(temporary); }
    result
}

fn needs_data_bundle(root: &Path, manifest: &ClientManifest) -> bool {
    !manifest.data_revision.is_empty()
        && (std::fs::read_to_string(root.join(".forever_data_revision"))
            .map(|s| s.trim() != manifest.data_revision)
            .unwrap_or(true)
            || manifest.files.iter().any(|file| file.path.starts_with("Data/")
                && !is_snapshot_index(manifest, &file.path)
                && !root.join(&file.path).is_file()))
}

/// CASC нельзя обновлять поверх произвольной локальной раскладки.
/// Сначала скачиваем и проверяем новый набор, затем заменяем Data целиком.
async fn replace_client_bundle(
    app: &AppHandle,
    root: &Path,
    manifest: &ClientManifest,
    build_manifest: Option<&BuildManifest>,
) -> Result<(), String> {
    if manifest.data_revision.len() > 80
        || !manifest.data_revision.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("Некорректная ревизия данных клиента".into());
    }
    let stage = root.join(format!(".forever-update-{}", manifest.data_revision));
    let backup = root.join(format!(".forever-old-data-{}", manifest.data_revision));
    // Остаток незавершённой замены не удаляем: в нём могут быть единственные данные.
    if backup.exists() {
        return Err(format!("Обнаружена резервная папка {}. Сохраните её и восстановите Data перед повторным обновлением.", backup.display()));
    }
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    if manifest.reuse_manifest.is_some() && root.join("Data").is_dir() {
        reuse_archives(app, root, &stage, manifest).await?;
    }
    let mut entries = Vec::new();
    for file in &manifest.files {
        let path = stage.join(safe_relative(&file.path)?);
        let size = file.size;
        let sha = file.sha256.clone();
        let valid = tokio::task::spawn_blocking(move || file_matches(&path, size, &sha, true))
            .await.map_err(|e| e.to_string())?;
        if !valid {
            // Неизменившиеся EXE, конфиги и мелкие файлы тоже не скачиваем.
            let source = root.join(safe_relative(&file.path)?);
            let destination = stage.join(safe_relative(&file.path)?);
            let sha = file.sha256.clone();
            let copied = tokio::task::spawn_blocking(move || -> Result<bool, String> {
                if !file_matches(&source, size, &sha, true) { return Ok(false); }
                if let Some(parent) = destination.parent() { std::fs::create_dir_all(parent).map_err(|e|e.to_string())?; }
                std::fs::copy(source, &destination).map_err(|e|e.to_string())?;
                // Источник мог измениться во время копирования.
                if file_matches(&destination, size, &sha, true) { Ok(true) }
                else { std::fs::remove_file(destination).map_err(|e|e.to_string())?; Ok(false) }
            }).await.map_err(|e|e.to_string())??;
            if copied { continue; }
            entries.push(entry_for(file, build_manifest));
        }
    }
    download_entries(app, "client", &stage, entries, DOWNLOAD_CONCURRENCY).await?;
    if !stage.join("Data/data").is_dir() {
        return Err("Манифест не содержит полный набор Data/data".into());
    }
    let had_data = root.join("Data").exists();
    if had_data {
        std::fs::rename(root.join("Data"), &backup).map_err(|e| format!("Не удалось сохранить старую Data: {e}"))?;
    }
    if let Err(e) = std::fs::rename(stage.join("Data"), root.join("Data")) {
        if had_data { let _ = std::fs::rename(&backup, root.join("Data")); }
        return Err(format!("Не удалось установить новую Data: {e}"));
    }
    // Только файлы манифеста: WTF, аддоны и пользовательские настройки не трогаем.
    for file in &manifest.files {
        if file.path.starts_with("Data/") { continue; }
        let relative = safe_relative(&file.path)?;
        let destination = root.join(&relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(stage.join(relative), destination).map_err(|e| format!("{}: {e}", file.path))?;
    }
    // Маркер записывается только после успешной установки всего набора.
    std::fs::write(root.join(".forever_data_revision"), format!("{}\n", manifest.data_revision)).map_err(|e| e.to_string())?;
    std::fs::write(root.join(".launcher_version"), format!("{}\n", manifest.version)).map_err(|e| e.to_string())?;
    // Это только созданные нами служебные папки, не пользовательские каталоги.
    if had_data { let _ = std::fs::remove_dir_all(&backup); }
    let _ = std::fs::remove_dir_all(&stage);
    emit(app, "client", done_event(0, 0, 0, 0, "Полный набор клиента обновлён"));
    Ok(())
}

async fn bounded_body(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    if response.content_length().map(|n|n > limit).unwrap_or(false) { return Err("Ответ больше ожидаемого".into()); }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e|e.to_string())? {
        if data.len() as u64 + chunk.len() as u64 > limit { return Err("Превышен размер ответа".into()); }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

fn reuse_event(app: &AppHandle, message: String, current: usize, total: usize, downloaded: u64) {
    emit(app, "client", UpdateEvent { kind: "reuse".into(), file: String::new(), current, total,
        downloaded, total_bytes: 0, percent: if total == 0 { 0.0 } else { current as f64 / total as f64 * 100.0 }, message });
}

async fn reuse_archives(app: &AppHandle, root: &Path, stage: &Path, manifest: &ClientManifest) -> Result<(), String> {
    use crate::casc_reuse;
    let reference = manifest.reuse_manifest.as_ref().ok_or("Нет рецепта")?;
    if reference.size == 0 || reference.size > 128 * 1024 * 1024 { return Err("Недопустимый размер рецепта".into()); }
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).timeout(Duration::from_secs(120)).build().map_err(|e|e.to_string())?;
    reuse_event(app, "Читаем карту блоков обновления…".into(), 0, 0, 0);
    let response = client.get(&reference.url).header("Accept-Encoding", "identity").send().await.map_err(|e|e.to_string())?.error_for_status().map_err(|e|e.to_string())?;
    let bytes = bounded_body(response, reference.size).await?;
    if bytes.len() as u64 != reference.size || hex::encode(Sha256::digest(&bytes)) != reference.sha256 { return Err("SHA-256 карты блоков не совпадает".into()); }
    let recipe: casc_reuse::Recipe = serde_json::from_slice(&bytes).map_err(|e|e.to_string())?;
    casc_reuse::validate(&recipe, &manifest.data_revision)?;
    for archive in &recipe.files {
        let file = manifest.files.iter().find(|f|f.path == archive.path).ok_or("Архив не входит в манифест")?;
        if file.size != archive.size || file.sha256 != archive.sha256 { return Err("Карта блоков относится к другому снимку".into()); }
    }
    let source = root.to_path_buf(); let scan_recipe = recipe.clone(); let scan_app = app.clone();
    let index = tokio::task::spawn_blocking(move || casc_reuse::scan(&source, &scan_recipe, |path, read| {
        reuse_event(&scan_app, format!("Проверяем локальные архивы: {:.1} ГиБ · {path}", read as f64 / 1073741824.0), 0, 0, 0);
    })).await.map_err(|e|e.to_string())??;
    let index = Arc::new(index);
    // При пустом/неподходящем клиенте эффективнее обычная параллельная загрузка.
    if index.is_empty() { return Ok(()); }
    let mut downloaded = 0u64; let mut reused = 0u64;
    for (number, archive) in recipe.files.iter().enumerate() {
        let destination = stage.join(&archive.path);
        let check_path = destination.clone(); let sha = archive.sha256.clone(); let size = archive.size;
        if tokio::task::spawn_blocking(move ||file_matches(&check_path, size, &sha, true)).await.map_err(|e|e.to_string())? { continue; }
        reuse_event(app, format!("Собираем {} из локальных блоков…",archive.path), number, recipe.files.len(), downloaded);
        let build_stage = stage.to_path_buf(); let build_archive = archive.clone(); let build_index = index.clone();
        let (temporary, ranges, copied) = tokio::task::spawn_blocking(move || casc_reuse::assemble(&build_stage, &build_archive, &build_index)).await.map_err(|e|e.to_string())??;
        reused += copied;
        let url = &manifest.files.iter().find(|f|f.path == archive.path).ok_or("Нет URL архива")?.http_url;
        for range in ranges {
            let mut result = Err("Не удалось скачать диапазон".to_string());
            for _ in 0..3 {
                result = async {
                    let end = range.offset.checked_add(range.size).ok_or("Переполнение диапазона")? - 1;
                    let response = client.get(url).header("Accept-Encoding", "identity")
                        .header("Range", format!("bytes={}-{end}", range.offset)).send().await.map_err(|e|e.to_string())?;
                    let expected = format!("bytes {}-{end}/{}",range.offset,archive.size);
                    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
                        || response.headers().get("Content-Range").and_then(|h|h.to_str().ok()) != Some(expected.as_str()) {
                        return Err("Сервер не подтвердил точный диапазон; полная загрузка автоматически не запускается".into());
                    }
                    let data = bounded_body(response, range.size).await?;
                    casc_reuse::verify_range(&data, &range)?;
                    Ok(data)
                }.await;
                if result.is_ok() { break; }
            }
            let data = result?;
            let path = temporary.clone(); let offset = range.offset;
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                use std::io::{Seek, SeekFrom, Write};
                let mut file = std::fs::OpenOptions::new().write(true).open(path).map_err(|e|e.to_string())?;
                file.seek(SeekFrom::Start(offset)).and_then(|_|file.write_all(&data)).map_err(|e|e.to_string())
            }).await.map_err(|e|e.to_string())??;
            downloaded += range.size;
            reuse_event(app, format!("Повторно использовано {:.2} ГиБ; загружено {:.2} МиБ",reused as f64/1073741824.0,downloaded as f64/1048576.0),number,recipe.files.len(),downloaded);
        }
        let path = temporary.clone(); let sha = archive.sha256.clone();
        let valid = tokio::task::spawn_blocking(move ||file_matches(&path, size, &sha, true)).await.map_err(|e|e.to_string())?;
        if !valid { return Err(format!("Итоговый SHA-256 архива {} не совпадает; старый клиент не изменён",archive.path)); }
        // На Windows rename не заменяет существующий файл; удаляем только неверный staging-файл.
        if destination.exists() { std::fs::remove_file(&destination).map_err(|e|e.to_string())?; }
        std::fs::rename(temporary, destination).map_err(|e|e.to_string())?;
    }
    reuse_event(app, format!("Архивы готовы: повторно использовано {:.2} ГиБ, загружено {:.2} МиБ",reused as f64/1073741824.0,downloaded as f64/1048576.0),recipe.files.len(),recipe.files.len(),downloaded);
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

/// Yandex Disk public API returns JSON `{ "href": "..." }`. The href is temporary,
/// so it is resolved for each file immediately before downloading.
async fn resolve_yandex_url(
    client: &reqwest::Client,
    api_url: &str,
    entry: &DownloadEntry,
) -> Result<String, String> {
    let response = client
        .get(api_url)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("{}: {error}", entry.path))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{}: Yandex API HTTP {status}", entry.path));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("{}: Yandex API response: {error}", entry.path))?;
    body.get("href")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{}: Yandex API returned no href", entry.path))
}

async fn download_one(
    client: &reqwest::Client,
    url: &str,
    entry: &DownloadEntry,
    destination: &Path,
    temporary: &Path,
    app: &AppHandle,
    event_name: &str,
    total: usize,
    current: &AtomicUsize,
    downloaded: &AtomicU64,
    total_bytes: u64,
    is_cdn: bool,
) -> Result<(), String> {
    let mut written = 0u64;
    let result = async {
        let request = client.get(url);
        // A stalled CDN must fall back to our server instead of blocking the install.
        let request = if is_cdn { request.timeout(Duration::from_secs(60)) } else { request };
        let response = request
            .send()
            .await
            .map_err(|error| format!("{}: {error}", entry.path))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{}: HTTP {status}", entry.path));
        }
        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(temporary)
            .await
            .map_err(|error| format!("{}: {error}", entry.path))?;
        use tokio::io::AsyncWriteExt;
        let mut hasher = Sha256::new();
        let mut last_emit = Instant::now();
        loop {
            let next = tokio::time::timeout(READ_TIMEOUT, stream.next())
                .await
                .map_err(|_| format!("{}: таймаут чтения от источника", entry.path))?;
            let chunk = match next {
                Some(chunk) => chunk.map_err(|error| format!("{}: {error}", entry.path))?,
                None => break,
            };
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
                    app,
                    event_name,
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
                        message: format!("{} · {:.1}% файла · {:.1}/{:.1} МиБ", entry.path,
                            if entry.size == 0 { 100.0 } else { written as f64 / entry.size as f64 * 100.0 },
                            written as f64 / 1048576.0, entry.size as f64 / 1048576.0),
                    },
                );
            }
        }
        file.flush()
            .await
            .map_err(|error| format!("{}: {error}", entry.path))?;
        drop(file);
        if written != entry.size {
            return Err(format!(
                "{}: ожидалось {} байт, получено {}",
                entry.path, entry.size, written
            ));
        }
        let digest = hex::encode(hasher.finalize());
        if digest != entry.sha256 {
            return Err(format!("{}: неверная SHA-256", entry.path));
        }
        if destination.exists() {
            tokio::fs::remove_file(destination)
                .await
                .map_err(|error| format!("{}: {error}", entry.path))?;
        }
        tokio::fs::rename(temporary, destination)
            .await
            .map_err(|error| format!("{}: {error}", entry.path))?;
        let done = current.fetch_add(1, Ordering::Relaxed) + 1;
        let all = downloaded.load(Ordering::Relaxed);
        emit(
            app,
            event_name,
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
                message: format!("{} · 100% файла · SHA-256 проверен", entry.path),
            },
        );
        Ok::<(), String>(())
    }
    .await;
    if result.is_err() {
        downloaded.fetch_sub(written, Ordering::Relaxed);
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result
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
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let root = root.to_path_buf();
    let app = app.clone();
    let event_name = event.to_string();

    let cdn_ok = Arc::new(AtomicUsize::new(0));
    let cdn_fallbacks = Arc::new(AtomicUsize::new(0));
    let cdn_failures = Arc::new(AtomicUsize::new(0));
    let cdn_disabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let yandex_ok = Arc::new(AtomicUsize::new(0));
    let tasks = entries.into_iter().map(|entry| {
        let client = client.clone();
        let root = root.clone();
        let app = app.clone();
        let event_name = event_name.clone();
        let current = Arc::clone(&current);
        let downloaded = Arc::clone(&downloaded);
        let cdn_ok = Arc::clone(&cdn_ok);
        let cdn_fallbacks = Arc::clone(&cdn_fallbacks);
        let cdn_failures = Arc::clone(&cdn_failures);
        let cdn_disabled = Arc::clone(&cdn_disabled);
        let yandex_ok = Arc::clone(&yandex_ok);
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

            let mut last_error = None;
            for attempt in 0..=MAX_FILE_RETRIES {
                let cdn_count = if cdn_disabled.load(Ordering::Relaxed) { 0 } else { entry.cdn_urls.len() };
                let mut attempts: Vec<(String, bool, bool)> = Vec::new();
                if let Some(url) = entry.yandex_url.as_ref() {
                    attempts.push((url.clone(), true, false));
                }
                for url in entry.cdn_urls.iter().take(cdn_count) {
                    attempts.push((url.clone(), false, true));
                }
                if entry.allow_server_fallback {
                    attempts.push((entry.url.clone(), false, false));
                }
                for (url, yandex, cdn) in &attempts {
                    let actual = if *yandex {
                        match resolve_yandex_url(&client, url, &entry).await {
                            Ok(href) => href,
                            Err(error) => {
                                last_error = Some(error);
                                continue;
                            }
                        }
                    } else {
                        url.clone()
                    };
                    match download_one(
                        &client,
                        &actual,
                        &entry,
                        &destination,
                        &temporary,
                        &app,
                        &event_name,
                        total,
                        &current,
                        &downloaded,
                        total_bytes,
                        *cdn,
                    )
                    .await
                    {
                        Ok(()) => {
                            if *yandex {
                                yandex_ok.fetch_add(1, Ordering::Relaxed);
                            } else if *cdn {
                                cdn_ok.fetch_add(1, Ordering::Relaxed);
                            } else if attempts.len() > 1 {
                                cdn_fallbacks.fetch_add(1, Ordering::Relaxed);
                            }
                            return Ok::<(), String>(());
                        }
                        Err(error) => {
                            if *cdn {
                                if cdn_failures.fetch_add(1, Ordering::Relaxed) + 1 >= 20 {
                                    cdn_disabled.store(true, Ordering::Relaxed);
                                }
                            }
                            last_error = Some(error);
                        }
                    }
                }
                if attempt < MAX_FILE_RETRIES {
                    let message = format!(
                        "{}: {} · повтор {}/{} через {} с",
                        entry.path,
                        last_error.as_deref().unwrap_or("нет источника загрузки"),
                        attempt + 1,
                        MAX_FILE_RETRIES,
                        RETRY_DELAY.as_secs()
                    );
                    emit(
                        &app,
                        &event_name,
                        UpdateEvent {
                            kind: "retry".into(),
                            file: entry.path.clone(),
                            current: current.load(Ordering::Relaxed),
                            total,
                            downloaded: downloaded.load(Ordering::Relaxed),
                            total_bytes,
                            percent: if total_bytes > 0 {
                                (downloaded.load(Ordering::Relaxed) as f64 / total_bytes as f64 * 100.0).min(99.9)
                            } else {
                                0.0
                            },
                            message,
                        },
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
            Err(format!(
                "{}: не удалось скачать после {} попыток: {}",
                entry.path,
                MAX_FILE_RETRIES + 1,
                last_error.unwrap_or_else(|| "нет источника загрузки".into())
            ))
        }
    });

    let results: Vec<Result<(), String>> = stream::iter(tasks).buffer_unordered(concurrency).collect().await;
    let direct = cdn_ok.load(Ordering::Relaxed);
    let fallback = cdn_fallbacks.load(Ordering::Relaxed);
    let yandex = yandex_ok.load(Ordering::Relaxed);
    if direct + fallback + yandex > 0 {
        emit(
            &app,
            event,
            UpdateEvent {
                kind: "log".into(),
                file: String::new(),
                current: yandex + direct,
                total: yandex + direct + fallback,
                downloaded: 0,
                total_bytes: 0,
                percent: 0.0,
                message: format!(
                    "Источники файлов: Yandex Disk — {yandex}, CDN Blizzard — {direct}, резервный сервер — {fallback}"
                ),
            },
        );
    }
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


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn manifest_file(path: &str, size: u64, sha256: &str) -> ManifestFile {
        ManifestFile {
            path: path.into(),
            size,
            sha256: sha256.into(),
            http_url: String::new(),
            verify_sha256: false,
            quick_check: true,
            full_check: true,
        }
    }

    fn temp_file(contents: &[u8]) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("forever-launcher-test-{unique}"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn reinstall_preserves_nested_user_data_and_backup() {
        let marker = temp_file(b"preserve");
        let root = marker.with_extension("client");
        let stage = root.join(".forever-update-test");
        for relative in ["WTF", "Interface", "_forever_/WTF", "_forever_/Interface"] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
            std::fs::write(root.join(relative).join("custom.txt"), relative).unwrap();
        }
        std::fs::create_dir_all(stage.join("_forever_")).unwrap();
        std::fs::write(stage.join("_forever_/Wow.exe"), b"new game").unwrap();
        preserve_user_directories(&root, &stage).unwrap();
        for relative in ["_forever_/WTF", "_forever_/Interface"] {
            assert_eq!(std::fs::read(stage.join(relative).join("custom.txt")).unwrap(), relative.as_bytes());
            assert!(root.join(relative).join("custom.txt").exists());
        }
        let backup = std::fs::read_dir(root.join(".forever-user-backups")).unwrap().next().unwrap().unwrap().path();
        for relative in ["WTF", "Interface", "_forever_/WTF", "_forever_/Interface"] {
            assert_eq!(std::fs::read(backup.join(relative).join("custom.txt")).unwrap(), relative.as_bytes());
        }
        assert_eq!(std::fs::read(stage.join("_forever_/Wow.exe")).unwrap(), b"new game");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(marker).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn user_backup_refuses_symlinks_without_deleting_originals() {
        let marker = temp_file(b"symlink");
        let root = marker.with_extension("client");
        let stage = root.join(".forever-update-test");
        std::fs::create_dir_all(root.join("_forever_/WTF")).unwrap();
        std::fs::write(root.join("_forever_/WTF/Config.wtf"), b"my settings").unwrap();
        std::os::unix::fs::symlink("WTF", root.join("_forever_/Interface")).unwrap();
        assert!(preserve_user_directories(&root, &stage).is_err());
        assert_eq!(std::fs::read(root.join("_forever_/WTF/Config.wtf")).unwrap(), b"my settings");
        assert!(root.join("_forever_/Interface").is_symlink());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(marker).unwrap();
    }

    #[test]
    fn archive_reuse_requires_exact_content_not_just_build_or_size() {
        let source = temp_file(b"same archive");
        let destination = source.with_extension("reused");
        let file = manifest_file("Data/data/data.000", 12, &hex::encode(Sha256::digest(b"same archive")));
        assert!(reuse_matching_archive(&source, &destination, &file).unwrap());
        assert_eq!(std::fs::read(&destination).unwrap(), b"same archive");
        std::fs::remove_file(&destination).unwrap();
        std::fs::write(&source, b"bad! archive").unwrap();
        assert!(!reuse_matching_archive(&source, &destination, &file).unwrap());
        assert!(!destination.exists());
        let index = manifest_file("Data/data/0000000008.idx", 12, &hex::encode(Sha256::digest(b"bad! archive")));
        assert!(!reuse_matching_archive(&source, &destination, &index).unwrap());
        assert!(!is_data_archive("Data/data/data.000/other"));
        assert!(!is_data_archive("Data/data/data.idx"));
        std::fs::remove_file(&source).unwrap();
    }

    #[test]
    fn user_data_merge_keeps_both_sides() {
        let path = temp_file(b"merge");
        let source = path.with_extension("source");
        let target = path.with_extension("target");
        std::fs::create_dir_all(source.join("AddOns")).unwrap();
        std::fs::create_dir_all(target.join("AddOns")).unwrap();
        std::fs::write(source.join("Config.wtf"), b"new").unwrap();
        std::fs::write(target.join("Config.wtf"), b"old").unwrap();
        std::fs::write(source.join("AddOns/a.txt"), b"a").unwrap();
        std::fs::write(target.join("AddOns/b.txt"), b"b").unwrap();
        merge_directory(&source, &target).unwrap();
        assert_eq!(std::fs::read(target.join("Config.wtf")).unwrap(), b"new");
        assert_eq!(std::fs::read(target.join("AddOns/a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(target.join("AddOns/b.txt")).unwrap(), b"b");
        std::fs::remove_dir_all(target).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn full_reinstall_requires_its_own_marker() {
        let path = temp_file(b"marker");
        let root = path.with_extension("client");
        std::fs::create_dir(&root).unwrap();
        let manifest: ClientManifest = serde_json::from_str(r#"{"version":"v2","build":"","data_revision":"fixed","files":[]}"#).unwrap();
        let mut build = build_manifest();
        build.build.clear();
        build.full_reinstall = true;
        std::fs::write(root.join(".launcher_version"), "v2\n").unwrap();
        assert!(needs_full_reinstall(&root, &manifest, Some(&build)));
        std::fs::write(root.join(FULL_REINSTALL_MARKER), "v2\n").unwrap();
        assert!(!needs_full_reinstall(&root, &manifest, Some(&build)));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn casc_revision_requires_replacement_even_when_game_build_matches() {
        let path = temp_file(b"marker");
        let root = path.with_extension("client");
        std::fs::create_dir(&root).unwrap();
        let mut manifest: ClientManifest = serde_json::from_str(r#"{"version":"new","build":"1.60.1.70888","data_revision":"70888-fixed","files":[]}"#).unwrap();
        assert!(needs_data_bundle(&root, &manifest));
        std::fs::write(root.join(".forever_data_revision"), "old\n").unwrap();
        assert!(needs_data_bundle(&root, &manifest));
        std::fs::write(root.join(".forever_data_revision"), "70888-fixed\n").unwrap();
        assert!(!needs_data_bundle(&root, &manifest));
        manifest.data_revision.clear();
        assert!(!needs_data_bundle(&root, &manifest));
        std::fs::remove_file(root.join(".forever_data_revision")).unwrap();
        std::fs::remove_dir(root).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_ignores_renamed_indices_but_restores_missing_archives() {
        let path = temp_file(b"marker");
        let root = path.with_extension("client");
        std::fs::create_dir_all(root.join("Data/data")).unwrap();
        std::fs::write(root.join(".forever_data_revision"), "fixed\n").unwrap();
        let mut manifest: ClientManifest = serde_json::from_str(r#"{"version":"v","data_revision":"fixed","files":[]}"#).unwrap();
        manifest.files.push(manifest_file("Data/data/000000000e.idx", 99, "old"));
        manifest.files.push(manifest_file("Data/wowf/000000000e.idx", 99, "old"));
        manifest.files.push(manifest_file("Data/ecache/000000000e.idx", 99, "old"));
        manifest.files.push(manifest_file("Data/data/data.000", 99, "old"));
        // Старых имён нет; игра создала другое поколение и изменила архив.
        std::fs::write(root.join("Data/data/000000000f.idx"), "new index").unwrap();
        std::fs::write(root.join("Data/data/data.000"), "modified by game").unwrap();
        for file in &manifest.files[..3] {
            assert!(is_snapshot_index(&manifest, &file.path));
        }
        assert!(!needs_data_bundle(&root, &manifest));
        assert!(!is_snapshot_index(&manifest, "_forever_/Wow.exe"));
        std::fs::remove_file(root.join("Data/data/data.000")).unwrap();
        assert!(needs_data_bundle(&root, &manifest));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn client_managed_file_only_needs_to_exist() {
        let path = temp_file(b"client-managed");
        let mut file = manifest_file("Data/data/000000000e.idx", 999, "wrong");
        file.quick_check = false;
        file.full_check = false;

        assert!(file_is_current(&path, &file, false));
        assert!(file_is_current(&path, &file, true));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn regular_file_is_checked_by_size_and_optional_sha() {
        let path = temp_file(b"hello");
        let mut file = manifest_file("_forever_/Wow.exe", 5, "wrong");
        assert!(file_is_current(&path, &file, false));

        file.size = 4;
        assert!(!file_is_current(&path, &file, false));

        file.size = 5;
        file.verify_sha256 = true;
        assert!(!file_is_current(&path, &file, false));
        std::fs::remove_file(path).unwrap();
    }

    fn build_manifest() -> BuildManifest {
        BuildManifest {
            build: "1.60.1.70888".into(),
            build_id: 70888,
            product: "wowf".into(),
            cdn: Some(CdnInfo {
                product: "wowf".into(),
                build_config: "0597e74b01a3f779d849ffe9b3270196".into(),
                cdn_config: "8ecf609471cb2ea16667be9cc2b7a555".into(),
                cdn_path: "tpr/wow".into(),
                cdn_hosts: vec!["level3.blizzard.com".into(), "us.cdn.blizzard.com".into()],
            }),
            yandex: None,
            overlay: vec!["_forever_/wow_loader.dll".into()],
            full_reinstall: false,
            fallback_manifest: "manifest.json".into(),
        }
    }

    #[test]
    fn cdn_is_used_only_for_content_addressed_metadata() {
        let build = build_manifest();
        let index = "Data/indices/0017a402f556fbece46c38dc431a2c9b.index";
        let urls = cdn_urls_for(index, Some(&build));
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://level3.blizzard.com/tpr/wow/data/00/17/0017a402f556fbece46c38dc431a2c9b.index");

        let config = "Data/config/05/97/0597e74b01a3f779d849ffe9b3270196";
        assert_eq!(
            cdn_urls_for(config, Some(&build))[1],
            "https://us.cdn.blizzard.com/tpr/wow/config/05/97/0597e74b01a3f779d849ffe9b3270196"
        );
        // Bulk CASC archives are not flat CDN objects and must stay on our server.
        assert!(cdn_urls_for("Data/data/data.000", Some(&build)).is_empty());
        assert!(cdn_urls_for("Data/config/encoding.info", Some(&build)).is_empty());
        assert!(cdn_urls_for("_forever_/wow_loader.dll", Some(&build)).is_empty());
        assert!(cdn_urls_for("_forever_/Wow.exe", Some(&build)).is_empty());
    }

    #[test]
    fn overlay_list_can_force_a_file_to_our_server() {
        let mut build = build_manifest();
        build.overlay.push("Data/config/05/97/0597e74b01a3f779d849ffe9b3270196".into());
        assert!(cdn_urls_for("Data/config/05/97/0597e74b01a3f779d849ffe9b3270196", Some(&build)).is_empty());
    }

    #[test]
    fn required_build_comes_from_the_small_manifest() {
        let mut manifest: ClientManifest = serde_json::from_str(r#"{"version":"v","build":"","files":[]}"#).unwrap();
        let build = build_manifest();
        assert_eq!(required_build(Some(&build), &manifest), "1.60.1.70888");
        assert_eq!(required_build(None, &manifest), "");
        manifest.build = "1.60.1.70000".into();
        assert_eq!(required_build(None, &manifest), "1.60.1.70000");
        assert_eq!(required_build(Some(&build), &manifest), "1.60.1.70888");
    }

    #[test]
    fn yandex_url_is_derived_for_every_client_file() {
        let mut build = build_manifest();
        build.yandex = Some(YandexInfo {
            enabled: true,
            all_files: true,
            public_url: "https://disk.yandex.ru/d/DxjZXLCwnEimiw".into(),
            path: "/1.60.1.70938".into(),
        });
        let url = yandex_url_for("Data/data/data.000", Some(&build)).expect("yandex url");
        assert!(url.starts_with("https://cloud-api.yandex.net/v1/disk/public/resources/download?"));
        assert!(url.contains("DxjZXLCwnEimiw"));
        assert!(url.contains("1.60.1.70938"));
        assert!(yandex_url_for("_forever_/wow_loader.dll", Some(&build)).is_some());
        assert!(yandex_url_for(".build.info", Some(&build)).is_some());
        build.yandex.as_mut().unwrap().enabled = false;
        assert!(yandex_url_for("Data/data/data.000", Some(&build)).is_none());
    }

    #[test]
    fn yandex_heavy_files_do_not_fall_back_to_server() {
        let mut build = build_manifest();
        build.yandex = Some(YandexInfo {
            enabled: true,
            all_files: true,
            public_url: "https://disk.yandex.ru/d/DxjZXLCwnEimiw".into(),
            path: "/1.60.1.70938".into(),
        });
        let heavy = entry_for(&manifest_file("Data/data/data.000", 1, ""), Some(&build));
        assert!(heavy.yandex_url.is_some());
        assert!(!heavy.allow_server_fallback);
        assert!(heavy.cdn_urls.is_empty());
        let overlay = entry_for(&manifest_file("_forever_/wow_loader.dll", 1, ""), Some(&build));
        assert!(overlay.yandex_url.is_some());
        assert!(!overlay.allow_server_fallback);
        assert!(overlay.cdn_urls.is_empty());
        for path in [".build.info", "Data/data/0000000008.idx", "Data/indices/abcdef.index", "_forever_/Wow.exe"] {
            let entry = entry_for(&manifest_file(path, 1, ""), Some(&build));
            assert!(entry.yandex_url.is_some(), "{path}");
            assert!(!entry.allow_server_fallback, "{path}");
            assert!(entry.cdn_urls.is_empty(), "{path}");
        }
        build.yandex.as_mut().unwrap().public_url.clear();
        let entry = entry_for(&manifest_file("Data/data/data.000", 1, ""), Some(&build));
        assert!(entry.yandex_url.is_none());
        assert!(!entry.allow_server_fallback);
        assert!(entry.cdn_urls.is_empty());
    }

    #[test]
    fn legacy_yandex_config_keeps_live_overlay_until_explicit_switch() {
        let build: BuildManifest = serde_json::from_str(r#"{
            "build":"1.60.1.70938",
            "yandex":{"enabled":true,"public_url":"https://disk.yandex.ru/d/test","path":"/1.60.1.70938"},
            "overlay":["Data/data/0000000008.idx"]
        }"#).unwrap();
        for path in ["_forever_/Wow.exe", ".build.info", "Data/data/0000000008.idx"] {
            let entry = entry_for(&manifest_file(path, 1, ""), Some(&build));
            assert!(entry.yandex_url.is_none());
            assert!(entry.allow_server_fallback);
        }
        let archive = entry_for(&manifest_file("Data/data/data.000", 1, ""), Some(&build));
        assert!(archive.yandex_url.is_some());
        assert!(!archive.allow_server_fallback);
    }
}
