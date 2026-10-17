//! Explicit runtime namespaces. Library callers never change process environment variables.
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::Duration,
};

use sha2::{Digest, Sha256};
pub const DEFAULT_ENVIRONMENT_ID: &str = "default";

#[derive(Clone, Debug)]
pub struct Environment {
    id: String,
    root: PathBuf,
    signal: Arc<Signal>,
}
#[derive(Debug, Default)]
pub(crate) struct Signal {
    pub version: Mutex<u64>,
    pub changed: Condvar,
}
thread_local! { static CURRENT: RefCell<Option<Environment>> = const { RefCell::new(None) }; }
impl Environment {
    pub fn new(id: &str) -> io::Result<Self> {
        let home = crate::config::home_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "user home is unavailable")
        })?;
        Self::at(home.join(".zmux"), id)
    }
    /// Root is a dedicated data root; it must be absolute and may not contain `..`.
    pub fn at(root: impl AsRef<Path>, id: &str) -> io::Result<Self> {
        validate_id(id)?;
        let root = root.as_ref();
        if !root.is_absolute()
            || root.components().any(|c| matches!(c, Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "environment root must be absolute without '..'",
            ));
        }
        let mut root: PathBuf = root
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        // Resolve an existing prefix so symlink aliases cannot acquire separate locks.
        let mut tail = Vec::new();
        let mut prefix = root.clone();
        while !prefix.exists() {
            tail.push(
                prefix
                    .file_name()
                    .ok_or_else(|| io::Error::other("invalid root"))?
                    .to_os_string(),
            );
            prefix.pop();
        }
        prefix = prefix.canonicalize()?;
        for part in tail.into_iter().rev() {
            prefix.push(part);
        }
        root = prefix;
        Ok(Self {
            id: id.into(),
            root,
            signal: Arc::default(),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn directory(&self) -> PathBuf {
        self.root.join("env").join(&self.id)
    }
    pub fn config_file(&self) -> PathBuf {
        self.directory().join("config/zmux.conf")
    }
    pub fn machines_file(&self) -> PathBuf {
        self.directory().join("config/machines.json")
    }
    pub fn history_directory(&self) -> PathBuf {
        self.directory().join("history")
    }
    pub fn drop_directory(&self) -> PathBuf {
        self.directory().join("drop")
    }
    pub fn log_directory(&self) -> PathBuf {
        self.directory().join("logs")
    }
    pub fn log_file(&self, name: &str) -> PathBuf {
        let dir = self.log_directory();
        let _ = secure_dir(&dir);
        dir.join(name)
    }
    /// Includes the complete canonical root, ID and local user. No namespace can enumerate another's IPC.
    pub fn namespace_key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.root.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(self.id.as_bytes());
        #[cfg(unix)]
        hash.update(unsafe { libc::getuid() }.to_le_bytes());
        #[cfg(windows)]
        {
            hash.update(
                std::env::var_os("USERPROFILE")
                    .unwrap_or_default()
                    .as_encoded_bytes(),
            );
        }
        format!("{:x}", hash.finalize())[..24].to_string()
    }
    pub fn runtime_directory(&self) -> io::Result<PathBuf> {
        #[cfg(unix)]
        let base = PathBuf::from("/tmp")
            .join(format!("zmux-{}", unsafe { libc::getuid() }));
        #[cfg(not(unix))]
        let base = std::env::temp_dir().join("zmux");
        secure_dir(&base)?;
        let dir = base.join(self.namespace_key());
        secure_dir(&dir)?;
        Ok(dir)
    }
    pub fn prepare(&self) -> io::Result<()> {
        secure_dir(&self.root)?;
        secure_dir(&self.root.join("env"))?;
        secure_dir(&self.directory())?;
        for dir in [
            self.directory().join("config"),
            self.history_directory(),
            self.drop_directory(),
            self.log_directory(),
        ] {
            secure_dir(&dir)?;
        }
        Ok(())
    }
    /// One live host per environment/socket. Clients do not acquire this lock.
    pub fn server_lock(&self, socket: &str) -> io::Result<File> {
        validate_socket(socket)?;
        self.prepare()?;
        let dir = self.directory().join("locks");
        secure_dir(&dir)?;
        let path = dir.join(format!("{socket}.lock"));
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(io::Error::other("lock path is a symlink"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options.open(path)?;
        lock.try_lock().map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("zmux environment '{}' socket '{socket}' is already in use: {e}", self.id)))?;
        Ok(lock)
    }
    /// Scoped compatibility adapter for existing synchronous APIs. Nested scopes restore on panic.
    /// Child threads must be created with `environment::spawn`/`spawn_named`.
    pub fn enter<T>(&self, operation: impl FnOnce() -> T) -> T {
        struct Restore(Option<Environment>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|c| {
                    c.replace(self.0.take());
                });
            }
        }
        let previous = CURRENT.with(|c| c.replace(Some(self.clone())));
        let _restore = Restore(previous);
        operation()
    }
    pub(crate) fn notify(&self) {
        let mut v = self
            .signal
            .version
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *v = v.wrapping_add(1);
        self.signal.changed.notify_all();
    }
    pub(crate) fn wait(&self, after: u64, timeout: Duration) -> u64 {
        let v = self
            .signal
            .version
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let (v, _) = self
            .signal
            .changed
            .wait_timeout_while(v, timeout, |v| *v == after)
            .unwrap_or_else(|p| p.into_inner());
        *v
    }
}
pub fn current() -> Environment {
    CURRENT.with(|c| c.borrow().clone()).unwrap_or_else(|| {
        static DEFAULT: OnceLock<Environment> = OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                Environment::new(DEFAULT_ENVIRONMENT_ID)
                    .expect("zmux environment requires a user home")
            })
            .clone()
    })
}
pub fn spawn<F, T>(operation: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let env = current();
    std::thread::spawn(move || env.enter(operation))
}
pub fn spawn_named<F, T>(
    builder: std::thread::Builder,
    operation: F,
) -> io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let env = current();
    builder.spawn(move || env.enter(operation))
}
pub fn validate_id(id: &str) -> io::Result<()> {
    let invalid = id.is_empty()
        || id.len() > 64
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'-'
                || b == b'_'
        });
    let reserved = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5",
        "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5",
        "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if invalid || reserved.contains(&id) {
        Err(io::Error::new(io::ErrorKind::InvalidInput,"environment ID must be 1–64 lowercase ASCII letters, digits, '-' or '_', start with a letter/digit, and not be a reserved device name"))
    } else {
        Ok(())
    }
}
pub fn validate_socket(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        Err(io::Error::new(io::ErrorKind::InvalidInput,"socket name must be 1–64 ASCII letters, digits, '.', '_' or '-', and cannot start with '.'"))
    } else {
        Ok(())
    }
}
pub(crate) fn secure_dir(path: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(io::Error::other(format!(
                "not a private directory: {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != unsafe { libc::getuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "environment directory has another owner",
                ));
            }
        }
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
