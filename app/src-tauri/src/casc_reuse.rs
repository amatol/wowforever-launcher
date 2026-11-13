//! Переиспользование архивов по содержимому, без доверия локальным CASC-индексам.
//! Формат gear-v1: min=64 KiB, mask=18 бит, max=1 MiB, сброс hash на границе.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MIN: usize = 64 * 1024;
pub const MAX: usize = 1024 * 1024;
const MASK: u64 = (1 << 18) - 1;
const fn gear() -> [u64; 256] {
    let mut values = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut v = (i as u64).wrapping_add(0x9e3779b97f4a7c15);
        v = (v ^ (v >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        v = (v ^ (v >> 27)).wrapping_mul(0x94d049bb133111eb);
        values[i] = v ^ (v >> 31);
        i += 1;
    }
    values
}
const GEAR: [u64; 256] = gear();

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Chunk {
    pub size: u32,
    pub sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Archive {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub chunks: Vec<Chunk>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Recipe {
    pub format: String,
    pub data_revision: String,
    pub files: Vec<Archive>,
}
#[derive(Clone, Debug)]
pub struct LocalChunk {
    pub path: PathBuf,
    pub offset: u64,
    pub size: u32,
}
pub type ChunkIndex = HashMap<[u8; 32], LocalChunk>;
#[derive(Clone, Debug)]
pub struct MissingRange {
    pub offset: u64,
    pub size: u64,
    pub chunks: Vec<Chunk>,
}

pub fn archive_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 3 && parts[0] == "Data" && ["data", "ecache", "wowf"].contains(&parts[1])
        && parts[2].strip_prefix("data.").map(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())).unwrap_or(false)
}

fn digest(value: &str) -> Result<[u8; 32], String> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(value, &mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// Потоковое чтение: память ограничена размером блока, а не архива.
pub fn chunks(path: &Path, mut visit: impl FnMut(u64, &[u8]) -> Result<(), String>) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    let mut input = [0u8; 64 * 1024];
    let mut chunk = Vec::with_capacity(MAX);
    let mut hash = 0u64;
    let mut offset = 0;
    loop {
        let n = file.read(&mut input).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        for &byte in &input[..n] {
            chunk.push(byte);
            hash = hash.wrapping_shl(1).wrapping_add(GEAR[byte as usize]);
            if chunk.len() >= MAX || (chunk.len() >= MIN && hash & MASK == 0) {
                visit(offset, &chunk)?;
                offset += chunk.len() as u64;
                chunk.clear();
                hash = 0;
            }
        }
    }
    if !chunk.is_empty() { visit(offset, &chunk)?; }
    let after = file.metadata().map_err(|e| e.to_string())?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(format!("{} изменён во время чтения. Закройте игру.", path.display()));
    }
    Ok(())
}

pub fn describe(root: &Path, path: &str) -> Result<Archive, String> {
    if !archive_path(path) { return Err("Недопустимый путь архива".into()); }
    let mut result = Archive { path: path.into(), size: 0, sha256: String::new(), chunks: Vec::new() };
    let mut sha = Sha256::new();
    chunks(&root.join(path), |_, data| {
        sha.update(data);
        result.size += data.len() as u64;
        result.chunks.push(Chunk { size: data.len() as u32, sha256: hex::encode(Sha256::digest(data)) });
        Ok(())
    })?;
    result.sha256 = hex::encode(sha.finalize());
    Ok(result)
}

pub fn validate(recipe: &Recipe, revision: &str) -> Result<(), String> {
    if recipe.format != "gear-v1" || recipe.data_revision != revision { return Err("Несовместимый рецепт обновления".into()); }
    let mut paths = HashSet::new();
    for file in &recipe.files {
        if !archive_path(&file.path) || !paths.insert(&file.path) { return Err("Некорректный список архивов".into()); }
        digest(&file.sha256)?;
        let mut size = 0u64;
        for chunk in &file.chunks {
            if chunk.size == 0 || chunk.size as usize > MAX { return Err("Недопустимый размер блока".into()); }
            digest(&chunk.sha256)?;
            size = size.checked_add(chunk.size as u64).ok_or("Переполнение размера")?;
        }
        if size != file.size { return Err("Рецепт не покрывает весь архив".into()); }
    }
    Ok(())
}

/// Читаем только архивы Data, не настройки и другие пользовательские файлы.
pub fn scan(root: &Path, recipe: &Recipe, mut progress: impl FnMut(&str, u64)) -> Result<ChunkIndex, String> {
    let mut index = HashMap::new();
    let mut directories = HashSet::new();
    for file in &recipe.files {
        if !archive_path(&file.path) { return Err("Недопустимый путь архива".into()); }
        directories.insert(Path::new(&file.path).parent().unwrap().to_path_buf());
    }
    let mut read = 0;
    for directory in directories {
        let entries = match fs::read_dir(root.join(&directory)) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        };
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let relative = directory.join(entry.file_name()).to_string_lossy().replace('\\', "/");
            if !archive_path(&relative) || !entry.file_type().map_err(|e| e.to_string())?.is_file() { continue; }
            let path = entry.path();
            progress(&relative, read);
            chunks(&path, |offset, data| {
                index.entry(Sha256::digest(data).into()).or_insert_with(|| LocalChunk { path: path.clone(), offset, size: data.len() as u32 });
                read += data.len() as u64;
                Ok(())
            })?;
        }
    }
    progress("", read);
    Ok(index)
}

