use jsqnl::{AccessPath, build_disk_index, execute, explain_analyze, init};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Temp(PathBuf);
static COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jsqnl-api-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn embedded_api_generates_rows_and_reports_actual_access() {
    let dir = Temp::new();
    let path = dir.0.join("db.jsqnl");
    init(&path).unwrap();
    fs::write(
        &path,
        concat!(
            "-- JSQNL: 1\n",
            "-- SCHEMA: records (id INT PRIMARY KEY)\n",
            "{\"id\":1}\n{\"id\":2}\n",
        ),
    )
    .unwrap();
    let mut rows = Vec::new();
    let report = execute(&path, "SELECT COUNT(*) FROM records;", |row| {
        rows.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, [json!({"count":2})]);
    assert_eq!(report.access, AccessPath::SequentialScan);
    assert_eq!(report.rows_read, 2);
    assert_eq!(report.rows_output, 1);
    assert_eq!(report.base_bytes_read, fs::metadata(&path).unwrap().len());
    build_disk_index(&path).unwrap();
    let indexed = explain_analyze(&path, "SELECT id FROM records WHERE id = 2;").unwrap();
    assert_eq!(indexed.access, AccessPath::IndexSeek);
    assert_eq!(indexed.rows_read, 1);
    assert_eq!(indexed.base_bytes_read, b"{\"id\":2}\n".len() as u64);
    assert_eq!(indexed.operators, ["index_seek", "filter", "project"]);
    let count = explain_analyze(&path, "SELECT COUNT(*) FROM records;").unwrap();
    assert_eq!(count.access, AccessPath::IndexCount);
    assert_eq!(count.base_bytes_read, 0);
    assert!(execute(&path, "EXPLAIN ANALYZE DELETE FROM records;", |_| Ok(())).is_err());
    execute(&path, "UPDATE records SET id = 3 WHERE id = 2;", |_| Ok(())).unwrap();
    let journal = explain_analyze(&path, "SELECT id FROM records WHERE id = 3;").unwrap();
    assert_eq!(journal.access, AccessPath::Materialized);
    assert!(journal.journal_bytes_read > 0);
}

#[test]
fn streaming_join_plan_reads_left_range_and_uses_only_needed_right_fields() {
    let dir = Temp::new();
    let path = dir.0.join("db.jsqnl");
    fs::write(
        &path,
        concat!(
            "-- SCHEMA: a (x INT PRIMARY KEY)\n",
            "  {\"x\":1}\n-- between rows\n    {\"x\":2}\n",
            "-- SCHEMA: b (y INT, text TEXT, unused JSON)\n",
            "{\"y\":2,\"text\":\"ok\",\"unused\":[1,2,3]}\n",
        ),
    )
    .unwrap();
    let mut rows = Vec::new();
    let stats = execute(
        &path,
        "SELECT a._line, b.text FROM a LEFT JOIN b ON a.x = b.y WHERE b.text IS NOT NULL;",
        |value| {
            rows.push(value);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(rows, [json!({"a._line":2,"b.text":"ok"})]);
    assert_eq!(stats.access, AccessPath::StreamingJoin);
    assert_eq!(stats.rows_read, 5);
    assert!(stats.base_bytes_read > fs::metadata(path).unwrap().len());
}

fn reference(path: &Path, table: &str) -> std::process::Output {
    let python = std::env::var_os("JSQNL_COMPAT_PYTHON").unwrap_or_else(|| "python3".into());
    Command::new(python)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference_reader.py"))
        .arg(path)
        .arg(table)
        .output()
        .expect("compatibility tests require an independent Python 3 interpreter")
}

#[test]
fn rust_and_reference_reader_agree_on_versioned_and_legacy_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (file, table) in [
        ("tests/fixtures/format-v1.jsqnl", "records"),
        ("examples/users.jsqnl", "users"),
    ] {
        let path = root.join(file);
        let output = reference(&path, table);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        let mut actual = Vec::new();
        execute(&path, &format!("SELECT * FROM {table};"), |row| {
            actual.push(row);
            Ok(())
        })
        .unwrap();
        assert_eq!(actual, expected);
        if table == "records" {
            assert_eq!(actual[0]["description"], json!("雪 🚀 \"引用\"\n\t\\"));
            assert_eq!(actual[0]["id"], json!(i64::MIN));
            assert_eq!(actual[1]["id"], json!(i64::MAX));
            assert!(actual[1].get("v").is_none());
        }
    }
}

#[test]
fn rust_and_reference_reader_reject_incompatible_files() {
    let dir = Temp::new();
    let path = dir.0.join("invalid.jsqnl");
    for source in [
        "-- JSQNL: 2\n-- SCHEMA: t (n INT)\n",
        "-- comment\n-- JSQNL: 1\n-- SCHEMA: t (n INT)\n",
        "-- SCHEMA: t (n VECTOR(2))\n{\"n\":[1]}\n",
        "-- SCHEMA: t (n INT)\n{\"n\":1,\"extra\":true}\n",
        "-- SCHEMA: t (n INT)\n{\"n\":-0}\n",
        "-- SCHEMA: t (n INT NOT NULL)\n{\"n\":null}\n",
        "-- SCHEMA: t (n INT PRIMARY KEY)\n{\"n\":1}\n{\"n\":1}\n",
        "-- SCHEMA: t (n JSON)\n{\"n\": 1e400}\n",
        "-- SCHEMA: t (n INT)\n{\"n\":1}\r \n",
    ] {
        fs::write(&path, source).unwrap();
        assert!(
            execute(&path, "SELECT * FROM t;", |_| Ok(())).is_err(),
            "{source}"
        );
        assert!(!reference(&path, "t").status.success(), "{source}");
    }
}

#[test]
fn query_callback_can_reenter_the_same_database() {
    let dir = Temp::new();
    let path = dir.0.join("data.jsqnl");
    fs::write(&path, "-- SCHEMA: t (n INT)\n{\"n\":1}\n{\"n\":2}\n").unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let first = execute(&path, "SELECT n FROM t;", |row| {
            if seen.is_empty() {
                execute(&path, "UPDATE t SET n = 20 WHERE n = 2;", |_| Ok(()))?;
            }
            seen.push(row);
            Ok(())
        });
        let mut stored = Vec::new();
        let second = execute(&path, "SELECT n FROM t;", |row| {
            stored.push(row);
            execute(&path, "SELECT COUNT(*) FROM t;", |_| Ok(()))?;
            Ok(())
        });
        send.send((first, second, seen, stored)).unwrap();
    });
    let (first, second, seen, stored) = recv
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    first.unwrap();
    second.unwrap();
    assert_eq!(seen, [json!({"n":1}), json!({"n":2})]);
    assert_eq!(stored, [json!({"n":1}), json!({"n":20})]);
    worker.join().unwrap();
}

