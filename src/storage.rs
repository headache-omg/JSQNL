use crate::engine::{Change, Database, Location, Row, SelectRows, matches, select_references};
use crate::plan::{AccessPath, BasePlan, QueryReport};
use crate::scratch::{KeyValidation, Scratch, configured_bytes};
use crate::syntax::{Column, Statement, parse, validate_version_line};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;

pub(crate) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

fn validate_regular(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hard-linked files are unsupported",
            ));
        }
    }
    Ok(())
}

pub(crate) fn regular_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_regular(&metadata)?;
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    if regular_metadata(path)?.is_none() {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    validate_regular(&file.metadata()?)?;
    Ok(file)
}

#[derive(Serialize, Deserialize)]
struct Transaction {
    version: u32,
    base: String,
    previous: String,
    changes: Vec<Change>,
}

fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(unix)]
pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    File::open(parent(path))?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn init(path: &Path) -> Result<()> {
    for suffix in [".log", ".idx", ".seek.sqlite", ".seek.sqlite.seal"] {
        let sidecar = sidecar(path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => bail!("sidecar exists: {}", sidecar.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", sidecar.display()));
            }
        }
    }
    private_options()
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?
        .sync_all()?;
    sync_parent(path)
}

struct Store {
    base: File,
    path: PathBuf,
    db: Database,
    base_hash: String,
    head: String,
    log_end: u64,
    uncommitted_bytes: usize,
    base_bytes: u64,
    base_rows: u64,
    materialization_limit: u64,
    key: Option<crate::journal_auth::JournalKey>,
    auth_version: Option<u32>,
}

impl Store {
    fn open(path: &Path) -> Result<Self> {
        let (path, base) = locked_base(path)?;
        let key = crate::journal_auth::JournalKey::load()?;
        let limit = crate::limits::materialized()?;
        let log_size = regular_metadata(&sidecar(&path, ".log"))?.map_or(0, |log| log.len());
        ensure!(
            base.metadata()?.len().saturating_add(log_size) <= limit,
            "database and journal exceed the materialization byte limit ({limit})"
        );
        let mut hash = Sha256::new();
        let (db, base_rows, base_bytes) = walk_base(
            &path,
            &base,
            BaseRead::Materialize,
            |line| hash.update(line.as_bytes()),
            |_, _, _| Ok(()),
            |_, _, _, _, _| Ok(()),
        )?;
        let base_hash = format!("{:x}", hash.finalize());
        let mut store = Self {
            base,
            path,
            db,
            base_hash,
            head: String::new(),
            log_end: 0,
            uncommitted_bytes: 0,
            base_bytes,
            base_rows,
            materialization_limit: limit,
            key,
            auth_version: None,
        };
        store.replay()?;
        Ok(store)
    }

    fn replay(&mut self) -> Result<()> {
        let path = sidecar(&self.path, ".log");
        let file = match open_regular(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
        };
        let mut reader = BufReader::new(file);
        let mut frame = Vec::new();
        let frame_limit = crate::limits::frame()?;
        loop {
            frame.clear();
            let size = crate::limits::frame_line(&mut reader, &mut frame, frame_limit)?;
            if frame.last() != Some(&b'\n') {
                self.uncommitted_bytes = size;
                break;
            }
            self.replay_frame(&frame[..frame.len() - 1])
                .with_context(|| format!("{} at byte {}", path.display(), self.log_end))?;
            self.log_end += size as u64;
        }
        Ok(())
    }

