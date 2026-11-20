use crate::storage::{private_options, regular_metadata};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use std::cell::Cell;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_LIMIT: u64 = 8 * 1024 * 1024 * 1024;

pub(crate) fn configured_bytes(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => {
            let bytes = value
                .parse::<u64>()
                .with_context(|| format!("invalid {name}"))?;
            ensure!(bytes >= 1024, "{name} must be at least 1024");
            Ok(bytes)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn private_directory(path: &Path) -> Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir(),
        "expected a private directory at {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
            "temporary directory must be owned by the current user and have mode 0700"
        );
    }
    Ok(())
}

fn registry() -> PathBuf {
    #[cfg(unix)]
    let name = format!("jsqnl-v1-{}", unsafe { libc::geteuid() });
    #[cfg(not(unix))]
    let name = "jsqnl-v1".to_owned();
    std::env::temp_dir().join(name)
}

fn open_lock(path: &Path) -> Result<File> {
    regular_metadata(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

fn gate(root: &Path) -> Result<File> {
    let path = root.join("registry.lock");
    let lock = match private_options().read(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => open_lock(&path)?,
        Err(error) => return Err(error.into()),
    };
    lock.lock()?;
    Ok(lock)
}

fn recover_locked(root: &Path) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|n| n.strip_prefix("q-")) else {
            continue;
        };
        let components: Vec<_> = suffix.split('-').collect();
        if components.len() != 3
            || components
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        // Drop may already have removed this directory. Never inspect a replacement link.
        if !path.try_exists()? {
            continue;
        }
        validate_directory(&path)?;
        let lock = match open_lock(&path.join("active.lock")) {
            Ok(lock) => match lock.try_lock() {
                Ok(()) => Some(lock),
                Err(std::fs::TryLockError::WouldBlock) => continue,
                Err(error) => return Err(error.into()),
            },
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        match fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(lock);
    }
    Ok(removed)
}

pub(crate) fn recover() -> Result<usize> {
    let root = registry();
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    validate_directory(&root)?;
    let _gate = gate(&root)?;
    recover_locked(&root)
}

pub(crate) struct Scratch {
    path: PathBuf,
    root: PathBuf,
    _active: File,
    limit: u64,
    used: Cell<u64>,
}