/// Временный файл создаётся отдельно. Каждая повторно используемая порция
/// проверяется повторно: игра/диск могли изменить её после первоначального чтения.
pub fn assemble(root: &Path, archive: &Archive, index: &ChunkIndex) -> Result<(PathBuf, Vec<MissingRange>, u64), String> {
    if !archive_path(&archive.path) { return Err("Недопустимый путь архива".into()); }
    let target = root.join(&archive.path);
    fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
    let temporary = target.with_extension("reuse-part");
    let mut output = File::create(&temporary).map_err(|e| e.to_string())?;
    output.set_len(archive.size).map_err(|e| e.to_string())?;
    let mut source: Option<(PathBuf, File)> = None;
    let mut buffer = vec![0; MAX];
    let mut offset = 0;
    let mut reused = 0;
    let mut ranges: Vec<MissingRange> = Vec::new();
    for chunk in &archive.chunks {
        let key = digest(&chunk.sha256)?;
        let mut found = false;
        if let Some(local) = index.get(&key).filter(|l| l.size == chunk.size) {
            if source.as_ref().map(|s| &s.0) != Some(&local.path) {
                source = File::open(&local.path).ok().map(|f| (local.path.clone(), f));
            }
            if let Some((_, input)) = source.as_mut() {
                let data = &mut buffer[..chunk.size as usize];
                found = input.seek(SeekFrom::Start(local.offset)).and_then(|_| input.read_exact(data)).is_ok()
                    && Sha256::digest(&*data).as_slice() == key;
                if found {
                    output.seek(SeekFrom::Start(offset)).and_then(|_| output.write_all(data)).map_err(|e| e.to_string())?;
                    reused += chunk.size as u64;
                }
            }
        }
        if !found {
            if let Some(last) = ranges.last_mut().filter(|r| r.offset + r.size == offset && r.size + chunk.size as u64 <= 8 * MAX as u64) {
                last.size += chunk.size as u64;
                last.chunks.push(chunk.clone());
            } else {
                ranges.push(MissingRange { offset, size: chunk.size as u64, chunks: vec![chunk.clone()] });
            }
        }
        offset += chunk.size as u64;
    }
    output.sync_all().map_err(|e| e.to_string())?;
    Ok((temporary, ranges, reused))
}

pub fn verify_range(data: &[u8], range: &MissingRange) -> Result<(), String> {
    if data.len() as u64 != range.size { return Err("Неполный ответ диапазона".into()); }
    let mut offset = 0;
    for chunk in &range.chunks {
        let end = offset + chunk.size as usize;
        if end > data.len() || Sha256::digest(&data[offset..end]).as_slice() != digest(&chunk.sha256)? {
            return Err("SHA-256 загруженного блока не совпадает".into());
        }
        offset = end;
    }
    if offset != data.len() { return Err("Лишние данные в диапазоне".into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn folder() -> PathBuf {
        let p = std::env::temp_dir().join(format!("reuse-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(p.join("Data/data")).unwrap(); p
    }
    fn bytes(size: usize) -> Vec<u8> {
        let mut state = 17u64;
        (0..size).map(|_| { state ^= state << 13; state ^= state >> 7; state ^= state << 17; state as u8 }).collect()
    }
    #[test]
    fn shifted_archive_is_reused_without_indices() {
        let old = folder(); let new = folder(); let stage = folder();
        let data = bytes(5 * MAX);
        fs::write(old.join("Data/data/data.007"), &data).unwrap();
        let mut changed = b"new prefix shifts every offset".to_vec(); changed.extend_from_slice(&data);
        fs::write(new.join("Data/data/data.000"), &changed).unwrap();
        let archive = describe(&new, "Data/data/data.000").unwrap();
        let recipe = Recipe { format: "gear-v1".into(), data_revision: "r".into(), files: vec![archive.clone()] };
        validate(&recipe, "r").unwrap();
        let index = scan(&old, &recipe, |_, _| {}).unwrap();
        let (temp, ranges, reused) = assemble(&stage, &archive, &index).unwrap();
        assert!(reused > 4 * MAX as u64);
        let mut file = fs::OpenOptions::new().write(true).open(&temp).unwrap();
        for range in ranges {
            let part = &changed[range.offset as usize..(range.offset + range.size) as usize];
            verify_range(part, &range).unwrap();
            let mut corrupt = part.to_vec(); corrupt[0] ^= 1; assert!(verify_range(&corrupt, &range).is_err());
            file.seek(SeekFrom::Start(range.offset)).unwrap();file.write_all(part).unwrap();
        }
        assert_eq!(fs::read(temp).unwrap(), changed);
        assert_eq!(fs::read(old.join("Data/data/data.007")).unwrap(), data);
        for p in [old,new,stage] { fs::remove_dir_all(p).unwrap(); }
    }
    #[test]
    fn changed_source_is_never_copied_unchecked() {
        let old = folder(); let stage = folder();
        fs::write(old.join("Data/data/data.000"), bytes(MAX)).unwrap();
        let archive = describe(&old, "Data/data/data.000").unwrap();
        let mut recipe = Recipe { format: "gear-v1".into(), data_revision: "r".into(), files: vec![archive.clone()] };
        let index = scan(&old, &recipe, |_, _| {}).unwrap();
        fs::write(old.join("Data/data/data.000"), vec![0; MAX]).unwrap();
        let (_, ranges, reused) = assemble(&stage, &archive, &index).unwrap();
        assert_eq!(reused, 0); assert_eq!(ranges.iter().map(|r|r.size).sum::<u64>(), archive.size);
        recipe.files[0].path = "../escape".into();assert!(validate(&recipe,"r").is_err());
        recipe.files[0] = archive; recipe.files[0].size += 1; assert!(validate(&recipe,"r").is_err());
        for p in [old,stage] { fs::remove_dir_all(p).unwrap(); }
    }
}
