use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use sha2::{Digest, Sha256};

pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_BATCH_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_FILES: usize = 128;
pub const MAX_DROP_DIR_BYTES: u64 = 1024 * 1024 * 1024;
pub const DROP_TTL: Duration = Duration::from_secs(24 * 3600);

pub fn drop_dir() -> PathBuf {
    crate::config::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zmux")
        .join("drop")
}

pub fn ensure_drop_dir() -> io::Result<PathBuf> {
    let dir = drop_dir();
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn sanitize_ext(name: &str) -> String {
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("bin");
    let cleaned: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned.to_ascii_lowercase()
    }
}

pub fn display_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    base.chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\' && *c != '\0')
        .take(255)
        .collect()
}

pub struct PartFile {
    pub id: String,
    pub final_path: PathBuf,
    part_path: PathBuf,
    file: File,
    hasher: Sha256,
    pub written: u64,
}

impl PartFile {
    pub fn create(ext: &str) -> io::Result<Self> {
        let dir = ensure_drop_dir()?;
        gc_expired(&dir)?;
        let id = crate::domain::ids::new_instance_id();
        let ext = sanitize_ext(ext);
        let final_path = dir.join(format!("{id}.{ext}"));
        let part_path = dir.join(format!("{id}.{ext}.part"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(
                &part_path,
                fs::Permissions::from_mode(0o600),
            );
        }
        Ok(Self {
            id,
            final_path,
            part_path,
            file,
            hasher: Sha256::new(),
            written: 0,
        })
    }

    pub fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        if self.written.saturating_add(data.len() as u64) > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file exceeds 64MiB limit",
            ));
        }
        self.file.write_all(data)?;
        self.hasher.update(data);
        self.written += data.len() as u64;
        Ok(())
    }

    pub fn finish(mut self, expected_sha: Option<&str>) -> io::Result<PathBuf> {
        self.file.flush()?;
        drop(self.file);
        let digest = format!("{:x}", self.hasher.finalize());
        if let Some(expected) = expected_sha {
            if !expected.is_empty() && expected != digest {
                let _ = fs::remove_file(&self.part_path);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sha256 mismatch",
                ));
            }
        }
        fs::rename(&self.part_path, &self.final_path)?;
        Ok(self.final_path)
    }

    pub fn cancel(self) {
        let _ = fs::remove_file(&self.part_path);
    }
}

pub fn gc_expired(dir: &Path) -> io::Result<()> {
    let now = SystemTime::now();
    let mut total = 0u64;
    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() || meta.file_type().is_symlink() {
            let _ = fs::remove_file(&path);
            continue;
        }
        let mtime = meta.modified().unwrap_or(now);
        let len = meta.len();
        if now.duration_since(mtime).unwrap_or_default() > DROP_TTL {
            let _ = fs::remove_file(&path);
            continue;
        }
        total += len;
        entries.push((path, len, mtime));
    }
    if total <= MAX_DROP_DIR_BYTES {
        return Ok(());
    }
    entries.sort_by_key(|e| e.2);
    for (path, len, _) in entries {
        if total <= MAX_DROP_DIR_BYTES {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
    Ok(())
}

pub fn write_bytes_atomic(ext: &str, data: &[u8]) -> io::Result<PathBuf> {
    let mut part = PartFile::create(ext)?;
    part.write_chunk(data)?;
    part.finish(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_path_traversal_from_names() {
        assert_eq!(display_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_ext("foo.png"), "png");
        assert_eq!(sanitize_ext("foo.Png"), "png");
        assert_eq!(sanitize_ext("noext"), "bin");
        assert_eq!(sanitize_ext("../x.sh"), "sh");
    }

    #[test]
    fn rejects_oversize_chunk_sequence() {
        let mut part = PartFile::create("bin").unwrap();
        let chunk = vec![0u8; 1024];
        let mut wrote = 0u64;
        let mut hit_limit = false;
        while wrote <= MAX_FILE_BYTES {
            match part.write_chunk(&chunk) {
                Ok(()) => wrote += chunk.len() as u64,
                Err(_) => {
                    hit_limit = true;
                    break;
                }
            }
        }
        part.cancel();
        assert!(hit_limit);
    }

    #[test]
    fn finish_rejects_sha256_mismatch() {
        let mut part = PartFile::create("bin").unwrap();
        part.write_chunk(b"hello").unwrap();
        let err = part.finish(Some("deadbeef")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
