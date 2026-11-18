use anyhow::{Context, Result, bail, ensure};
use std::env;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

const USAGE: &str = "JSQNL 0.1.0
Usage:
  jsqnl init DATABASE.jsqnl
  jsqnl run SCRIPT.jsqnl
  jsqnl exec DATABASE.jsqnl [SQL | -]
  jsqnl index DATABASE.jsqnl
  jsqnl disk-index DATABASE.jsqnl
  jsqnl check DATABASE.jsqnl
  jsqnl scan PARQUET_DIRECTORY SQL
  jsqnl fork DATABASE.jsqnl NEW_DIRECTORY

exec reads statements from stdin when SQL is omitted or is '-'.
run evaluates a mixed SCHEMA, JSONL and SQL script in memory.
SELECT outputs JSON Lines.";

fn output(rows: impl IntoIterator<Item = serde_json::Value>) -> Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    for row in rows {
        serde_json::to_writer(&mut out, &row)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn write_query(
    run: impl FnOnce(&mut dyn FnMut(serde_json::Value) -> Result<()>) -> Result<()>,
) -> Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    run(&mut |value| {
        serde_json::to_writer(&mut out, &value)?;
        out.write_all(b"\n")?;
        Ok(())
    })?;
    out.flush()?;
    Ok(())
}

fn read_source(input: impl Read) -> Result<String> {
    let mut source = String::new();
    input
        .take(jsqnl::QUERY_LIMIT_BYTES + 1)
        .read_to_string(&mut source)?;
    ensure!(
        source.len() as u64 <= jsqnl::QUERY_LIMIT_BYTES,
        "query or script exceeds byte limit"
    );
    Ok(source)
}

fn cli() -> Result<()> {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    let command = args.first().and_then(|arg| arg.to_str());
    match (command, &args[1.min(args.len())..]) {
        (None | Some("-h" | "--help" | "help"), []) => println!("{USAGE}"),
        (Some("-V" | "--version"), []) => println!("jsqnl {}", env!("CARGO_PKG_VERSION")),
        (Some("init"), [path]) => jsqnl::init(Path::new(path))?,
        (Some("run"), [path]) => {
            let source =
                read_source(fs::File::open(path)?).with_context(|| format!("reading {path:?}"))?;
            output(jsqnl::run_script(&source)?)?;
        }
        (Some("exec"), [path] | [path, _]) => {
            let source = if let Some(sql) = args.get(2).filter(|arg| *arg != "-") {
                sql.to_str().context("SQL must be UTF-8")?.to_owned()
            } else {
                read_source(io::stdin().lock())?
            };
            write_query(|emit| jsqnl::execute(Path::new(path), &source, emit).map(|_| ()))?;
        }
        (Some("scan"), [path, sql]) => {
            let sql = sql.to_str().context("SQL must be UTF-8")?;
            write_query(|emit| jsqnl::scan_parquet(Path::new(path), sql, emit))?;
        }
        (Some("fork"), [path, directory]) => jsqnl::fork(Path::new(path), Path::new(directory))?,
        (Some("check"), [path]) => output([jsqnl::check(Path::new(path))?])?,
        (Some("disk-index"), [path]) => jsqnl::build_disk_index(Path::new(path))?,
        (Some("index"), [path]) => output([jsqnl::rebuild_index(Path::new(path))?])?,
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn main() {
    if let Err(error) = jsqnl::recover_temporary_files().and_then(|_| cli()) {
        eprintln!("jsqnl: {}", jsqnl::terminal_diagnostic(&error));
        std::process::exit(1);
    }
}
