//! Докачка самообновления: постоянный .part, строгий HTTP Range и SHA-256.
use super::LauncherManifest;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub retries: u32,
    pub delay: Duration,
    pub idle: Duration,
}

pub(super) const POLICY: Policy = Policy {
    retries: 50,
    delay: Duration::from_secs(5),
    idle: Duration::from_secs(120),
};

/// Хеш в имени не даёт смешать разные версии. Путь не зависит от PID и
/// сохраняется после перезапуска. На Linux файл остаётся на том же томе.
pub(super) fn partial_path(current: &Path, sha256: &str) -> PathBuf {
    let name = current.file_name().unwrap_or_default().to_string_lossy();
    current.with_file_name(format!(".{name}.update-{}.part", sha256.to_ascii_lowercase()))
}

async fn hash_matches(path: &Path, expected: &str) -> Result<bool, String> {
    let path = path.to_owned();
    let expected = expected.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 { break; }
            hash.update(&buffer[..n]);
        }
        Ok(hex::encode(hash.finalize()).eq_ignore_ascii_case(&expected))
    }).await.map_err(|e| e.to_string())?
}

fn percent(downloaded: u64, size: u64) -> f64 {
    if size == 0 { 0.0 } else { (downloaded as f64 / size as f64 * 100.0).min(99.9) }
}

async fn file_len(path: &Path) -> Result<u64, String> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() => Ok(meta.len()),
        Ok(_) => Err(format!("Вместо файла обновления обнаружен каталог: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("Не удалось проверить файл обновления: {e}")),
    }
}

async fn reset(path: &Path) -> Result<(), String> {
    tokio::fs::File::create(path).await.map_err(|e| format!("Не удалось сбросить неполное обновление: {e}"))?;
    Ok(())
}

/// Неправильный диапазон не дописывается к уже полученному префиксу.
fn valid_range(response: &reqwest::Response, offset: u64, size: u64) -> bool {
    let expected = format!("bytes {offset}-{}/{}", size - 1, size);
    response.headers().get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok()) == Some(expected.as_str())
}

async fn attempt<F: Fn(&str, f64, String)>(
    client: &reqwest::Client,
    manifest: &LauncherManifest,
    path: &Path,
    policy: Policy,
    event: &F,
) -> Result<(), String> {
    let mut offset = file_len(path).await?;
    if offset > manifest.size {
        reset(path).await?;
        offset = 0;
    }
    if offset == manifest.size {
        event("verify", 99.9, "Проверяем SHA-256 сохранённого обновления…".into());
        if hash_matches(path, &manifest.sha256).await? { return Ok(()); }
        reset(path).await?;
        return Err("SHA-256 сохранённого обновления не совпадает; повторяем с начала файла".into());
    }
    event("progress", percent(offset, manifest.size), format!(
        "Загрузка лаунчера {}: {:.1}/{:.1} МиБ{}",
        manifest.version, offset as f64 / 1048576.0, manifest.size as f64 / 1048576.0,
        if offset > 0 { " · докачка" } else { "" }
    ));
    let mut request = client.get(&manifest.download_url).header("Accept-Encoding", "identity");
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }
    // Только ожидание ответа ограничено, полного лимита времени загрузки нет.
    let response = tokio::time::timeout(policy.idle, request.send()).await
        .map_err(|_| "Таймаут ожидания ответа сервера обновлений".to_string())?
        .map_err(|e| format!("Не удалось скачать обновление: {e}"))?;
    if let Some(encoding) = response.headers().get(reqwest::header::CONTENT_ENCODING) {
        if encoding != "identity" { return Err("Неожиданное сжатие ответа сервера обновлений".into()); }
    }
    match response.status() {
        reqwest::StatusCode::PARTIAL_CONTENT => {
            if !valid_range(&response, offset, manifest.size) {
                return Err("Сервер вернул неправильный Content-Range; сохранённая часть не изменена".into());
            }
        }
        reqwest::StatusCode::OK => {
            if offset > 0 {
                event("log", 0.0, "Сервер не поддержал докачку (HTTP 200); загружаем файл заново, без склеивания".into());
            }
            offset = 0;
        }
        reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
            reset(path).await?;
            return Err("Сервер отклонил диапазон (HTTP 416); повторяем с начала файла".into());
        }
        status => return Err(format!("Сервер обновлений вернул HTTP {status}")),
    }
    if response.content_length().map(|n| n != manifest.size - offset).unwrap_or(false) {
        return Err("Размер ответа не соответствует ожидаемой части обновления".into());
    }
    let mut file = tokio::fs::OpenOptions::new().create(true).write(true)
        .append(offset > 0).truncate(offset == 0).open(path).await
        .map_err(|e| format!("Не удалось открыть временный файл: {e}"))?;
    let mut stream = response.bytes_stream();
    let mut downloaded = offset;
    let mut last_event = Instant::now();
    // Закрываем файл до возврата ошибки: следующий повтор увидит реальную длину.
    let transfer = async {
        loop {
            let next = tokio::time::timeout(policy.idle, stream.next()).await
                .map_err(|_| "Нет данных от сервера обновлений: таймаут простоя".to_string())?;
            let Some(chunk) = next else { break; };
            let chunk = chunk.map_err(|e| format!("Обрыв загрузки обновления: {e}"))?;
            if chunk.len() as u64 > manifest.size - downloaded {
                return Err("Ответ сервера превысил ожидаемый размер обновления".into());
            }
            file.write_all(&chunk).await.map_err(|e| format!("Не удалось записать обновление: {e}"))?;
            downloaded += chunk.len() as u64;
            if last_event.elapsed() >= Duration::from_millis(200) {
                last_event = Instant::now();
                event("progress", percent(downloaded, manifest.size), format!(
                    "Обновление лаунчера: {:.1}% · {:.1}/{:.1} МиБ", percent(downloaded, manifest.size),
                    downloaded as f64 / 1048576.0, manifest.size as f64 / 1048576.0
                ));
            }
        }
        Ok::<(), String>(())
    }.await;
    let flushed = file.flush().await;
    drop(file);
    transfer?;
    flushed.map_err(|e| format!("Не удалось сохранить обновление: {e}"))?;
    if downloaded != manifest.size {
        return Err(format!("Обновление скачано частично: {downloaded}/{} байт", manifest.size));
    }
    event("verify", 99.9, "Загрузка завершена, проверяем SHA-256…".into());
    if !hash_matches(path, &manifest.sha256).await? {
        reset(path).await?;
        return Err("SHA-256 обновления не совпадает; повторяем с начала файла".into());
    }
    Ok(())
}

