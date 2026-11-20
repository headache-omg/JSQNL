use crate::engine::{Change, Database, Location, Row};
use crate::import_json::StrictValue;
use crate::scratch::{KeyValidation, ScratchSlot};
use crate::storage::{open_regular, private_options, sidecar, sync_parent};
use crate::syntax::{Kind, parse_import_tables};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SQL_LIMIT: u64 = 4 * 1024 * 1024;

/// Serialization used by the input data file.
#[derive(Clone, Copy, Debug)]
pub enum ImportFormat {
    Json,
    Jsonl,
}

/// Counts and destination table for a completed conversion.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub table: String,
    pub rows: u64,
}

struct Stage(PathBuf);

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn same_input(before: &Metadata, after: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && before.created().ok() == after.created().ok()
    }
}

fn unchanged(path: &Path, file: &File, before: &Metadata) -> Result<()> {
    let named =
        fs::symlink_metadata(path).with_context(|| format!("checking input {}", path.display()))?;
    ensure!(
        named.is_file() && same_input(before, &file.metadata()?) && same_input(before, &named),
        "input changed during conversion: {}",
        path.display()
    );
    Ok(())
}

/// Combine one table definition from a SQL DDL file with JSON records.
/// Publishes a versioned base file only after all rows have been validated.
pub fn import(
    schema: impl AsRef<Path>,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    table: Option<&str>,
    format: ImportFormat,
) -> Result<ImportReport> {
    let schema = schema.as_ref();
    let input = input.as_ref();
    let output = output.as_ref();
    let mut sql = String::new();
    let mut schema_file =
        open_regular(schema).with_context(|| format!("opening SQL schema {}", schema.display()))?;
    let schema_before = schema_file.metadata()?;
    (&mut schema_file)
        .take(SQL_LIMIT + 1)
        .read_to_string(&mut sql)?;
    unchanged(schema, &schema_file, &schema_before)?;
    ensure!(
        sql.len() as u64 <= SQL_LIMIT,
        "SQL definition exceeds byte limit"
    );

    let mut tables = parse_import_tables(&sql)?;
    let table = match table {
        Some(name) => tables
            .into_iter()
            .find(|table| table.name == name)
            .with_context(|| format!("SQL definition has no table {name}"))?,
        None if tables.len() == 1 => tables.pop().expect("exactly one table"),
        None => bail!("SQL definition has multiple tables; specify --table"),
    };
    let mut db = Database::default();
    db.apply(
        &Change::Schema {
            table: table.name.clone(),
            columns: table.columns.clone(),
        },
        Location::new("base", 0),
    )?;
    let source =
        open_regular(input).with_context(|| format!("opening input {}", input.display()))?;
    let input_before = source.metadata()?;
    let mut reader = BufReader::new(&source);
    let has_bom = reader.fill_buf()?.starts_with(b"\xef\xbb\xbf");
    if has_bom {
        reader.consume(3);
    }

    for suffix in ["", ".log", ".idx", ".seek.sqlite", ".seek.sqlite.seal"] {
        let path = sidecar(output, suffix);
        match fs::symlink_metadata(&path) {
            Ok(_) => bail!("output or companion already exists: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staged = sidecar(
        output,
        &format!(".{}.{nonce}.{sequence}.import", std::process::id()),
    );
    let file = private_options().create_new(true).open(&staged)?;
    let stage = Stage(staged);
    let mut writer = BufWriter::with_capacity(128 * 1024, file);
    let limit = crate::limits::record()?;
    let definitions = table
        .columns
        .iter()
        .map(|column| {
            let kind = match column.kind {
                Kind::Int => "INT".into(),
                Kind::Float => "FLOAT".into(),
                Kind::Text => "TEXT".into(),
                Kind::Bool => "BOOL".into(),
                Kind::Json => "JSON".into(),
                Kind::Vector(size) => format!("VECTOR({size})"),
            };
            let constraint = if column.primary {
                " PRIMARY KEY"
            } else if column.required {
                " NOT NULL"
            } else {
                ""
            };
            format!("{} {kind}{constraint}", column.name)
        })
        .collect::<Vec<_>>();
    let header = format!("-- SCHEMA: {} ({})\n", table.name, definitions.join(", "));
    ensure!(
        header.len() as u64 <= limit,
        "generated schema exceeds record byte limit"
    );
    writeln!(writer, "-- JSQNL: 1")?;
    writer.write_all(header.as_bytes())?;

    let mut keys = table
        .columns
        .iter()
        .any(|column| column.primary)
        .then(|| KeyValidation::exclusive(ScratchSlot::default()));
    let mut row_number = 0;
    let mut write_row = |row: Row| -> Result<()> {
        row_number += 1;
        (|| -> Result<()> {
            crate::limits::measure(&row, limit - 1)?;
            let (name, id, key) = db.register_base_row(&row, false, keys.is_some())?;
            if let (Some(keys), Some(key)) = (&mut keys, key) {
                keys.insert(name, id, &key)?;
            }
            for (name, width) in &table.widths {
                if let Some(value) = row.get(name).and_then(serde_json::Value::as_str) {
                    ensure!(
                        value.chars().count() as u64 <= *width,
                        "column {name} exceeds character width {width}"
                    );
                }
            }
            serde_json::to_writer(&mut writer, &row)?;
            writer.write_all(b"\n")?;
            Ok(())
        })()
        .with_context(|| format!("record {row_number}"))
    };
    match format {
        ImportFormat::Jsonl => {
            let mut line = String::new();
            let mut number = 1;
            while crate::limits::line(
                &mut reader,
                &mut line,
                if number == 1 && has_bom {
                    limit - 3
                } else {
                    limit
                },
            )
            .with_context(|| format!("JSONL line {number}"))?
                != 0
            {
                if !line.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
                    let value = serde_json::from_str::<StrictValue>(&line)
                        .with_context(|| format!("JSONL line {number}"))?;
                    let row = into_row(value).with_context(|| format!("JSONL line {number}"))?;
                    write_row(row)?;
                }
                line.clear();
                number += 1;
            }
        }
        ImportFormat::Json => {
            read_json(reader, limit, &mut write_row).context("reading JSON input")?
        }
    }
    if let Some(keys) = keys {
        keys.finish()?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    unchanged(schema, &schema_file, &schema_before)?;
    unchanged(input, &source, &input_before)?;
    crate::publish::rename_new(&stage.0, output)
        .with_context(|| format!("publishing {}", output.display()))?;

    sync_parent(output).with_context(|| {
        format!(
            "output was published at {}, but directory sync failed; durability is unconfirmed",
            output.display()
        )
    })?;
    Ok(ImportReport {
        table: table.name,
        rows: row_number,
    })
}

fn next_json_byte(reader: &mut impl BufRead) -> Result<Option<u8>> {
    loop {
        let pending = reader.fill_buf()?;
        if pending.is_empty() {
            return Ok(None);
        }
        let skip = pending
            .iter()
            .take_while(|&&byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
            .count();
        let next = pending.get(skip).copied();
        reader.consume(skip);
        if next.is_some() {
            return Ok(next);
        }
    }
}

fn into_row(value: StrictValue) -> Result<Row> {
    match value.0 {
        serde_json::Value::Object(row) => Ok(row),
        _ => bail!("expected a JSON object"),
    }
}

fn read_json_row(reader: &mut impl BufRead, limit: u64) -> Result<Row> {
    ensure!(
        next_json_byte(reader)? == Some(b'{'),
        "expected a JSON object"
    );
    let bound = limit.saturating_add(1);
    let mut source = reader.take(bound);
    let result = StrictValue::deserialize(&mut serde_json::Deserializer::from_reader(&mut source));
    ensure!(
        bound - source.limit() <= limit,
        "JSON record exceeds byte limit"
    );
    into_row(result?)
}

fn read_json(
    mut reader: impl BufRead,
    limit: u64,
    output: &mut impl FnMut(Row) -> Result<()>,
) -> Result<()> {
    if next_json_byte(&mut reader)? == Some(b'[') {
        reader.consume(1);
        if next_json_byte(&mut reader)? == Some(b']') {
            reader.consume(1);
        } else {
            loop {
                output(read_json_row(&mut reader, limit)?)?;
                let next = next_json_byte(&mut reader)?;
                match next {
                    Some(b',') => reader.consume(1),
                    Some(b']') => {
                        reader.consume(1);
                        break;
                    }
                    _ => bail!("expected ',' or ']' after JSON object"),
                }
            }
        }
    } else {
        output(read_json_row(&mut reader, limit)?)?;
    }
    ensure!(
        next_json_byte(&mut reader)?.is_none(),
        "trailing content after JSON value"
    );
    Ok(())
}