#[test]
fn format_readers_agree_on_schema_comments_line_endings_and_finite_numbers() {
    let dir = Temp::new();
    let path = dir.0.join("db.jsqnl");
    for (contents, valid) in [
        (
            "-- SCHEMA: t (x INT) -- note (units)\r\n{\"x\":1}\r\n".to_owned(),
            true,
        ),
        ("-- SCHEMA: t (x INT)\r{\"x\":1}\r".to_owned(), false),
        (
            format!("-- SCHEMA: t (x FLOAT)\n{{\"x\":1{}}}\n", "0".repeat(309)),
            false,
        ),
    ] {
        fs::write(&path, contents).unwrap();
        let mut rows = Vec::new();
        let rust = execute(&path, "SELECT x FROM t;", |row| {
            rows.push(row);
            Ok(())
        });
        let python = reference(&path, "t");
        assert_eq!(rust.is_ok(), valid, "{rust:?}");
        assert_eq!(python.status.success(), valid, "{python:?}");
        if valid {
            assert_eq!(
                serde_json::from_slice::<Vec<Value>>(&python.stdout).unwrap(),
                rows
            );
        }
    }
}

#[test]
fn reference_reader_matches_schema_tokens_and_validates_overwritten_values() {
    let temp = Temp::new();
    let path = temp.0.join("rows");
    for (body, valid) in [
        ("SCHEMA t (K INT)\n{}", false),
        ("SCHEMA t (n ſTRING)\n{}", false),
        ("SCHEMA t (n VECTOR(01))\n{}", false),
        ("SCHEMA t (n VECTOR(18446744073709551616))\n{}", false),
        ("SCHEMA t (n VECTOR(2)NOT NULL)\n{\"n\":[1,2]}", true),
        ("SCHEMA t (n JSON)\n{\"n\":[\"\\ud800\"],\"n\":1}", false),
        ("SCHEMA t (n INT)\n{\"n\":\r1}", false),
        ("SCHEMA t (n TEXT) -- ordinary note\n{\"n\":\"雪\"}", true),
    ] {
        fs::write(&path, body).unwrap();
        assert_eq!(
            execute(&path, "SELECT * FROM t;", |_| Ok(())).is_ok(),
            valid,
            "{body}"
        );
        let python = std::env::var_os("JSQNL_COMPAT_PYTHON").unwrap_or_else(|| "python3".into());
        let read = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference_reader.py"))
            .arg(&path)
            .arg("t")
            .env("PYTHONIOENCODING", "ascii")
            .output()
            .unwrap();
        assert_eq!(read.status.success(), valid, "{body}: {read:?}");
        if valid {
            assert!(serde_json::from_slice::<Vec<Value>>(&read.stdout).is_ok());
        }
    }
}

#[test]
fn parquet_scan_propagates_reader_stderr_in_error() {
    let dir = Temp::new();
    let table_dir = dir.0.join("items");
    fs::create_dir(&table_dir).unwrap();
    let error = jsqnl::scan_parquet(&table_dir, "SELECT * FROM items;", |_| Ok(()))
        .expect_err("empty parquet directory or missing pyarrow must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("Parquet:"),
        "expected reader stderr details in error, got: {message}"
    );
}