    fn replay_frame(&mut self, frame: &[u8]) -> Result<()> {
        let separator = frame
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed transaction")?;
        let checksum = &frame[..separator];
        let payload = &frame[separator + 1..];
        let digest = hash(payload);
        let version = if let Some(key) = &self.key {
            let version = key.verify(&self.path, checksum, payload)?;
            ensure!(
                self.auth_version.is_none_or(|prior| prior == version),
                "mixed journal authentication versions"
            );
            self.auth_version = Some(version);
            version
        } else {
            ensure!(
                !checksum.starts_with(b"hmac:") && !checksum.starts_with(b"hmac3:"),
                "JSQNL_JOURNAL_KEY_FILE is required for authenticated journals"
            );
            ensure!(
                checksum == digest.as_bytes(),
                "transaction checksum mismatch"
            );
            VERSION
        };
        let tx: Transaction = crate::internal_json::decode(payload)?;
        ensure!(
            tx.version == version,
            "unsupported or mismatched log version {}",
            tx.version
        );
        ensure!(
            tx.base == self.base_hash,
            "the .jsqnl base changed after a transaction was saved"
        );
        ensure!(tx.previous == self.head, "transaction order mismatch");
        ensure!(!tx.changes.is_empty(), "empty transaction");
        for (position, change) in tx.changes.iter().enumerate() {
            self.db
                .apply(change, Location::log(self.log_end, position))?;
        }
        self.head = digest;
        Ok(())
    }

    fn commit(&mut self, changes: Vec<Change>) -> Result<()> {
        ensure!(
            self.auth_version != Some(2),
            "legacy authenticated journal is read-only; use FORK to upgrade it"
        );
        let tx = Transaction {
            version: if self.key.is_some() { 3 } else { VERSION },
            base: self.base_hash.clone(),
            previous: self.head.clone(),
            changes,
        };
        let frame_limit = crate::limits::frame()?;
        let tag_len = if self.key.is_some() { 70 } else { 64 };
        let payload = crate::limits::encode(&tx, frame_limit - tag_len - 2)?;
        let next_size = self
            .base_bytes
            .saturating_add(self.log_end)
            .saturating_add(payload.len() as u64)
            .saturating_add(tag_len + 2);
        ensure!(
            next_size <= self.materialization_limit,
            "transaction would exceed the materialization byte limit ({})",
            self.materialization_limit
        );
        crate::internal_json::check_depth(&payload)?;
        let digest = hash(&payload);
        let path = sidecar(&self.path, ".log");
        regular_metadata(&path).with_context(|| format!("inspecting {}", path.display()))?;
        let mut file = private_options()
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        validate_regular(&file.metadata()?)?;
        sync_parent(&path)?;
        file.set_len(self.log_end)?;
        file.seek(SeekFrom::Start(self.log_end))?;

        let tag = self
            .key
            .as_ref()
            .map_or_else(|| digest.clone(), |key| key.tag(&self.path, &payload));
        let frame = [tag.as_bytes(), b"\t", &payload, b"\n"].concat();
        if let Err(error) = file.write_all(&frame).and_then(|()| file.sync_all()) {
            file.set_len(self.log_end)
                .and_then(|()| file.sync_all())
                .context("rolling back a failed log write; transaction outcome is uncertain")?;
            return Err(error).context("saving the transaction");
        }
        self.db.locate(&tx.changes, self.log_end);
        self.log_end += frame.len() as u64;
        self.head = digest;
        Ok(())
    }

    fn index(&self) -> Value {
        json!({
            "version": VERSION,
            "base_sha256": self.base_hash,
            "log_head_sha256": self.head,
            "log_bytes": self.log_end,
            "tables": self.db.index()
        })
    }

