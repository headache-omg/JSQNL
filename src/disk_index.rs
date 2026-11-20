use crate::engine::{Change, Database, Location, integral_literal};
use crate::storage::{
    BaseRead, has_log, locked_base, open_regular, private_options, regular_metadata, sidecar,
    sync_parent, walk_base,
};
use crate::syntax::{Column, Kind, Op, Predicate};
use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn fingerprint(file: &File) -> Result<String> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "1:{}:{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "1:{}:{:?}:{:?}",
            metadata.len(),
            metadata.modified()?,
            metadata.created().ok()
        ))
    }
}

fn digest(file: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Seal {
    base: String,
    index: String,
    sha256: String,
    max_line_bytes: Option<u64>,
}

fn load_seal(index: &Path) -> Result<Option<Seal>> {
    let path = sidecar(index, ".seal");
    let file = match open_regular(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening disk index seal"),
    };
    let mut bytes = Vec::new();
    file.take(2049).read_to_end(&mut bytes)?;
    if bytes.len() > 2048 {
        return Ok(None);
    }
    Ok(serde_json::from_slice::<Seal>(&bytes).ok())
}

fn write_seal(
    index: &Path,
    base: String,
    sha256: String,
    max_line_bytes: u64,
    stage: &Path,
) -> Result<()> {
    let file = open_regular(index)?;
    let seal = Seal {
        base,
        index: fingerprint(&file)?,
        sha256,
        max_line_bytes: Some(max_line_bytes),
    };
    let mut writer = private_options().create_new(true).open(stage)?;
    serde_json::to_writer(&mut writer, &seal)?;
    writer.write_all(b"\n")?;
    writer.sync_all()?;
    drop(writer);
    std::fs::rename(stage, sidecar(index, ".seal"))?;
    sync_parent(index)
}

fn unexpected_sidecar(index: &Path) -> Result<Option<std::path::PathBuf>> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let path = sidecar(index, suffix);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Ok(Some(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", path.display()));
            }
        }
    }
    Ok(None)
}

/// Expensive integrity check for the CHECK command. Ordinary queries use the
/// filesystem fingerprints, so index seek does not read or hash the SQLite file.
pub(crate) fn check(path: &Path, base: &File) -> Result<Option<&'static str>> {
    let index = sidecar(path, ".seek.sqlite");
    let mut file = match open_regular(&index) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening disk index"),
    };
    if unexpected_sidecar(&index)?.is_some() {
        return Ok(Some("invalid"));
    }
    let Some(seal) = load_seal(&index)? else {
        return Ok(Some("unsealed"));
    };
    if seal.base != fingerprint(base)? {
        return Ok(Some("stale"));
    }
    let before = fingerprint(&file)?;
    if before != seal.index || digest(&mut file)? != seal.sha256 || before != fingerprint(&file)? {
        return Ok(Some("invalid"));
    }
    if seal.max_line_bytes.is_none() {
        return Ok(Some("legacy"));
    }
    Ok(Some("current"))
}

pub fn build(path: &Path) -> Result<()> {
    let (path, base) = locked_base(path)?;
    ensure!(
        !has_log(&path)?,
        "disk-index requires a base file without an update log"
    );
    let signature = fingerprint(&base)?;
    let index = sidecar(&path, ".seek.sqlite");
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stage = sidecar(&index, &format!(".{nonce}.tmp"));
    let seal_stage = sidecar(&stage, ".seal");
    private_options().create_new(true).open(&stage)?;
    let result = (|| -> Result<()> {
        let mut sqlite = Connection::open(&stage)?;
        sqlite.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA cache_size=-65536;
             CREATE TABLE meta (fingerprint TEXT NOT NULL);
             CREATE TABLE schemas (name TEXT PRIMARY KEY, columns_json TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE offsets (
                 name TEXT NOT NULL, id INTEGER NOT NULL, pk TEXT, offset INTEGER NOT NULL,
                 PRIMARY KEY(name, id), UNIQUE(name, pk)
             ) WITHOUT ROWID;",
        )?;
        let transaction = sqlite.transaction()?;
        let mut max_line_bytes = 0;
        {
            let mut schema = transaction.prepare("INSERT INTO schemas VALUES (?, ?)")?;
            let mut row = transaction.prepare("INSERT INTO offsets VALUES (?, ?, ?, ?)")?;
            walk_base(
                &path,
                &base,
                BaseRead::BuildIndex,
                |line| max_line_bytes = max_line_bytes.max(line.len() as u64),
                |_, name, columns| {
                    schema.execute(params![name, serde_json::to_string(columns)?])?;
                    Ok(())
                },
                |name, id, key, offset, _| {
                    row.execute(params![name, id, key, offset])
                        .with_context(|| {
                            if let Some(key) = key {
                                format!("indexing {name} row {id} with PRIMARY KEY {key}")
                            } else {
                                format!("indexing {name} row {id}")
                            }
                        })?;
                    Ok(())
                },
            )?;
        }
        ensure!(
            fingerprint(&base)? == signature,
            "database changed while building the disk index"
        );
        transaction.execute("INSERT INTO meta VALUES (?)", [&signature])?;
        transaction.commit()?;
        drop(sqlite);
        let mut file = File::open(&stage)?;
        file.sync_all()?;
        let sha256 = digest(&mut file)?;
        drop(file);
        std::fs::rename(&stage, &index)?;
        write_seal(&index, signature, sha256, max_line_bytes, &seal_stage)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&stage);
        let _ = std::fs::remove_file(&seal_stage);
    }
    result
}

