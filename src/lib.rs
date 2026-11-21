//! Embedded query execution for JSQNL files.
mod aggregate;
mod disk_index;
mod engine;
mod grace_join;
mod import;
mod import_json;
mod internal_json;
mod journal_auth;
mod limits;
mod parquet;
mod plan;
mod publish;
mod scratch;
mod sort;
mod storage;
mod syntax;

use anyhow::{Result, bail, ensure};
use engine::{Database, Location};
pub use import::{ImportFormat, ImportReport, import};
pub use plan::{AccessPath, QueryReport};
use serde_json::Value;
use std::path::Path;
use std::time::Instant;
use syntax::{Statement, parse};

/// Maximum UTF-8 length of a query batch or mixed script.
pub const QUERY_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

fn check_query_length(sql: &str) -> Result<()> {
    ensure!(
        sql.len() as u64 <= QUERY_LIMIT_BYTES,
        "query or script exceeds byte limit ({QUERY_LIMIT_BYTES})"
    );
    Ok(())
}

/// Create an empty base file without replacing an existing database.
pub fn init(path: impl AsRef<Path>) -> Result<()> {
    storage::init(path.as_ref())
}

/// Parse and execute SQL, sending JSON records to the caller as they become available.
///
/// A single read-only SELECT can emit records before it reaches the end of the base
/// file. For a write batch, emitting records starts after the transaction is durable.
/// A callback error stops execution but cannot undo an already committed transaction.
pub fn execute(
    path: impl AsRef<Path>,
    sql: &str,
    mut emit: impl FnMut(Value) -> Result<()>,
) -> Result<QueryReport> {
    let started = Instant::now();
    check_query_length(sql)?;
    let statements = parse(sql)?;
    if let [(offset, Statement::ExplainAnalyze(statement))] = statements.as_slice() {
        let mut report =
            execute_parsed(path.as_ref(), vec![(*offset, *statement.clone())], |_| {
                Ok(())
            })?;
        report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        emit(serde_json::to_value(&report)?)?;
        return Ok(report);
    }
    ensure!(
        !statements
            .iter()
            .any(|(_, stmt)| matches!(stmt, Statement::ExplainAnalyze(_))),
        "EXPLAIN ANALYZE must be the only statement in a batch"
    );
    let mut report = execute_parsed(path.as_ref(), statements, emit)?;
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

/// Run a single SELECT and return the actual plan and counters, discarding its rows.
pub fn explain_analyze(path: impl AsRef<Path>, sql: &str) -> Result<QueryReport> {
    let started = Instant::now();
    check_query_length(sql)?;
    let statements = parse(sql)?;
    if !matches!(statements.as_slice(), [(_, Statement::Select(..))]) {
        bail!("EXPLAIN ANALYZE requires one SELECT statement");
    }
    let mut report = execute_parsed(path.as_ref(), statements, |_| Ok(()))?;
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}

fn execute_parsed(
    path: &Path,
    statements: Vec<(u64, Statement)>,
    mut emit: impl FnMut(Value) -> Result<()>,
) -> Result<QueryReport> {
    let mut output = 0;
    let mut report = storage::query(path, statements, |row| {
        emit(row)?;
        output += 1;
        Ok(())
    })?;
    report.rows_output = output;
    Ok(report)
}

/// Evaluate a mixed schema, JSONL and SQL script in memory.
pub fn run_script(source: &str) -> Result<Vec<Value>> {
    use anyhow::Context;
    check_query_length(source)?;
    let mut db = Database::default();
    let limit = limits::materialized()?;
    db.limit_materialization(limit);
    let mut bytes = 0;
    let mut output = Vec::new();
    for (offset, statement) in parse(source)? {
        let (_, rows, _) = db
            .execute(statement, Location::new("script", offset))
            .with_context(|| format!("script byte {offset}"))?;
        for row in rows {
            limits::charge_result(&mut bytes, limit, &row)?;
            output.push(row);
        }
    }
    Ok(output)
}

/// Rebuild the byte-offset index used for point lookups on a log-free base file.
pub fn build_disk_index(path: impl AsRef<Path>) -> Result<()> {
    disk_index::build(path.as_ref())
}

/// Validate the base file and journal, reporting logical row counts and index status.
pub fn check(path: impl AsRef<Path>) -> Result<Value> {
    storage::check(path.as_ref())
}

/// Rebuild the inspectable JSON offset index.
pub fn rebuild_index(path: impl AsRef<Path>) -> Result<Value> {
    storage::rebuild_index(path.as_ref())
}

/// Fork a consistent snapshot into a new directory.
pub fn fork(path: impl AsRef<Path>, directory: impl AsRef<Path>) -> Result<()> {
    storage::fork(path.as_ref(), directory.as_ref())
}

/// Execute one SELECT against a Parquet directory using the external reader.
pub fn scan_parquet(
    directory: impl AsRef<Path>,
    sql: &str,
    emit: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    check_query_length(sql)?;
    parquet::scan(directory.as_ref(), sql, emit)
}

/// Remove orphaned query scratch directories in the current temporary root.
pub fn recover_temporary_files() -> Result<usize> {
    scratch::recover()
}

/// Render a diagnostic without literal terminal control characters.
#[doc(hidden)]
pub fn terminal_diagnostic(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    let mut safe = String::new();
    for character in message.chars() {
        if character.is_control()
            || matches!(character as u32, 0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069)
        {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}