    fn write_index(&self, index: &Value) -> Result<()> {
        let path = sidecar(&self.path, ".idx");
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = sidecar(&path, &format!(".{}.{nonce}.tmp", std::process::id()));
        let mut file = private_options().create_new(true).open(&temp)?;
        let result = (|| -> Result<()> {
            serde_json::to_writer(&mut file, index)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &path)?;
            sync_parent(&path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result.with_context(|| format!("writing {}", path.display()))
    }
}

pub(crate) fn locked_base(path: &Path) -> Result<(PathBuf, File)> {
    let path = path
        .canonicalize()
        .with_context(|| format!("opening {}", path.display()))?;
    let base = open_regular(&path).with_context(|| format!("opening {}", path.display()))?;
    base.lock().context("locking the database")?;
    Ok((path, base))
}

pub(crate) fn has_log(path: &Path) -> Result<bool> {
    let log = sidecar(path, ".log");
    Ok(regular_metadata(&log)
        .with_context(|| format!("inspecting {}", log.display()))?
        .is_some_and(|metadata| metadata.len() != 0))
}

#[derive(Clone, Copy)]
pub(crate) enum BaseRead {
    ValidateKeys,
    Indexed,
    BuildIndex,
    Materialize,
}

pub(crate) fn walk_base(
    path: &Path,
    base: &File,
    mode: BaseRead,
    mut on_line: impl FnMut(&str),
    mut on_schema: impl FnMut(&Database, &str, &[Column]) -> Result<()>,
    mut on_row: impl FnMut(&str, u64, Option<&str>, u64, &Row) -> Result<()>,
) -> Result<(Database, u64, u64)> {
    let mut input = BufReader::with_capacity(256 * 1024, base);
    let mut db = Database::default();
    if matches!(mode, BaseRead::Materialize) {
        db.limit_materialization(crate::limits::materialized()?);
    }
    let record_limit = crate::limits::record()?;
    let mut line = String::new();
    let mut line_number = 0;
    let mut offset = 0;
    let mut row_count = 0;
    loop {
        let length = crate::limits::line(&mut input, &mut line, record_limit)?;
        if length == 0 {
            break;
        }
        line_number += 1;
        on_line(&line);
        let content = if line_number == 1 {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        } else {
            &line
        };
        let content = content.trim_start();
        let record_offset = offset + (line.len() - content.len()) as u64;
        let content = content.trim_end();
        (|| -> Result<()> {
            if content.starts_with('{') {
                let row = crate::import_json::bounded_row(content)
                    .context("JSONL requires one object per physical line")?;
                let (name, id, key) = db.register_base_row(
                    &row,
                    matches!(mode, BaseRead::ValidateKeys | BaseRead::Materialize),
                    matches!(mode, BaseRead::BuildIndex),
                )?;
                row_count += 1;
                on_row(name, id, key.as_deref(), record_offset, &row)?;
                if matches!(mode, BaseRead::Materialize) {
                    let name = name.to_owned();
                    db.retain_base_row(&name, id, row, record_offset)?;
                }
            } else {
                ensure!(!content.starts_with('\u{feff}'), "unexpected UTF-8 BOM");
                validate_version_line(content, line_number == 1)?;
                for (_, statement) in parse(content)? {
                    let Statement::Schema(name, columns) = statement else {
                        bail!("use 'run' for mixed scripts, or 'exec' for SQL against a database");
                    };
                    db.apply(
                        &Change::Schema {
                            table: name.clone(),
                            columns: columns.clone(),
                        },
                        Location::new("base", offset),
                    )?;
                    on_schema(&db, &name, &columns)?;
                }
            }
            Ok(())
        })()
        .with_context(|| format!("{} line {line_number}", path.display()))?;
        line.clear();
        offset += length as u64;
    }
    Ok((db, row_count, offset))
}

pub(crate) fn query(
    path: &Path,
    statements: Vec<(u64, Statement)>,
    mut emit: impl FnMut(Value) -> Result<()>,
) -> Result<QueryReport> {
    if let [(_, Statement::Select(table, columns, predicate, options))] = statements.as_slice() {
        let (path, mut base) = locked_base(path)?;
        if !has_log(&path)? {
            let validate = |db: &Database| {
                db.validate_select(table, columns.as_deref(), predicate.as_ref())?;
                db.validate_group_order(table, options)
            };
            let plan =
                BasePlan::prepare(&path, &base, table, predicate.as_ref(), options, validate)?;
            // JSQNL mutations only append to the journal. The open base handle is
            // a read-only snapshot, so the caller may reenter after planning.
            base.unlock().context("unlocking the read-only snapshot")?;
            let validation_scratch = if matches!(
                plan,
                BasePlan::Scan {
                    indexed_validation: false
                }
            ) && !options.group.is_empty()
                && base.metadata()?.len()
                    > configured_bytes(
                        "JSQNL_GROUP_MEMORY_BYTES",
                        crate::aggregate::DEFAULT_BUDGET,
                    )? {
                Some(Rc::new(Scratch::new()?))
            } else {
                None
            };
            let mut key_validation = validation_scratch.as_deref().map(KeyValidation::new);
            let mut validation_bytes = 0;
            let mut results =
                SelectRows::with_scratch(columns.as_deref(), options, validation_scratch.clone())?;
            let mut selected = |id, row: &Row| -> Result<()> {
                if predicate
                    .as_ref()
                    .is_none_or(|filter| matches(filter, id, row))
                    && let Some(value) = results.push(id, row)?
                {
                    emit(value)?;
                }
                Ok(())
            };
            let record_limit = crate::limits::record()?;
            let (access, rows, bytes) = match plan {
                BasePlan::Count(count) => {
                    results.set_count(count);
                    (AccessPath::IndexCount, 0, 0)
                }
                BasePlan::Seek(location) => {
                    let mut bytes = 0;
                    if let Some((id, offset)) = location {
                        base.seek(SeekFrom::Start(offset))?;
                        let mut line = String::new();
                        bytes = crate::limits::line(
                            &mut BufReader::new(&base),
                            &mut line,
                            record_limit,
                        )? as u64;
                        let row: Row = crate::import_json::bounded_row(line.trim())
                            .context("reading indexed row")?;
                        selected(id, &row)?;
                    }
                    (AccessPath::IndexSeek, u64::from(location.is_some()), bytes)
                }
                BasePlan::Scan { indexed_validation } => {
                    let (db, rows, bytes) = walk_base(
                        &path,
                        &base,
                        if indexed_validation {
                            BaseRead::Indexed
                        } else if key_validation.is_some() {
                            BaseRead::BuildIndex
                        } else {
                            BaseRead::ValidateKeys
                        },
                        |_| {},
                        |db, name, _| {
                            if name == table {
                                validate(db)?;
                            }
                            Ok(())
                        },
                        |name, id, primary, _, row| {
                            if let (Some(key), Some(validation)) = (primary, &mut key_validation) {
                                validation.insert(name, id, key)?;
                            }
                            if name == table {
                                selected(id, row)?;
                            }
                            Ok(())
                        },
                    )?;
                    validate(&db)?;
                    if let Some(validation) = key_validation.take() {
                        validation_bytes = validation.finish()?;
                    }
                    (AccessPath::SequentialScan, rows, bytes)
                }
                BasePlan::GraceJoin {
                    indexed_validation,
                    budget,
                } => {
                    let stats = crate::grace_join::execute(
                        &path,
                        &base,
                        indexed_validation,
                        budget,
                        table,
                        columns.as_deref(),
                        predicate.as_ref(),
                        options,
                        &mut emit,
                    )?;
                    let mut report = QueryReport {
                        rows_read: stats.rows,
                        base_bytes_read: stats.bytes,
                        spill_bytes_written: Some(stats.spill_bytes),
                        oversized_partitions: Some(stats.skew_partitions),
                        ..QueryReport::new(AccessPath::GraceHashJoin, &statements)
                    };
                    report.record_spill(stats.result_spill);
                    return Ok(report);
                }
                BasePlan::Join { indexed_validation } => {
                    let right_name = &options
                        .join
                        .as_ref()
                        .expect("join plan has a right table")
                        .table;
                    let join = options.join.as_ref().expect("join plan has a right table");
                    let needed_right = (columns.is_some() || options.count.is_some()).then(|| {
                        let alias = join.alias.as_deref().unwrap_or(right_name);
                        let mut names =
                            select_references(columns.as_deref(), predicate.as_ref(), options);
                        names.extend([join.first_key.clone(), join.second_key.clone()]);
                        names
                            .into_iter()
                            .filter_map(|name| {
                                if let Some((qualifier, column)) = name.split_once('.') {
                                    (qualifier == alias).then(|| column.to_owned())
                                } else {
                                    Some(name)
                                }
                            })
                            .collect::<std::collections::BTreeSet<_>>()
                    });
                    let mut right = Vec::new();
                    let mut left_range: Option<(u64, u64)> = None;
                    let (db, rows, bytes) = walk_base(
                        &path,
                        &base,
                        if indexed_validation {
                            BaseRead::Indexed
                        } else {
                            BaseRead::ValidateKeys
                        },
                        |_| {},
                        |_, _, _| Ok(()),
                        |name, id, _, offset, row| {
                            if name == right_name {
                                let row = if let Some(fields) = &needed_right {
                                    row.iter()
                                        .filter(|(name, _)| fields.contains(*name))
                                        .map(|(name, value)| (name.clone(), value.clone()))
                                        .collect()
                                } else {
                                    row.clone()
                                };
                                right.push((id, row));
                            }
                            if name == table {
                                left_range.get_or_insert((offset, offset)).1 = offset;
                            }
                            Ok(())
                        },
                    )?;
                    let mut probe_rows = 0;
                    let mut probe_bytes = 0;
                    let spilled = db.join_rows(
                        table,
                        columns.as_deref(),
                        predicate.as_ref(),
                        options,
                        right,
                        |consume| {
                            if let Some((start, end)) = left_range {
                                base.seek(SeekFrom::Start(start))?;
                                let mut reader = BufReader::with_capacity(256 * 1024, &base);
                                let mut line = String::new();
                                let mut offset = start;
                                while offset <= end {
                                    let bytes =
                                        crate::limits::line(&mut reader, &mut line, record_limit)?
                                            as u64;
                                    ensure!(bytes != 0, "base truncated during JOIN");
                                    if line.trim_start().starts_with('{') {
                                        let row = crate::import_json::bounded_row(line.trim())?;
                                        probe_rows += 1;
                                        consume(probe_rows, &row)?;
                                    }
                                    offset += bytes;
                                    probe_bytes += bytes;
                                    line.clear();
                                }
                            }
                            Ok(())
                        },
                        &mut emit,
                    )?;
                    let mut report = QueryReport {
                        rows_read: rows + probe_rows,
                        base_bytes_read: bytes + probe_bytes,
                        ..QueryReport::new(AccessPath::StreamingJoin, &statements)
                    };
                    report.record_spill(spilled);
                    return Ok(report);
                }
            };
            let spilled = results.finish_with(&mut emit)?;
            let mut report = QueryReport {
                rows_read: rows,
                base_bytes_read: bytes,
                ..QueryReport::new(access, &statements)
            };
            if validation_bytes != 0 {
                report.spill_bytes_written = Some(validation_bytes);
            }
            report.record_spill(spilled);
            return Ok(report);
        }
    }

    let mut report = QueryReport::new(AccessPath::Materialized, &statements);
    let mut store = Store::open(path)?;
    report.base_bytes_read = store.base_bytes;
    report.rows_read = store.base_rows;
    report.journal_bytes_read = store.log_end + store.uncommitted_bytes as u64;
    let mut changes = Vec::new();
    let mut rows = Vec::new();
    let mut result_bytes = 0;
    let result_limit = crate::limits::materialized()?;
    for (offset, statement) in statements {
        let (pending, selected) = store
            .db
            .execute(statement, Location::new("pending", offset))
            .with_context(|| format!("input byte {offset}"))?;
        changes.extend(pending);
        for value in selected {
            crate::limits::charge_result(&mut result_bytes, result_limit, &value)?;
            rows.push(value);
        }
    }
    if !changes.is_empty() {
        store.commit(changes)?;
        if let Err(error) = store.write_index(&store.index()) {
            eprintln!(
                "jsqnl: transaction saved, index refresh failed: {}",
                crate::terminal_diagnostic(&error)
            );
        }
    }
    drop(store);
    for value in rows {
        emit(value)?;
    }
    Ok(report)
}

fn validate_base(
    path: &Path,
    base: &File,
    on_line: impl FnMut(&str),
    mut on_row: impl FnMut(&str, u64, u64),
) -> Result<Database> {
    // Small files retain only keys. Large files validate uniqueness on disk.
    let scratch = (base.metadata()?.len() > crate::aggregate::DEFAULT_BUDGET)
        .then(Scratch::new)
        .transpose()?;
    let mut keys = scratch.as_ref().map(KeyValidation::new);
    let (db, _, _) = walk_base(
        path,
        base,
        if keys.is_some() {
            BaseRead::BuildIndex
        } else {
            BaseRead::ValidateKeys
        },
        on_line,
        |_, _, _| Ok(()),
        |table, id, key, offset, _| {
            if let (Some(keys), Some(key)) = (&mut keys, key) {
                keys.insert(table, id, key)?;
            }
            on_row(table, id, offset);
            Ok(())
        },
    )?;
    if let Some(keys) = keys {
        keys.finish()?;
    }
    Ok(db)
}

fn check_base(path: &Path, base: &File) -> Result<Value> {
    let disk_status = crate::disk_index::check(path, base)?;
    let index_path = sidecar(path, ".idx");
    let (mut saved, mut status) = match open_regular(&index_path) {
        Ok(file) => match serde_json::from_reader::<_, Value>(BufReader::new(file)) {
            Ok(saved) => (Some(saved), "current"),
            Err(_) => (None, "invalid"),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => (None, "missing"),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", index_path.display()));
        }
    };
    let mut hash = saved.is_some().then(Sha256::new);
    let mut matches = true;
    let db = validate_base(
        path,
        base,
        |line| {
            if let Some(hash) = &mut hash {
                hash.update(line.as_bytes());
            }
        },
        |table, id, offset| {
            if matches && let Some(saved) = &mut saved {
                let location = saved
                    .get_mut("tables")
                    .and_then(Value::as_object_mut)
                    .and_then(|tables| tables.get_mut(table))
                    .and_then(Value::as_object_mut)
                    .and_then(|rows| rows.remove(&id.to_string()));
                matches = location.as_ref() == Some(&json!({"source": "base", "offset": offset}));
            }
        },
    )?;
    let tables: serde_json::Map<_, _> = db
        .base_row_counts()
        .map(|(name, count)| (name.to_owned(), json!(count)))
        .collect();
    if let (Some(saved), Some(hash)) = (saved, hash) {
        let expected_tables: serde_json::Map<_, _> = tables
            .keys()
            .map(|name| (name.clone(), json!({})))
            .collect();
        if !matches
            || saved
                != json!({
                    "version": VERSION,
                    "base_sha256": format!("{:x}", hash.finalize()),
                    "log_head_sha256": "",
                    "log_bytes": 0,
                    "tables": expected_tables,
                })
        {
            status = "stale";
        }
    }
    let mut report = json!({
        "committed_log_bytes": 0,
        "uncommitted_log_bytes": 0,
        "index": status,
        "rows_by_table": tables,
    });
    if let Some(status) = disk_status {
        report["disk_index"] = json!(status);
    }
    Ok(report)
}

pub fn check(path: &Path) -> Result<Value> {
    let (snapshot_path, base) = locked_base(path)?;
    if !has_log(&snapshot_path)? {
        return check_base(&snapshot_path, &base);
    }
    drop(base);
    let store = Store::open(path)?;
    let index = store.index();
    let index_path = sidecar(&store.path, ".idx");
    let index_status = match open_regular(&index_path) {
        Ok(file) => match serde_json::from_reader::<_, Value>(BufReader::new(file)) {
            Ok(saved) if saved == index => "current",
            Ok(_) => "stale",
            Err(_) => "invalid",
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", index_path.display()));
        }
    };
    let tables = index["tables"]
        .as_object()
        .expect("index always contains a tables object")
        .iter()
        .map(|(name, rows)| {
            (
                name.clone(),
                json!(rows.as_object().expect("table index is an object").len()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(json!({
        "committed_log_bytes": store.log_end,
        "uncommitted_log_bytes": store.uncommitted_bytes,
        "index": index_status,
        "rows_by_table": tables,
    }))
}

pub fn rebuild_index(path: &Path) -> Result<Value> {
    let store = Store::open(path)?;
    let index = store.index();
    store.write_index(&index)?;
    Ok(index)
}

fn fork_authenticated_log(
    source_log: impl Read,
    destination_log: &mut File,
    source_path: &Path,
    destination_path: &Path,
    key: &crate::journal_auth::JournalKey,
    base_bytes: u64,
) -> Result<()> {
    let frame_limit = crate::limits::frame()?;
    let materialization_limit = crate::limits::materialized()?;
    let mut source = BufReader::new(source_log);
    let mut frame = Vec::new();
    let mut previous = String::new();
    let mut written = base_bytes;
    loop {
        frame.clear();
        if crate::limits::frame_line(&mut source, &mut frame, frame_limit)? == 0 {
            break;
        }
        ensure!(
            frame.pop() == Some(b'\n'),
            "source log was truncated during fork"
        );
        let separator = frame
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed transaction")?;
        let payload = &frame[separator + 1..];
        let version = key.verify(source_path, &frame[..separator], payload)?;
        let mut tx: Transaction = crate::internal_json::decode(payload)?;
        ensure!(
            tx.version == version,
            "unsupported or mismatched log version"
        );
        tx.version = 3;
        tx.previous = previous;
        let payload = crate::limits::encode(&tx, frame_limit - 72)?;
        let tag = key.tag(destination_path, &payload);
        written = written.saturating_add((tag.len() + payload.len() + 2) as u64);
        ensure!(
            written <= materialization_limit,
            "fork would exceed the materialization byte limit"
        );
        destination_log.write_all(tag.as_bytes())?;
        destination_log.write_all(b"\t")?;
        destination_log.write_all(&payload)?;
        destination_log.write_all(b"\n")?;
        previous = hash(&payload);
    }
    Ok(())
}

/// Publish the base file last, so the branch cannot be opened before its log is ready.
pub fn fork(path: &Path, directory: &Path) -> Result<()> {
    let (snapshot_path, base) = locked_base(path)?;
    let (source_path, mut source, log_end, key) = if has_log(&snapshot_path)? {
        drop(base);
        let store = Store::open(path)?;
        (store.path, store.base, store.log_end, store.key)
    } else {
        validate_base(&snapshot_path, &base, |_| {}, |_, _, _| {})?;
        (snapshot_path, base, 0, None)
    };
    fs::create_dir(directory)
        .with_context(|| format!("creating branch directory {}", directory.display()))?;
    let destination = directory.join(
        source_path
            .file_name()
            .context("database has no file name")?,
    );
    let staged = sidecar(&destination, ".pending");
    let destination_log = sidecar(&destination, ".log");
    let preparation = (|| -> Result<()> {
        let mut base = private_options().create_new(true).open(&staged)?;
        source.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut source, &mut base)?;
        base.sync_all()?;
        drop(base);
        if log_end != 0 {
            let mut log = open_regular(&sidecar(&source_path, ".log"))?.take(log_end);
            let mut copy = private_options().create_new(true).open(&destination_log)?;
            if let Some(key) = &key {
                let canonical = directory.canonicalize()?.join(
                    destination
                        .file_name()
                        .context("missing branch file name")?,
                );
                fork_authenticated_log(
                    &mut log,
                    &mut copy,
                    &source_path,
                    &canonical,
                    key,
                    source.metadata()?.len(),
                )?;
                ensure!(log.limit() == 0, "source log was truncated during fork");
            } else {
                ensure!(
                    std::io::copy(&mut log, &mut copy)? == log_end,
                    "source log was truncated during fork"
                );
            }
            copy.sync_all()?;
        }
        sync_parent(&destination)?;
        sync_parent(directory)?;
        fs::rename(&staged, &destination)?;
        Ok(())
    })();
    if let Err(error) = preparation {
        let _ = fs::remove_file(&staged);
        let _ = fs::remove_file(&destination_log);
        let _ = fs::remove_dir(directory);
        return Err(error).context("creating database branch");
    }
    sync_parent(&destination).context("branch was published but its directory could not be synced")
}