pub(super) async fn download<F: Fn(&str, f64, String)>(
    client: &reqwest::Client,
    manifest: &LauncherManifest,
    path: &Path,
    policy: Policy,
    event: F,
) -> Result<(), String> {
    if manifest.size == 0 || manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Некорректный размер или SHA-256 в манифесте лаунчера".into());
    }
    for retry in 0..=policy.retries {
        match attempt(client, manifest, path, policy, &event).await {
            Ok(()) => return Ok(()),
            Err(error) if retry == policy.retries => {
                return Err(format!("Не удалось обновить лаунчер после {} попыток: {error}. Частичная загрузка сохранена в {}", retry + 1, path.display()));
            }
            Err(error) => {
                let downloaded = file_len(path).await.unwrap_or(0);
                event("retry", percent(downloaded, manifest.size), format!(
                    "{error} · повтор {}/{} через {} с · сохранено {:.1} МиБ",
                    retry + 1, policy.retries, policy.delay.as_secs(), downloaded as f64 / 1048576.0
                ));
                tokio::time::sleep(policy.delay).await;
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const BODY: &[u8] = b"0123456789abcdef";
    const FAST: Policy = Policy { retries: 2, delay: Duration::ZERO, idle: Duration::from_secs(3) };

    fn manifest(url: String) -> LauncherManifest {
        LauncherManifest { version: "test".into(), download_url: url, size: BODY.len() as u64,
            sha256: hex::encode(Sha256::digest(BODY)), changelog: String::new() }
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("forever-self-update-{}-{stamp}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn part(&self) -> PathBuf { self.0.join("update.part") }
    }
    impl Drop for Temp {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        [format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", BODY.len()).into_bytes(), body.to_vec()].concat()
    }
    fn partial(start: usize) -> Vec<u8> {
        [format!("HTTP/1.1 206 Partial Content\r\nConnection: close\r\nContent-Range: bytes {start}-15/16\r\nContent-Length: {}\r\n\r\n", 16-start).into_bytes(), BODY[start..].to_vec()].concat()
    }
    fn server(replies: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let thread = std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                captured.lock().unwrap().push(String::from_utf8(request).unwrap().to_ascii_lowercase());
                stream.write_all(&reply).unwrap();
            }
        });
        (format!("http://{address}/launcher"), requests, thread)
    }

    #[tokio::test]
    async fn disconnect_is_resumed_from_saved_offset() {
        let temp = Temp::new();
        let (url, requests, thread) = server(vec![ok(&BODY[..5]), partial(5)]);
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(!requests[0].contains("range:"));
        assert!(requests[1].contains("range: bytes=5-"));
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn persisted_partial_survives_new_download_call() {
        let temp = Temp::new();
        let (url, _, thread) = server(vec![ok(&BODY[..7])]);
        assert!(download(&reqwest::Client::new(), &manifest(url), &temp.part(), Policy { retries: 0, ..FAST }, |_,_,_|{}).await.is_err());
        thread.join().unwrap();
        assert_eq!(std::fs::read(temp.part()).unwrap(), &BODY[..7]);
        let (url, requests, thread) = server(vec![partial(7)]);
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        assert!(requests.lock().unwrap()[0].contains("range: bytes=7-"));
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn ignored_range_restarts_without_concatenating() {
        let temp = Temp::new();
        std::fs::write(temp.part(), &BODY[..3]).unwrap();
        let (url, requests, thread) = server(vec![ok(BODY)]);
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        assert!(requests.lock().unwrap()[0].contains("range: bytes=3-"));
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn wrong_range_preserves_prefix_and_retries() {
        let temp = Temp::new();
        std::fs::write(temp.part(), &BODY[..3]).unwrap();
        let (url, requests, thread) = server(vec![partial(2), partial(3)]);
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        for request in requests.lock().unwrap().iter() { assert!(request.contains("range: bytes=3-")); }
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn corrupt_prefix_is_detected_by_final_sha_and_restarted() {
        let temp = Temp::new();
        std::fs::write(temp.part(), b"bad").unwrap();
        let (url, requests, thread) = server(vec![partial(3), ok(BODY)]);
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        assert!(!requests.lock().unwrap()[1].contains("range:"));
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn repeated_http_errors_keep_partial_and_obey_retry_budget() {
        let temp = Temp::new();
        std::fs::write(temp.part(), &BODY[..3]).unwrap();
        let error = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, requests, thread) = server(vec![error; 3]);
        let retries = std::cell::Cell::new(0);
        assert!(download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |kind,_,_|{
            if kind == "retry" { retries.set(retries.get()+1); }
        }).await.is_err());
        thread.join().unwrap();
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert_eq!(retries.get(), 2);
        assert_eq!(std::fs::read(temp.part()).unwrap(), &BODY[..3]);
    }

    #[tokio::test]
    async fn slow_active_transfer_can_outlive_idle_timeout() {
        let temp = Temp::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/slow", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n").unwrap();
            for byte in BODY {
                stream.write_all(&[*byte]).unwrap();
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let started = Instant::now();
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), Policy { idle: Duration::from_millis(200), ..FAST }, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        assert!(started.elapsed() > Duration::from_millis(400));
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn rejected_range_resets_instead_of_appending() {
        let temp = Temp::new();
        std::fs::write(temp.part(), &BODY[..3]).unwrap();
        let error = b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, requests, thread) = server(vec![error, ok(BODY)]);
        download(&reqwest::Client::new(), &manifest(url), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
        thread.join().unwrap();
        assert!(!requests.lock().unwrap()[1].contains("range:"));
        assert_eq!(std::fs::read(temp.part()).unwrap(), BODY);
    }

    #[tokio::test]
    async fn complete_verified_file_does_not_need_network() {
        let temp = Temp::new();
        std::fs::write(temp.part(), BODY).unwrap();
        download(&reqwest::Client::new(), &manifest("http://127.0.0.1:1/".into()), &temp.part(), FAST, |_,_,_|{}).await.unwrap();
    }

    #[test]
    fn partial_path_is_persistent_and_bound_to_artifact() {
        let current = Path::new("games/Forever.AppImage");
        assert_eq!(partial_path(current, "ABCD"), partial_path(current, "abcd"));
        assert_ne!(partial_path(current, "abcd"), partial_path(current, "ffff"));
        assert_eq!(partial_path(current, "abcd").parent(), current.parent());
    }
}
