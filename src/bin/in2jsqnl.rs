use anyhow::{Context, Result, bail, ensure};
use jsqnl::{ImportFormat, import};
use std::ffi::OsString;
use std::path::PathBuf;

const USAGE: &str = "IN2JSQNL
Usage: in2jsqnl --schema SCHEMA.sql --input DATA --output DATABASE.jsqnl [--table TABLE] [--format json|jsonl]

Auto-detects JSONL from .jsonl and .ndjson extensions; otherwise reads JSON.";

fn set(slot: &mut Option<OsString>, key: &str, value: OsString) -> Result<()> {
    ensure!(slot.replace(value).is_none(), "duplicate {key}");
    Ok(())
}

fn run() -> Result<()> {
    jsqnl::recover_temporary_files()?;
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_none_or(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let (mut schema, mut input, mut output, mut table, mut format) = (None, None, None, None, None);
    while let Some(arg) = args.next() {
        let flag = arg.to_str().context("option must be UTF-8")?;
        let target = match flag {
            "--schema" => &mut schema,
            "--input" => &mut input,
            "--output" => &mut output,
            "--table" => &mut table,
            "--format" => &mut format,
            _ => bail!("unknown option {flag}\n{USAGE}"),
        };
        set(
            target,
            flag,
            args.next()
                .with_context(|| format!("missing value for {flag}"))?,
        )?;
    }
    let schema = PathBuf::from(schema.context("--schema is required")?);
    let input = PathBuf::from(input.context("--input is required")?);
    let output = PathBuf::from(output.context("--output is required")?);
    let table = table
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("table name must be UTF-8"))
        })
        .transpose()?;
    let format = match format.as_deref().and_then(|value| value.to_str()) {
        Some("json") => ImportFormat::Json,
        Some("jsonl") => ImportFormat::Jsonl,
        None if format.is_none() => match input.extension().and_then(|value| value.to_str()) {
            Some(ext)
                if ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("ndjson") =>
            {
                ImportFormat::Jsonl
            }
            _ => ImportFormat::Json,
        },
        _ => bail!("expected --format json or jsonl"),
    };
    let imported = import(schema, input, output, table.as_deref(), format)?;
    println!("{}", serde_json::to_string(&imported)?);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("in2jsqnl: {}", jsqnl::terminal_diagnostic(&error));
        std::process::exit(1);
    }
}