fn point<'a>(
    predicate: &'a Predicate,
    primary: Option<&Column>,
) -> Option<(&'a str, &'a serde_json::Value)> {
    match predicate {
        Predicate::And(terms) => terms.iter().find_map(|term| point(term, primary)),
        Predicate::Compare(column, Op::Eq, value)
            if column == "_line" || primary.is_some_and(|pk| pk.name == *column) =>
        {
            Some((column, value))
        }
        _ => None,
    }
}

fn open_current(path: &Path, base: &File) -> Result<Option<Connection>> {
    let index = sidecar(path, ".seek.sqlite");
    if regular_metadata(&index)
        .with_context(|| format!("inspecting disk index {}", index.display()))?
        .is_none()
    {
        return Ok(None);
    }
    if let Some(auxiliary) = unexpected_sidecar(&index)? {
        anyhow::bail!("unexpected disk index sidecar: {}", auxiliary.display());
    }
    let Some(seal) = load_seal(&index)? else {
        return Ok(None);
    };
    let limit = crate::limits::record()?;
    // A seal built without line sizes cannot establish validity under this limit.
    if seal.max_line_bytes.is_none_or(|size| size > limit) {
        return Ok(None);
    }
    let file = open_regular(&index)?;
    if seal.base != fingerprint(base)? || seal.index != fingerprint(&file)? {
        return Ok(None);
    }
    let sqlite = Connection::open_with_flags(
        &index,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .with_context(|| format!("opening disk index {}", index.display()))?;
    sqlite.pragma_update(None, "trusted_schema", false)?;
    let saved: String = sqlite.query_row("SELECT fingerprint FROM meta", [], |row| row.get(0))?;
    Ok((saved == fingerprint(base)?).then_some(sqlite))
}

pub(crate) fn count(path: &Path, base: &File, table: &str) -> Result<Option<u64>> {
    let Some(sqlite) = open_current(path, base)? else {
        return Ok(None);
    };
    sqlite
        .query_row("SELECT 1 FROM schemas WHERE name = ?", [table], |_| Ok(()))
        .optional()?
        .ok_or_else(|| anyhow!("unknown table {table}"))?;
    // Base logical IDs are dense, and the index is only used without an update log.
    let last = sqlite
        .query_row(
            "SELECT id FROM offsets WHERE name = ? ORDER BY id DESC LIMIT 1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(Some(last.unwrap_or(0)))
}

pub(crate) fn locate(
    path: &Path,
    base: &File,
    table: &str,
    predicate: Option<&Predicate>,
    validate: impl FnOnce(&Database) -> Result<()>,
) -> Result<Option<Option<(u64, u64)>>> {
    let Some(predicate) = predicate else {
        return Ok(None);
    };
    let Some(sqlite) = open_current(path, base)? else {
        return Ok(None);
    };
    let columns: Option<String> = sqlite
        .query_row(
            "SELECT columns_json FROM schemas WHERE name = ?",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    let mut db = Database::default();
    if let Some(columns) = columns {
        db.apply(
            &Change::Schema {
                table: table.to_owned(),
                columns: serde_json::from_str(&columns)?,
            },
            Location::new("base", 0),
        )?;
    }
    validate(&db)?;
    let primary = db.primary_column(table)?;
    let Some((column, value)) = point(predicate, primary) else {
        return Ok(None);
    };
    let read_location =
        |row: &rusqlite::Row<'_>| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?));
    let location = if column == "_line" {
        let Some(id) = integral_literal(value).and_then(|value| i64::try_from(value).ok()) else {
            return Ok(Some(None));
        };
        sqlite
            .query_row(
                "SELECT id, offset FROM offsets WHERE name = ? AND id = ?",
                params![table, id],
                read_location,
            )
            .optional()?
    } else {
        let key = match primary.expect("point column is the primary key").kind {
            Kind::Int => integral_literal(value)
                .and_then(|key| i64::try_from(key).ok())
                .map(|key| key.to_string()),
            Kind::Text => value.is_string().then(|| value.to_string()),
            _ => unreachable!("schema validates primary key types"),
        };
        let Some(key) = key else {
            return Ok(Some(None));
        };
        sqlite
            .query_row(
                "SELECT id, offset FROM offsets WHERE name = ? AND pk = ?",
                params![table, key],
                read_location,
            )
            .optional()?
    };
    Ok(Some(location))
}

pub(crate) fn is_current(path: &Path, base: &File) -> Result<bool> {
    Ok(open_current(path, base)?.is_some())
}
