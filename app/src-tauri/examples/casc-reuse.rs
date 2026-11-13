// Офлайн-генератор и аудит: тот же алгоритм, что используется лаунчером.
#[path = "../src/casc_reuse.rs"]
mod casc_reuse;
use casc_reuse::*;
use std::{fs, path::Path};
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 { return Err("generate <snapshot> <manifest.json> <recipe.json> | audit <client> <recipe.json> <report.json> | verify <client> <recipe.json> <new-stage-dir>".into()); }
    if args[1] == "generate" {
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(&args[3]).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        let mut recipe = Recipe { format: "gear-v1".into(), data_revision: manifest["data_revision"].as_str().ok_or("Нет ревизии")?.into(), files: Vec::new() };
        for f in manifest["files"].as_array().ok_or("Нет файлов")? {
            let path = f["path"].as_str().ok_or("Нет пути")?;
            if !archive_path(path) { continue; }
            let archive = describe(Path::new(&args[2]), path)?;
            if archive.sha256 != f["sha256"].as_str().unwrap_or("") || Some(archive.size) != f["size"].as_u64() { return Err(format!("Снимок изменился: {path}")); }
            println!("{} {} chunks",path,archive.chunks.len());
            recipe.files.push(archive);
        }
        validate(&recipe, &recipe.data_revision)?;
        fs::write(&args[4],serde_json::to_vec(&recipe).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
    } else if args[1] == "audit" {
        let recipe: Recipe = serde_json::from_slice(&fs::read(&args[3]).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        validate(&recipe,&recipe.data_revision)?;
        let index = scan(Path::new(&args[2]), &recipe, |p,b| println!("scan {p} {b}"))?;
        let mut total = 0u64; let mut reused = 0u64;
        for archive in &recipe.files {
            for chunk in &archive.chunks {
                total += chunk.size as u64;
                let key: [u8;32] = hex::decode(&chunk.sha256).unwrap().try_into().unwrap();
                if index.get(&key).map(|c|c.size) == Some(chunk.size) { reused += chunk.size as u64; }
            }
        }
        let report = serde_json::json!({"archive_bytes":total,"reusable_bytes":reused,"download_bytes":total-reused,"unique_local_chunks":index.len()});
        println!("{report}");fs::write(&args[4],report.to_string()).map_err(|e|e.to_string())?;
    } else if args[1] == "verify" {
        // Последовательное восстановление всех архивов с удалением только
        // собственных уже проверенных временных файлов: не нужно 56 ГБ места.
        let recipe: Recipe = serde_json::from_slice(&fs::read(&args[3]).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        validate(&recipe,&recipe.data_revision)?;
        if !recipe.data_revision.bytes().all(|c|c.is_ascii_alphanumeric() || c == b'-' || c == b'_') { return Err("Неверная ревизия".into()); }
        fs::create_dir(&args[4]).map_err(|e|e.to_string())?;
        let index = scan(Path::new(&args[2]), &recipe, |p,b| println!("scan {p} {b}"))?;
        let runtime = tokio::runtime::Runtime::new().map_err(|e|e.to_string())?;
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(120)).build().map_err(|e|e.to_string())?;
        let mut reused = 0u64; let mut downloaded = 0u64;
        for archive in &recipe.files {
            let (temporary,ranges,copied) = assemble(Path::new(&args[4]),archive,&index)?;
            reused += copied;
            for range in ranges {
                let url = format!("https://forever.mmolabs.ru/launcher/releases/{}/files/{}",recipe.data_revision,archive.path);
                let end = range.offset + range.size - 1;
                let data = runtime.block_on(async {
                    let r = client.get(url).header("Accept-Encoding","identity").header("Range",format!("bytes={}-{end}",range.offset)).send().await.map_err(|e|e.to_string())?;
                    let expected = format!("bytes {}-{end}/{}",range.offset,archive.size);
                    if r.status() != 206 || r.headers().get("Content-Range").and_then(|h|h.to_str().ok()) != Some(expected.as_str()) { return Err("Неверный Content-Range".to_string()); }
                    r.bytes().await.map(|b|b.to_vec()).map_err(|e|e.to_string())
                })?;
                verify_range(&data,&range)?;
                use std::io::{Seek,SeekFrom,Write};
                let mut f = fs::OpenOptions::new().write(true).open(&temporary).map_err(|e|e.to_string())?;
                f.seek(SeekFrom::Start(range.offset)).and_then(|_|f.write_all(&data)).map_err(|e|e.to_string())?;
                downloaded += data.len() as u64;
            }
            use sha2::{Digest,Sha256}; use std::io::Read;
            let mut f = fs::File::open(&temporary).map_err(|e|e.to_string())?; let mut hash = Sha256::new(); let mut buf = vec![0;MAX];
            loop { let n=f.read(&mut buf).map_err(|e|e.to_string())?; if n==0 {break;} hash.update(&buf[..n]); }
            if hex::encode(hash.finalize()) != archive.sha256 { return Err(format!("Итоговый хеш не совпал: {}",archive.path)); }
            drop(f); fs::remove_file(temporary).map_err(|e|e.to_string())?;
            println!("VERIFIED {} reused_total={reused} downloaded_total={downloaded}",archive.path);
        }
        let report = serde_json::json!({"archives_verified":recipe.files.len(),"reused_bytes":reused,"downloaded_bytes":downloaded,"sha256_errors":0});
        println!("{report}");fs::write(Path::new(&args[4]).join("report.json"),report.to_string()).map_err(|e|e.to_string())?;
    } else { return Err("Неизвестная команда".into()); }
    Ok(())
}
