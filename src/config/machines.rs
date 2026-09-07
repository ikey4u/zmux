//! Display names keyed by stable machine identity, separate from SSH routes.
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct MachineNames {
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

pub fn config_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("ZMUX_CONFIG") {
        let path = PathBuf::from(path);
        return Ok(path
            .parent()
            .unwrap_or(Path::new("."))
            .join("machines.json"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| super::home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "configuration directory unavailable",
            )
        })?;
    Ok(base.join("zmux").join("machines.json"))
}

impl MachineNames {
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn rename(
        &mut self,
        path: &Path,
        id: &str,
        name: &str,
    ) -> io::Result<()> {
        self.save_name(path, name, |updated, name| {
            updated.names.insert(id.to_string(), name.to_string());
        })
    }

    pub fn workspace_name(
        &self,
        machine: &str,
        workspace: &str,
    ) -> Option<&str> {
        self.workspaces
            .get(machine)?
            .get(workspace)
            .map(String::as_str)
    }

    pub fn rename_workspace(
        &mut self,
        path: &Path,
        machine: &str,
        workspace: &str,
        name: &str,
    ) -> io::Result<()> {
        self.save_name(path, name, |updated, name| {
            updated
                .workspaces
                .entry(machine.to_string())
                .or_default()
                .insert(workspace.to_string(), name.to_string());
        })
    }

    fn save_name(
        &mut self,
        path: &Path,
        name: &str,
        update: impl FnOnce(&mut Self, &str),
    ) -> io::Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "name must be nonempty and contain no control characters",
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Serialize read/merge/replace so two clients renaming different
        // machines do not discard each other's configuration.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        let mut updated = Self::load(path)?;
        update(&mut updated, name);
        let data = serde_json::to_vec_pretty(&updated)?;
        let temporary =
            path.with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        *self = updated;
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestConfig(PathBuf);
    impl TestConfig {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "zmux-names-{}-{unique}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root.join("machines.json"))
        }
    }
    impl Drop for TestConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn concurrent_clients_merge_names_and_preserve_unknown_fields() {
        let config = TestConfig::new();
        std::fs::write(&config.0, r#"{"version":7,"names":{}}"#).unwrap();
        std::thread::scope(|scope| {
            for id in ["local", "ssh:8#loopback", "ssh:11#unavailable"] {
                let path = &config.0;
                scope.spawn(move || {
                    // Independent clients can even choose the same label.
                    MachineNames::default()
                        .rename(path, id, " 开发机器 ")
                        .unwrap();
                });
            }
        });
        let saved = MachineNames::load(&config.0).unwrap();
        assert_eq!(saved.names.len(), 3);
        assert!(saved.names.values().all(|name| name == "开发机器"));
        assert_eq!(saved.extra["version"], 7);
        assert_eq!(
            std::fs::read_dir(config.0.parent().unwrap())
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn workspace_names_are_persistent_and_scoped_by_machine_and_socket() {
        let config = TestConfig::new();
        let mut names = MachineNames::default();
        names.rename(&config.0, "local", "Laptop").unwrap();
        names
            .rename_workspace(&config.0, "local", "default", "Editor")
            .unwrap();
        let mut other = MachineNames::default();
        other
            .rename_workspace(&config.0, "remote", "default", "Deploy")
            .unwrap();
        names
            .rename_workspace(&config.0, "local", "second", "Shells")
            .unwrap();
        let restored = MachineNames::load(&config.0).unwrap();
        assert_eq!(restored.names["local"], "Laptop");
        assert_eq!(restored.workspace_name("local", "default"), Some("Editor"));
        assert_eq!(
            restored.workspace_name("remote", "default"),
            Some("Deploy")
        );
        assert_eq!(restored.workspace_name("local", "second"), Some("Shells"));
    }

    #[test]
    fn invalid_names_and_corrupt_config_do_not_overwrite_saved_data() {
        let config = TestConfig::new();
        let mut names = MachineNames::load(&config.0).unwrap();
        names.rename(&config.0, "local", "Original").unwrap();
        let before = std::fs::read(&config.0).unwrap();
        for invalid in ["", "  ", "bad\x1b[2J", "two\nlines"] {
            assert_eq!(
                names
                    .rename(&config.0, "local", invalid)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(std::fs::read(&config.0).unwrap(), before);
        }
        std::fs::write(&config.0, b"invalid JSON").unwrap();
        assert!(names.rename(&config.0, "local", "New").is_err());
        assert_eq!(names.names["local"], "Original");
        assert_eq!(std::fs::read(&config.0).unwrap(), b"invalid JSON");
    }
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> =
        from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