impl Scratch {
    pub(crate) fn new() -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let limit = configured_bytes("JSQNL_TEMP_LIMIT_BYTES", DEFAULT_LIMIT)?;
        let root = registry();
        if let Err(error) = private_directory(&root)
            && !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
        {
            return Err(error);
        }
        validate_directory(&root)?;
        let _gate = gate(&root)?;
        recover_locked(&root)?;
        let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("q-{}-{time}-{count}", std::process::id()));
        private_directory(&path)?;
        let active = private_options()
            .read(true)
            .create_new(true)
            .open(path.join("active.lock"))?;
        active.lock()?;
        Ok(Self {
            path,
            root,
            _active: active,
            limit,
            used: Cell::new(0),
        })
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub(crate) fn account(&self, bytes: u64) -> Result<()> {
        let total = self
            .used
            .get()
            .checked_add(bytes)
            .context("temporary storage size overflow")?;
        ensure!(
            total <= self.limit,
            "temporary storage quota exceeded (limit {} bytes)",
            self.limit
        );
        self.used.set(total);
        Ok(())
    }

    pub(crate) fn release(&self, bytes: u64) {
        self.used.set(self.used.get().saturating_sub(bytes));
    }

    pub(crate) fn reserve_sqlite(&self, name: &str) -> Result<(Connection, u64)> {
        self.reserve_sqlite_with_budget(name, (self.limit - self.used.get()) / 2)
    }

    fn reserve_sqlite_with_budget(
        &self,
        name: &str,
        reservation: u64,
    ) -> Result<(Connection, u64)> {
        let connection = self.open_sqlite(name, reservation)?;
        self.account(reservation)?;
        Ok((connection, reservation))
    }

    fn open_sqlite(&self, name: &str, remaining: u64) -> Result<Connection> {
        ensure!(
            remaining >= 4096,
            "temporary storage quota exceeded (limit {} bytes)",
            self.limit
        );
        let connection = Connection::open(self.path(name))?;
        connection.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-4096; PRAGMA page_size=4096;")?;
        connection.pragma_update(None, "max_page_count", remaining / 4096)?;
        Ok(connection)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Serialize removal with the scavenger, including removal of the lock file.
        if let Ok(_gate) = gate(&self.root) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(crate) struct KeyValidation {
    scratch: ScratchSlot,
    database: Option<Connection>,
    reserved: u64,
    exclusive: bool,
}

impl KeyValidation {
    pub(crate) fn new(scratch: ScratchSlot) -> Self {
        Self {
            scratch,
            database: None,
            reserved: 0,
            exclusive: false,
        }
    }

    /// Use when key validation is the only consumer of this scratch directory.
    pub(crate) fn exclusive(scratch: ScratchSlot) -> Self {
        Self {
            exclusive: true,
            ..Self::new(scratch)
        }
    }

    pub(crate) fn insert(&mut self, relation: &str, id: u64, key: &str) -> Result<()> {
        if self.database.is_none() {
            let scratch = self.scratch.get()?;
            let (db, reserved) = if self.exclusive {
                let remaining = scratch.limit - scratch.used.get();
                scratch.reserve_sqlite_with_budget("keys.sqlite", remaining)?
            } else {
                scratch.reserve_sqlite("keys.sqlite")?
            };
            db.execute_batch("CREATE TABLE keys (name TEXT, key TEXT, PRIMARY KEY (name, key)) WITHOUT ROWID; BEGIN;")?;
            self.database = Some(db);
            self.reserved = reserved;
        }
        self.database
            .as_ref()
            .expect("key validation database is open")
            .prepare_cached("INSERT INTO keys VALUES (?, ?)")?
            .execute(rusqlite::params![relation, key])
            .with_context(|| format!("validating PRIMARY KEY of {relation} row {id}"))?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<u64> {
        let Some(database) = self.database.take() else {
            return Ok(0);
        };
        database.execute_batch("COMMIT;")?;
        drop(database);
        let scratch = self.scratch.get()?;
        let path = scratch.path("keys.sqlite");
        let bytes = path.metadata()?.len();
        fs::remove_file(path)?;
        scratch.release(self.reserved);
        Ok(bytes)
    }
}

/// Buffer each partition in memory and open it only for the duration of a flush.
#[derive(Default)]
pub(crate) struct PartitionWriters {
    buffers: std::collections::HashMap<PathBuf, Vec<u8>>,
}

impl PartitionWriters {
    const BUFFER: usize = 32 * 1024;

    fn open(path: &Path) -> Result<File> {
        Ok(private_options().append(true).create(true).open(path)?)
    }

    fn flush(path: &Path, buffer: &mut Vec<u8>) -> Result<()> {
        use std::io::Write;
        if !buffer.is_empty() {
            Self::open(path)?.write_all(buffer)?;
            buffer.clear();
        }
        Ok(())
    }

    pub(crate) fn append(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let buffer = if let Some(buffer) = self.buffers.get_mut(path) {
            buffer
        } else {
            self.buffers.entry(path.to_owned()).or_default()
        };
        if buffer.len() + bytes.len() + 1 > Self::BUFFER {
            Self::flush(path, buffer)?;
        }
        if bytes.len() >= Self::BUFFER {
            let mut file = Self::open(path)?;
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
        } else {
            buffer.extend_from_slice(bytes);
            buffer.push(b'\n');
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<()> {
        for (path, mut buffer) in self.buffers {
            Self::flush(&path, &mut buffer)?;
        }
        Ok(())
    }
}

/// Lazily shares one query's temporary directory across physical operators.
#[derive(Clone, Default)]
pub(crate) struct ScratchSlot(std::rc::Rc<std::cell::OnceCell<std::rc::Rc<Scratch>>>);

impl ScratchSlot {
    pub(crate) fn new(scratch: Option<std::rc::Rc<Scratch>>) -> Self {
        let slot = Self::default();
        if let Some(scratch) = scratch {
            let _ = slot.0.set(scratch);
        }
        slot
    }

    pub(crate) fn get(&self) -> Result<std::rc::Rc<Scratch>> {
        if let Some(scratch) = self.0.get() {
            return Ok(scratch.clone());
        }
        let scratch = std::rc::Rc::new(Scratch::new()?);
        let _ = self.0.set(scratch.clone());
        Ok(scratch)
    }
}
