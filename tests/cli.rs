use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);
const BASE: &str = concat!(
    "-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, tags JSON, v VECTOR(2))\n",
    "{\"id\":10,\"name\":\"雪\",\"age\":16,\"tags\":[],\"v\":[1,0]}\n",
    "{\"id\":20,\"name\":\"Taro\",\"age\":20,\"v\":[0,1]}\n",
);

struct Fixture(PathBuf);

impl Fixture {
    fn new(base: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "jsqnl-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let fixture = Self(dir);
        if let Some(base) = base {
            fs::write(fixture.file("db.jsqnl"), base).unwrap();
        }
        fixture
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn call(&self, command: &str, sql: Option<&str>) -> Output {
        command_for(command, &self.file("db.jsqnl"))
            .args(sql)
            .output()
            .unwrap()
    }

    fn exec(&self, sql: &str) -> Vec<Value> {
        rows(self.call("exec", Some(sql)))
    }

    fn reject(&self, sql: &str, expected: &str) {
        let output = self.call("exec", Some(sql));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{stderr}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command_for(command: &str, path: &Path) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_jsqnl"));
    process.arg(command).arg(path);
    process
}

fn rows(output: Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn mixed_script_and_jsonl_are_executable() {
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo.jsqnl");
    let output = rows(command_for("run", &sample).output().unwrap());
    assert_eq!(
        output,
        [
            json!({"_line": 1, "name": "Yuki", "age": 18}),
            json!({"_line": 3, "name": "Mika O'Neil", "age": 21}),
        ]
    );
}

#[test]
fn init_and_stdin_create_a_persistent_database() {
    let db = Fixture::new(None);
    assert!(db.call("init", None).status.success());
    assert!(!db.call("init", None).status.success());
    let mut process = command_for("exec", &db.file("db.jsqnl"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(b"SCHEMA words (text TEXT NOT NULL);\nINSERT INTO words VALUES ('hello');\n")
        .unwrap();
    assert!(process.wait().unwrap().success());
    assert_eq!(
        db.exec("SELECT _line, text FROM words;"),
        [json!({"_line": 1, "text": "hello"})]
    );
}

#[test]
fn crud_replay_retains_ids_and_rebuilds_byte_offsets() {
    let db = Fixture::new(Some(BASE));
    let index = rows(db.call("index", None)).remove(0);
    let original = index["tables"]["users"]["2"]["offset"].as_u64().unwrap() as usize;
    assert_eq!(&BASE.as_bytes()[original..original + 8], b"{\"id\":20");
    db.exec(
        "DELETE FROM users WHERE id = 10; INSERT INTO users VALUES {\"id\":30,\"name\":\"Mika\"};",
    );
    db.exec("UPDATE users SET age = 22 WHERE id = 20; INJECT age FROM users THE 2 WHO + 1;");
    db.reject(
        "INJECT age FROM users THE 1 WHO + 1;",
        "unknown logical row ID 1",
    );
    assert_eq!(
        db.exec("SELECT _line, age FROM users;"),
        [json!({"_line":2,"age":23}), json!({"_line":3,"age":null})]
    );
    assert_eq!(fs::read_to_string(db.file("db.jsqnl")).unwrap(), BASE);
    let saved: Value = serde_json::from_slice(&fs::read(db.file("db.jsqnl.idx")).unwrap()).unwrap();
    let loc = &saved["tables"]["users"]["2"];
    let log = fs::read_to_string(db.file("db.jsqnl.log")).unwrap();
    let start = loc["offset"].as_u64().unwrap() as usize;
    let (_, payload) = log[start..]
        .lines()
        .next()
        .unwrap()
        .split_once('\t')
        .unwrap();
    let transaction: Value = serde_json::from_str(payload).unwrap();
    let position = loc["change"].as_u64().unwrap() as usize;
    assert_eq!(transaction["changes"][position]["Update"]["row"]["age"], 23);
    fs::write(db.file("db.jsqnl.idx"), b"invalid cache").unwrap();
    assert_eq!(rows(db.call("index", None)), [saved]);
}

#[test]
fn schema_enforced_for_all_write_paths_and_whole_batch_rolls_back() {
    let db = Fixture::new(Some(BASE));
    db.reject(
        "INSERT INTO users (id, name) VALUES (10, 'dup');",
        "duplicate PRIMARY KEY",
    );
    db.reject(
        "INSERT INTO users (id) VALUES (30);",
        "invalid value for column name",
    );
    db.reject("UPDATE users SET id = 30;", "duplicate PRIMARY KEY");
    db.reject("UPDATE users SET v = [2];", "invalid value for column v");
    db.reject(
        "INJECT v FROM users THE 1 WHO + [3];",
        "requires compatible operands",
    );
    db.reject(
        "INJECT v FROM users THE 1 WHO = [1, true];",
        "invalid value for column v",
    );
    db.reject("INJECT age FROM users THE 1 WHO / 0;", "division by zero");
    db.reject(
        "INJECT age FROM users THE 1 WHO / 3;",
        "non-integral division",
    );
    db.reject(
        "INJECT age FROM users THE 1 WHO + 9223372036854775807;",
        "invalid value for column age",
    );
    db.reject(
        "DELETE FROM users; SELECT _line FROM users; SELECT bad FROM users;",
        "unknown column bad",
    );
    db.reject(
        "SELECT bad FROM users WHERE id = 999;",
        "unknown column bad",
    );
    db.reject(
        "DELETE FROM users WHERE id = 999 AND bad = 0;",
        "unknown column bad",
    );
    assert!(!db.file("db.jsqnl.log").exists());
    assert_eq!(
        db.exec("SELECT id FROM users;"),
        [json!({"id":10}), json!({"id":20})]
    );
    db.exec(
        "INJECT tags FROM users THE 1 WHO + [\"a\",\"b\"]; INJECT v FROM users THE 1 WHO = [1,-2];",
    );
    assert_eq!(
        db.exec("SELECT tags, v FROM users WHERE _line = 1;"),
        [json!({"tags":["a","b"],"v":[1,-2]})]
    );
}

#[test]
fn predicates_and_exact_integer_comparisons() {
    let db = Fixture::new(Some(BASE));
    db.exec("INSERT INTO users (id, name, age) VALUES (9007199254740993, 'A; -- data', -2);");
    assert_eq!(
        db.exec("select name from users where age >= 20 OR (age < 0 AND id > 9007199254740992.0);"),
        [json!({"name":"Taro"}), json!({"name":"A; -- data"})]
    );
    assert!(
        db.exec("SELECT id FROM users WHERE id = 9007199254740992.0;")
            .is_empty()
    );
    db.reject("SELECT id FROM users", "expected ';'");
    db.reject("SELECT * FROM users WHERE id = 3e+;", "byte");
}

#[test]
fn torn_tail_recovers_and_corruption_fails_closed() {
    let db = Fixture::new(Some(BASE));
    db.exec("INJECT age FROM users THE 1 WHO + 1;");
    let path = db.file("db.jsqnl.log");
    let committed = fs::read(&path).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    assert_eq!(
        db.exec("SELECT age FROM users WHERE _line = 1;"),
        [json!({"age":17})]
    );
    db.exec("INJECT age FROM users THE 1 WHO + 1;");
    assert_eq!(
        db.exec("SELECT age FROM users WHERE _line = 1;"),
        [json!({"age":18})]
    );
    let mut corrupt = fs::read(&path).unwrap();
    assert!(corrupt.starts_with(&committed));
    corrupt[committed.len() + 65] ^= 1;
    fs::write(path, corrupt).unwrap();
    db.reject("SELECT * FROM users;", "checksum mismatch");
}

#[test]
fn changing_base_file_after_commit_is_rejected() {
    let db = Fixture::new(Some(BASE));
    db.exec("DELETE FROM users WHERE _line = 1;");
    fs::write(db.file("db.jsqnl"), format!("-- edit\n{BASE}")).unwrap();
    db.reject("SELECT * FROM users;", "base changed");
}

#[test]
fn concurrent_read_modify_write_serializes() {
    let db = Fixture::new(Some(BASE));
    let processes: Vec<_> = (0..12)
        .map(|_| {
            command_for("exec", &db.file("db.jsqnl"))
                .arg("INJECT age FROM users THE 1 WHO + 1;")
                .spawn()
                .unwrap()
        })
        .collect();
    for mut process in processes {
        assert!(process.wait().unwrap().success());
    }
    assert_eq!(
        db.exec("SELECT age FROM users WHERE _line = 1;"),
        [json!({"age":28})]
    );
}

#[test]
fn base_validation_checks_jsonl_and_keeps_tables_separate() {
    let db = Fixture::new(Some(&format!(
        "{BASE}-- SCHEMA: other (id INT PRIMARY KEY)\n{{\"id\":10}}\n"
    )));
    assert_eq!(
        db.exec("SELECT _line, id FROM other;"),
        [json!({"_line":1,"id":10})]
    );
    let invalid = Fixture::new(Some("-- SCHEMA: users (id INT)\n{\n\"id\": 1\n}\n"));
    invalid.reject("SELECT * FROM users;", "one object per physical line");
    let invalid = Fixture::new(Some(
        "-- SCHEMA: users (id INT)\n{\"id\": 1, \"unexpected\": >}\n",
    ));
    invalid.reject("SELECT * FROM users;", "expected value");
    let invalid = Fixture::new(Some(
        "-- SCHEMA: users (id INT)\n{\"id\": 1, \"unexpected\": true}\n",
    ));
    invalid.reject("SELECT * FROM users;", "unknown column unexpected");
}

#[test]
fn resonate_ranks_persisted_vectors_and_handles_degenerate_inputs() {
    let db = Fixture::new(Some(BASE));
    db.exec(concat!(
        "INSERT INTO users (id, name, v) VALUES (30, 'same', [1e300,0]);",
        "INSERT INTO users (id, name, v) VALUES (40, 'zero', [0,0]);",
        "INSERT INTO users (id, name) VALUES (50, 'missing');",
        "INSERT INTO users (id, name, v) VALUES (60, 'opposite', [-1,0]);",
    ));
    assert_eq!(
        db.exec("RESONATE v FROM users WITH [1e-300,0] LIMIT 10;"),
        [
            json!({"_line":1,"_score":1.0}),
            json!({"_line":3,"_score":1.0}),
            json!({"_line":2,"_score":0.0}),
            json!({"_line":6,"_score":-1.0}),
        ]
    );
    assert_eq!(
        db.exec("RESONATE v FROM users WITH [1,0] WHERE id >= 20 LIMIT 1;"),
        [json!({"_line":3,"_score":1.0})]
    );
    db.reject(
        "RESONATE v FROM users WITH [0,0] LIMIT 1;",
        "nonzero VECTOR(2)",
    );
    db.reject(
        "RESONATE v FROM users WITH [1] LIMIT 1;",
        "nonzero VECTOR(2)",
    );
    db.reject(
        "RESONATE age FROM users WITH [1,0] LIMIT 1;",
        "requires a VECTOR column",
    );
    db.reject(
        "RESONATE v FROM users WITH [1,0] LIMIT 0;",
        "positive integer",
    );
}

#[test]
fn decay_scales_selected_values_and_replays_atomically() {
    let db = Fixture::new(Some(concat!(
        "-- SCHEMA: samples (weight FLOAT, v VECTOR(2), id INT)\n",
        "{\"weight\":8,\"v\":[3,-4],\"id\":1}\n",
        "{\"weight\":null,\"id\":2}\n",
        "{\"weight\":5,\"v\":[0,1],\"id\":3}\n",
    )));
    db.exec(
        "DECAY weight FROM samples BY 0.25 WHERE id < 3; DECAY v FROM samples BY 0.5 WHERE id < 3;",
    );
    assert_eq!(
        db.exec("SELECT * FROM samples;"),
        [
            json!({"weight":2.0,"v":[1.5,-2.0],"id":1}),
            json!({"weight":null,"id":2}),
            json!({"weight":5,"v":[0,1],"id":3}),
        ]
    );
    let log = fs::read(db.file("db.jsqnl.log")).unwrap();
    db.exec("DECAY weight FROM samples BY 1;");
    db.reject(
        "DECAY v FROM samples BY 0; DECAY weight FROM samples BY 1.1;",
        "between 0 and 1",
    );
    db.reject(
        "DECAY id FROM samples BY 0.5;",
        "requires a FLOAT or VECTOR column",
    );
    assert_eq!(fs::read(db.file("db.jsqnl.log")).unwrap(), log);
    db.exec("DECAY v FROM samples BY 0 WHERE id = 1;");
    assert_eq!(
        db.exec("RESONATE v FROM samples WITH [0,1] LIMIT 5;"),
        [json!({"_line":3,"_score":1.0}),]
    );
}

#[test]
fn fuse_fills_missing_values_and_preserves_target_id_and_key() {
    let db = Fixture::new(Some(BASE));
    db.reject("FUSE users THE 1 INTO THE 2;", "conflict in column name");
    db.reject("FUSE users THE 1 INTO THE 1;", "distinct logical row IDs");
    assert!(!db.file("db.jsqnl.log").exists());
    db.exec("FUSE users THE 1 INTO THE 2 KEEP TARGET;");
    assert_eq!(
        db.exec("SELECT _line, id, name, tags, v FROM users;"),
        [json!({"_line":2,"id":20,"name":"Taro","tags":[],"v":[0,1]}),]
    );
    db.reject(
        "INJECT age FROM users THE 1 WHO + 1;",
        "unknown logical row ID 1",
    );
}

#[test]
fn fork_copies_only_committed_history_and_keeps_branches_independent() {
    let db = Fixture::new(Some(BASE));
    db.exec("DELETE FROM users WHERE _line = 1; INJECT age FROM users THE 2 WHO + 1;");
    let log = db.file("db.jsqnl.log");
    let committed = fs::read(&log).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(log)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    let branch = db.file("branch");
    let fork = || {
        command_for("fork", &db.file("db.jsqnl"))
            .arg(&branch)
            .output()
            .unwrap()
    };
    let result = fork();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fork().status.success());
    assert_eq!(fs::read(branch.join("db.jsqnl.log")).unwrap(), committed);
    let child = branch.join("db.jsqnl");
    let on_branch = |sql| rows(command_for("exec", &child).arg(sql).output().unwrap());
    assert_eq!(
        on_branch("SELECT _line, age FROM users;"),
        [json!({"_line":2,"age":21})]
    );
    on_branch("INSERT INTO users (id,name) VALUES (30,'branch');");
    db.exec("INJECT age FROM users THE 2 WHO + 1;");
    assert_eq!(
        on_branch("SELECT _line, age FROM users;"),
        [json!({"_line":2,"age":21}), json!({"_line":3,"age":null}),]
    );
    assert_eq!(
        db.exec("SELECT _line, age FROM users;"),
        [json!({"_line":2,"age":22})]
    );
}

#[test]
fn check_reports_log_and_index_health_without_mutating_files() {
    let db = Fixture::new(Some(BASE));
    let check = || rows(db.call("check", None)).remove(0);
    assert_eq!(
        check(),
        json!({
            "committed_log_bytes": 0, "uncommitted_log_bytes": 0,
            "index": "missing", "rows_by_table": {"users": 2},
        })
    );
    let old_index = rows(db.call("index", None)).remove(0);
    db.exec("DELETE FROM users WHERE _line = 1;");
    let log_path = db.file("db.jsqnl.log");
    let committed = fs::metadata(&log_path).unwrap().len();
    assert_eq!(check()["index"], "current");
    fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    let index_path = db.file("db.jsqnl.idx");
    let stale = serde_json::to_vec(&old_index).unwrap();
    fs::write(&index_path, &stale).unwrap();
    assert_eq!(
        check(),
        json!({
            "committed_log_bytes": committed, "uncommitted_log_bytes": 7,
            "index": "stale", "rows_by_table": {"users": 1},
        })
    );
    assert_eq!(fs::metadata(&log_path).unwrap().len(), committed + 7);
    assert_eq!(fs::read(&index_path).unwrap(), stale);
    fs::write(&index_path, b"corrupt").unwrap();
    assert_eq!(check()["index"], "invalid");
    let mut corrupt_log = fs::read(&log_path).unwrap();
    corrupt_log[0] ^= 1;
    fs::write(log_path, corrupt_log).unwrap();
    let result = db.call("check", None);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("checksum mismatch"));
}

#[test]
fn indexed_equalities_preserve_predicate_and_mutation_semantics() {
    let db = Fixture::new(Some(BASE));
    assert_eq!(
        db.exec("SELECT _line FROM users WHERE id = 20.0 AND age >= 18;"),
        [json!({"_line":2}),]
    );
    assert!(
        db.exec("SELECT _line FROM users WHERE _line = 2.5;")
            .is_empty()
    );
    assert!(
        db.exec("SELECT _line FROM users WHERE id = 20 AND _line = 1;")
            .is_empty()
    );
    assert_eq!(
        db.exec("SELECT _line FROM users WHERE _line = 1 OR id = 20;"),
        [json!({"_line":1}), json!({"_line":2}),]
    );
    db.reject(
        "SELECT _line FROM users WHERE id = 999 AND unknown = 2;",
        "unknown column unknown",
    );
    db.exec("UPDATE users SET id = 30 WHERE _line = 2.0; DELETE FROM users WHERE id = 10.0;");
    assert!(db.exec("SELECT _line FROM users WHERE id = 20;").is_empty());
    assert_eq!(
        db.exec("SELECT _line FROM users WHERE id = 30;"),
        [json!({"_line":2})]
    );
    let text = Fixture::new(Some(
        "-- SCHEMA: labels (key TEXT PRIMARY KEY)\n{\"key\":\"雪\"}\n",
    ));
    assert_eq!(
        text.exec("SELECT _line FROM labels WHERE key = '雪';"),
        [json!({"_line":1})]
    );
    assert!(
        text.exec("SELECT _line FROM labels WHERE key = 1;")
            .is_empty()
    );
}

#[test]
#[ignore = "requires a Python with PyArrow; set JSQNL_PYTHON"]
fn scan_queries_all_parquet_shards_in_filename_order() {
    let fixture = Fixture::new(None);
    let directory = fixture.file("articles");
    fs::create_dir(&directory).unwrap();
    let python = std::env::var_os("JSQNL_PYTHON").unwrap_or_else(|| "python3".into());
    let setup = Command::new(python)
        .arg("-c")
        .arg(concat!(
            "import sys, pathlib, pyarrow as pa, pyarrow.parquet as pq\n",
            "path = pathlib.Path(sys.argv[1])\n",
            "pq.write_table(pa.table({'title': ['後'], 'rank': [2]}), path / '002.parquet')\n",
            "pq.write_table(pa.table({'title': ['先'], 'rank': [1]}), path / '001.parquet')\n",
        ))
        .arg(&directory)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let scan = |sql| command_for("scan", &directory).arg(sql).output().unwrap();
    assert_eq!(
        rows(scan("SELECT _line, title FROM articles WHERE rank >= 1;")),
        [
            json!({"_line":1,"title":"先"}),
            json!({"_line":2,"title":"後"}),
        ]
    );
    assert_eq!(
        rows(scan("SELECT _line FROM articles WHERE _line = 2;")),
        [json!({"_line":2})]
    );
    assert_eq!(
        rows(scan(
            "SELECT COUNT(*) AS total FROM articles WHERE rank BETWEEN 1 AND 2;"
        )),
        [json!({"total":2})]
    );
    assert_eq!(
        rows(scan(
            "SELECT title FROM articles ORDER BY rank DESC LIMIT 1;"
        )),
        [json!({"title":"後"})]
    );
    assert_eq!(
        rows(scan(
            "SELECT rank, COUNT(*) AS total FROM articles GROUP BY rank ORDER BY rank DESC;"
        )),
        [json!({"rank":2,"total":1}), json!({"rank":1,"total":1})]
    );
    for sql in ["SELECT missing FROM articles;", "DELETE FROM articles;"] {
        let result = scan(sql);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(directory.join("001.parquet"), directory.join("003.parquet"))
            .unwrap();
        let result = scan("SELECT * FROM articles;");
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("expected a regular Parquet file")
        );
    }
}

#[test]
fn streaming_select_validates_all_tables_and_preserves_logical_ids() {
    let db = Fixture::new(Some(concat!(
        "\u{feff}-- SCHEMA: first (key INT PRIMARY KEY, value TEXT)\r\n",
        "{\"key\":10,\"value\":\"雪\"}\r\n",
        "{\"key\":20}\n",
        "-- SCHEMA: second (key INT PRIMARY KEY)\n",
        "{\"key\":10}",
    )));
    assert_eq!(
        db.exec("SELECT _line, value FROM first WHERE key >= 10;"),
        [
            json!({"_line":1,"value":"雪"}),
            json!({"_line":2,"value":null}),
        ]
    );
    assert_eq!(
        db.exec("SELECT _line, key FROM second;"),
        [json!({"_line":1,"key":10})]
    );
    db.reject("SELECT * FROM missing;", "unknown table missing");
    let duplicate = Fixture::new(Some(concat!(
        "-- SCHEMA: first (key INT)\n",
        "-- SCHEMA: second (key INT PRIMARY KEY)\n",
        "{\"key\":5}\n{\"key\":5}\n",
    )));
    duplicate.reject("SELECT * FROM first;", "duplicate PRIMARY KEY");
}

#[test]
fn single_select_with_log_reads_updated_state() {
    let db = Fixture::new(Some(BASE));
    db.exec("UPDATE users SET age = 99 WHERE _line = 1; DELETE FROM users WHERE _line = 2;");
    assert_eq!(
        db.exec("SELECT _line, age FROM users;"),
        [json!({"_line":1,"age":99})]
    );
}

#[test]
fn disk_index_queries_and_stale_index_fallback() {
    let db = Fixture::new(Some(BASE));
    assert!(db.call("disk-index", None).status.success());
    assert!(db.file("db.jsqnl.seek.sqlite").is_file());
    assert_eq!(db.exec("SELECT COUNT(*) FROM users;"), [json!({"count":2})]);
    assert_eq!(
        db.exec("SELECT _line, name FROM users WHERE id = 20.0 AND age BETWEEN 19 AND 21;"),
        [json!({"_line":2,"name":"Taro"})]
    );
    assert_eq!(
        db.exec("SELECT COUNT(*) AS hits FROM users WHERE _line = 2;"),
        [json!({"hits":1})]
    );
    assert!(db.exec("SELECT * FROM users WHERE id = 999;").is_empty());
    db.reject(
        "SELECT bad FROM users WHERE id = 999;",
        "unknown column bad",
    );
    fs::write(db.file("db.jsqnl"), BASE.replace("\"id\":20", "\"id\":30")).unwrap();
    assert!(db.exec("SELECT * FROM users WHERE id = 20;").is_empty());
    assert_eq!(
        db.exec("SELECT _line FROM users WHERE id = 30;"),
        [json!({"_line":2})]
    );
    db.exec("UPDATE users SET age = 25 WHERE id = 30;");
    assert_eq!(
        db.exec("SELECT age FROM users WHERE id = 30;"),
        [json!({"age":25})]
    );
    assert!(!db.call("disk-index", None).status.success());

    let labels = Fixture::new(Some(
        "-- SCHEMA: labels (key TEXT PRIMARY KEY)\n{\"key\":\"雪\"}\n",
    ));
    assert!(labels.call("disk-index", None).status.success());
    assert_eq!(
        labels.exec("SELECT _line FROM labels WHERE key = '雪';"),
        [json!({"_line":1})]
    );
    let invalid = Fixture::new(Some(
        "-- SCHEMA: t (id INT PRIMARY KEY)\n{\"id\":1}\n{\"id\":1}\n",
    ));
    assert!(!invalid.call("disk-index", None).status.success());
    assert!(!invalid.file("db.jsqnl.seek.sqlite").exists());
}

#[test]
fn conventional_select_clauses_on_stream_and_journal() {
    let db = Fixture::new(Some(BASE));
    for _ in 0..2 {
        assert_eq!(
            db.exec("SELECT COUNT(*) FROM users WHERE id IN (10, 20) AND v IS NOT NULL;"),
            [json!({"count":2})]
        );
        assert_eq!(
            db.exec("SELECT name FROM users ORDER BY age DESC, id ASC LIMIT 1 OFFSET 1;"),
            [json!({"name":"雪"})]
        );
        assert_eq!(
            db.exec("SELECT COUNT(*) FROM users WHERE id = 99;"),
            [json!({"count":0})]
        );
        assert!(db.exec("SELECT COUNT(*) FROM users OFFSET 1;").is_empty());
        assert!(
            db.exec("SELECT name FROM users ORDER BY age LIMIT 0;")
                .is_empty()
        );
        db.reject(
            "SELECT name FROM users ORDER BY invalid;",
            "unknown column invalid",
        );
        db.exec("UPDATE users SET age = 20 WHERE id = 20;");
    }
    let large = Fixture::new(Some(&format!(
        "-- SCHEMA: entries (n INT)\n{}",
        (1..=40)
            .map(|n| format!("{{\"n\":{n}}}\n"))
            .collect::<String>()
    )));
    assert_eq!(
        large.exec("SELECT n FROM entries ORDER BY n DESC LIMIT 2 OFFSET 1;"),
        [json!({"n":39}), json!({"n":38})]
    );
}

#[test]
fn joins_preserve_cardinality_and_expose_qualified_columns() {
    let db = Fixture::new(Some(concat!(
        "-- SCHEMA: people (id INT PRIMARY KEY, name TEXT, team INT)\n",
        "{\"id\":1,\"name\":\"Ada\",\"team\":9}\n",
        "{\"id\":2,\"name\":\"Lin\",\"team\":8}\n",
        "{\"id\":3,\"name\":\"No team\"}\n",
        "-- SCHEMA: teams (id FLOAT, name TEXT)\n",
        "{\"id\":9.0,\"name\":\"Blue\"}\n",
        "{\"id\":9,\"name\":\"Green\"}\n",
        "{\"id\":null,\"name\":\"Unknown\"}\n",
    )));
    assert_eq!(db.exec("SELECT p.name, t.name FROM people AS p INNER JOIN teams AS t ON t.id = p.team ORDER BY t.name DESC;"), [
        json!({"p.name":"Ada","t.name":"Green"}), json!({"p.name":"Ada","t.name":"Blue"}),
    ]);
    assert_eq!(db.exec("SELECT people.name, teams.name FROM people LEFT JOIN teams ON people.team = teams.id WHERE teams.name IS NULL;"), [
        json!({"people.name":"Lin","teams.name":null}),
        json!({"people.name":"No team","teams.name":null}),
    ]);
    assert_eq!(db.exec("SELECT p.name, COUNT(*) AS total FROM people AS p LEFT OUTER JOIN teams AS t ON p.team = t.id GROUP BY p.name ORDER BY total DESC, p.name;"), [
        json!({"p.name":"Ada","total":2}), json!({"p.name":"Lin","total":1}),
        json!({"p.name":"No team","total":1}),
    ]);
    db.reject(
        "SELECT name FROM people JOIN teams ON people.team = teams.id;",
        "ambiguous JOIN column name",
    );
    db.reject(
        "SELECT * FROM people JOIN teams ON people.team = people.id;",
        "opposite tables",
    );
    assert_eq!(
        db.exec(
            "SELECT a.id, b.id FROM people AS a JOIN people AS b ON a.team = b.team WHERE a.id = 1;"
        ),
        [json!({"a.id":1,"b.id":1}),]
    );
}

#[test]
fn grouping_preserves_numeric_equality_and_count_with_disk_index_and_log() {
    let db = Fixture::new(Some(concat!(
        "-- SCHEMA: t (id INT PRIMARY KEY, category FLOAT)\n",
        "{\"id\":1,\"category\":2}\n",
        "{\"id\":2,\"category\":2.0}\n",
        "{\"id\":3,\"category\":1}\n",
        "{\"id\":4}\n",
        "{\"id\":5,\"category\":null}\n",
    )));
    assert!(db.call("disk-index", None).status.success());
    assert_eq!(db.exec("SELECT category, COUNT(*) AS total FROM t GROUP BY category ORDER BY total DESC, category DESC LIMIT 2;"), [
        json!({"category":2,"total":2}), json!({"category":null,"total":2}),
    ]);
    assert_eq!(
        db.exec("SELECT COUNT(*) FROM t WHERE id = 2 GROUP BY category;"),
        [json!({"count":1})]
    );
    assert!(
        db.exec("SELECT COUNT(*) FROM t WHERE id = 6 GROUP BY category;")
            .is_empty()
    );
    assert_eq!(
        db.exec("SELECT category FROM t GROUP BY category ORDER BY category LIMIT 1 OFFSET 1;"),
        [json!({"category":1})]
    );
    db.reject(
        "SELECT id, COUNT(*) FROM t GROUP BY category;",
        "explicit grouped columns",
    );
    db.exec("UPDATE t SET category = 2 WHERE id = 3;");
    assert_eq!(
        db.exec("SELECT category, COUNT(*) AS n FROM t GROUP BY category ORDER BY n DESC LIMIT 1;"),
        [json!({"category":2,"n":3})]
    );
}

#[cfg(unix)]
#[test]
fn database_sidecars_reject_unsafe_file_types() {
    use std::os::unix::fs::symlink;

    let db = Fixture::new(Some(BASE));
    let sentinel = db.file("sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    let log = db.file("db.jsqnl.log");
    symlink(&sentinel, &log).unwrap();
    db.reject("SELECT * FROM users;", "regular file");
    db.reject("DELETE FROM users;", "regular file");
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");

    fs::remove_file(&log).unwrap();
    fs::hard_link(&sentinel, &log).unwrap();
    db.reject("DELETE FROM users;", "hard-linked");
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");

    fs::remove_file(&log).unwrap();
    assert!(Command::new("mkfifo").arg(&log).status().unwrap().success());
    db.reject("SELECT * FROM users;", "regular file");
    fs::remove_file(&log).unwrap();

    let disk_index = db.file("db.jsqnl.seek.sqlite");
    symlink(&sentinel, &disk_index).unwrap();
    db.reject("SELECT * FROM users WHERE id = 10;", "regular file");
    fs::remove_file(&disk_index).unwrap();
    assert!(db.call("disk-index", None).status.success());
    let wal = db.file("db.jsqnl.seek.sqlite-wal");
    fs::write(&wal, b"unexpected").unwrap();
    db.reject(
        "SELECT * FROM users WHERE id = 10;",
        "unexpected disk index sidecar",
    );
    fs::remove_file(&wal).unwrap();
    assert_eq!(
        db.exec("SELECT name FROM users WHERE id = 10;"),
        [json!({"name":"雪"})]
    );

    let empty = Fixture::new(None);
    symlink(empty.file("absent"), empty.file("db.jsqnl.seek.sqlite")).unwrap();
    assert!(!empty.call("init", None).status.success());
    assert!(!empty.file("db.jsqnl").exists());
}

#[test]
fn grace_hash_join_supports_numeric_keys_and_cleans_spill_files() {
    let db = Fixture::new(Some(&format!(
        "-- SCHEMA: a (key FLOAT, padding TEXT)\n{{\"key\":2,\"padding\":\"{}\"}}\n{{\"key\":null}}\n{{\"key\":7}}\n-- SCHEMA: b (key FLOAT, n INT)\n{{\"key\":2.0,\"n\":6}}\n{{\"key\":2,\"n\":8}}\n{{\"key\":null,\"n\":9}}\n",
        "x".repeat(2048),
    )));
    let temp = db.file("scratch");
    fs::create_dir(&temp).unwrap();
    let execute = |sql: &str| {
        rows(
            command_for("exec", &db.file("db.jsqnl"))
                .arg(sql)
                .env("JSQNL_JOIN_MEMORY_BYTES", "1024")
                .env("TMPDIR", &temp)
                .output()
                .unwrap(),
        )
    };
    let query = "SELECT a._line, b.n FROM a LEFT JOIN b ON a.key = b.key ORDER BY a._line, b.n;";
    assert_eq!(
        execute(query),
        [
            json!({"a._line":1,"b.n":6}),
            json!({"a._line":1,"b.n":8}),
            json!({"a._line":2,"b.n":null}),
            json!({"a._line":3,"b.n":null}),
        ]
    );
    let report = execute(&format!("EXPLAIN ANALYZE {query}")).remove(0);
    assert_eq!(report["access"], "grace_hash_join");
    assert_eq!(report["rows_read"], 6);
    assert_eq!(report["oversized_partitions"], 0);
    assert!(report["spill_bytes_written"].as_u64().unwrap() > 0);
    #[cfg(unix)]
    assert_eq!(scratch_directories(&temp), 0);
}

#[test]
fn grace_hash_join_handles_skew_and_validates_primary_keys_on_disk() {
    let mut data = String::from(concat!(
        "-- SCHEMA: a (id INT PRIMARY KEY, key INT)\n",
        "{\"id\":1,\"key\":1}\n{\"id\":2,\"key\":2}\n",
        "-- SCHEMA: b (id INT PRIMARY KEY, key INT)\n",
    ));
    for id in 1..=90 {
        data.push_str(&format!("{{\"id\":{id},\"key\":1}}\n"));
    }
    let db = Fixture::new(Some(&data));
    let path = db.file("db.jsqnl");
    let query = "SELECT a.id, COUNT(*) AS total FROM a LEFT JOIN b ON a.key = b.key GROUP BY a.id ORDER BY a.id;";
    let execute = |sql| {
        command_for("exec", &path)
            .arg(sql)
            .env("JSQNL_JOIN_MEMORY_BYTES", "1024")
            .output()
            .unwrap()
    };
    assert_eq!(
        rows(execute(query)),
        [json!({"a.id":1,"total":90}), json!({"a.id":2,"total":1})]
    );
    let explain = format!("EXPLAIN ANALYZE {query}");
    assert_eq!(rows(execute(&explain))[0]["oversized_partitions"], 1);
    data.push_str("{\"id\":1,\"key\":2}\n");
    fs::write(&path, data).unwrap();
    let output = execute(query);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("validating PRIMARY KEY"));
}

#[cfg(unix)]
fn registry_path(temp: &Path) -> PathBuf {
    temp.join(format!("jsqnl-v1-{}", unsafe { libc::geteuid() }))
}

#[cfg(unix)]
fn scratch_directories(temp: &Path) -> usize {
    let root = registry_path(temp);
    if !root.exists() {
        return 0;
    }
    fs::read_dir(root)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("q-"))
        .count()
}

#[cfg(unix)]
#[test]
fn external_aggregation_preserves_counts_and_enforces_quota() {
    let mut input = String::from("-- SCHEMA: t (k FLOAT, label TEXT)\n");
    for n in 0..3000 {
        input.push_str(&format!("{{\"k\":{n},\"label\":\"x\"}}\n"));
    }
    input.push_str("{\"k\":2999.0,\"label\":\"x\"}\n{}\n{\"k\":null,\"label\":null}\n");
    let db = Fixture::new(Some(&input));
    let temp = db.file("temp");
    fs::create_dir(&temp).unwrap();
    let query =
        "SELECT k, label, COUNT(*) AS n FROM t GROUP BY k, label ORDER BY n DESC, k DESC LIMIT 2;";
    let execute = |sql: &str, quota: &str| {
        command_for("exec", &db.file("db.jsqnl"))
            .arg(sql)
            .env("TMPDIR", &temp)
            .env("JSQNL_GROUP_MEMORY_BYTES", "1024")
            .env("JSQNL_TEMP_LIMIT_BYTES", quota)
            .output()
            .unwrap()
    };
    let report = rows(execute(&format!("EXPLAIN ANALYZE {query}"), "33554432")).remove(0);
    assert!(
        report["operators"]
            .as_array()
            .unwrap()
            .contains(&json!("external_hash_aggregation"))
    );
    assert!(report["spill_bytes_written"].as_u64().unwrap() > 0);
    assert_eq!(
        rows(execute(query, "33554432")),
        [
            json!({"k":2999,"label":"x","n":2}),
            json!({"k":null,"label":null,"n":2}),
        ]
    );
    let error = execute(query, "1024");
    assert!(!error.status.success());
    assert!(error.stdout.is_empty());
    assert!(String::from_utf8_lossy(&error.stderr).contains("temporary storage quota exceeded"));
    assert_eq!(scratch_directories(&temp), 0);
}

#[cfg(unix)]
#[test]
fn temporary_recovery_removes_only_unlocked_query_directories() {
    use std::os::unix::fs::DirBuilderExt;

    let db = Fixture::new(None);
    let temp = db.file("temp");
    fs::create_dir(&temp).unwrap();
    let registry = registry_path(&temp);
    let private = |path: &Path| fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    private(&registry);
    let active = registry.join("q-1-1-1");
    let stale = registry.join("q-1-1-2");
    let incomplete = registry.join("q-1-1-3");
    for path in [&active, &stale, &incomplete] {
        private(path);
    }
    let live_lock = fs::File::create(active.join("active.lock")).unwrap();
    live_lock.lock().unwrap();
    fs::File::create(stale.join("active.lock")).unwrap();
    fs::write(stale.join("payload"), b"orphan").unwrap();
    fs::write(active.join("payload"), b"in use").unwrap();
    fs::write(registry.join("user-file"), b"keep").unwrap();
    std::os::unix::fs::symlink(&active, registry.join("q-1-1-4")).unwrap();

    let start = || {
        Command::new(env!("CARGO_BIN_EXE_jsqnl"))
            .arg("--version")
            .env("TMPDIR", &temp)
            .output()
            .unwrap()
    };
    let output = start();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stale.exists() && !incomplete.exists());
    assert_eq!(fs::read(active.join("payload")).unwrap(), b"in use");
    assert_eq!(fs::read(registry.join("user-file")).unwrap(), b"keep");
    assert!(
        fs::symlink_metadata(registry.join("q-1-1-4"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    live_lock.unlock().unwrap();
    drop(live_lock);
    assert!(start().status.success());
    assert!(!active.exists());
}

#[test]
fn deep_json_remains_readable_in_the_journal_and_spill_paths() {
    let nested = format!("{}0{}", "[".repeat(126), "]".repeat(126));
    let db = Fixture::new(Some(&format!(
        "-- SCHEMA: t (n INT, value JSON)\n{{\"n\":1,\"value\":{nested}}}\n"
    )));
    db.exec("UPDATE t SET n = 2;");
    assert_eq!(db.exec("SELECT n FROM t;"), [json!({"n":2})]);
    assert!(db.call("check", None).status.success());

    let data = format!(
        "-- SCHEMA: a (key INT, padding TEXT)\n{{\"key\":1,\"padding\":\"{}\"}}\n-- SCHEMA: b (key INT, value JSON)\n{{\"key\":1,\"value\":{nested}}}\n",
        "x".repeat(2048)
    );
    let join = Fixture::new(Some(&data));
    let result = command_for("exec", &join.file("db.jsqnl"))
        .env("JSQNL_JOIN_MEMORY_BYTES", "1024")
        .arg("SELECT b.value FROM a JOIN b ON a.key = b.key;")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!("{{\"b.value\":{nested}}}\n")
    );

    let mut data = format!("-- SCHEMA: t (value JSON)\n{{\"value\":{nested}}}\n");
    for number in 0..80 {
        data.push_str(&format!("{{\"value\":{number}}}\n"));
    }
    let group = Fixture::new(Some(&data));
    let result = command_for("exec", &group.file("db.jsqnl"))
        .env("JSQNL_GROUP_MEMORY_BYTES", "1024")
        .arg("SELECT COUNT(*) FROM t GROUP BY value;")
        .output()
        .unwrap();
    assert_eq!(rows(result), vec![json!({"count":1}); 81]);
}

#[cfg(target_os = "linux")]
fn constrained(command: &mut Command, resource: libc::__rlimit_resource_t, maximum: u64) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: maximum,
                rlim_max: maximum,
            };
            if libc::setrlimit(resource, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(target_os = "linux")]
#[test]
fn aggregation_spills_under_a_low_descriptor_limit() {
    let mut input = String::from("-- SCHEMA: t (n INT)\n");
    for n in 0..10_000 {
        input.push_str(&format!("{{\"n\":{n}}}\n"));
    }
    let db = Fixture::new(Some(&input));
    let mut command = command_for("exec", &db.file("db.jsqnl"));
    command
        .env("JSQNL_GROUP_MEMORY_BYTES", "32768")
        .arg("SELECT n, COUNT(*) AS total FROM t GROUP BY n ORDER BY n DESC LIMIT 1;");
    constrained(&mut command, libc::RLIMIT_NOFILE, 32);
    assert_eq!(
        rows(command.output().unwrap()),
        [json!({"n":9999,"total":1})]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn check_and_fork_stream_large_log_free_databases() {
    let db = Fixture::new(None);
    let path = db.file("db.jsqnl");
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    writeln!(file, "-- SCHEMA: t (n INT PRIMARY KEY, payload TEXT)").unwrap();
    let payload = "x".repeat(64);
    for n in 0..320_000 {
        writeln!(file, "{{\"n\":{n},\"payload\":\"{payload}\"}}").unwrap();
    }
    file.flush().unwrap();
    drop(file);
    let mut check = command_for("check", &path);
    constrained(&mut check, libc::RLIMIT_AS, 64 * 1024 * 1024);
    assert_eq!(
        rows(check.output().unwrap())[0]["rows_by_table"],
        json!({"t":320000})
    );
    let branch = db.file("branch");
    let mut fork = command_for("fork", &path);
    fork.arg(&branch);
    constrained(&mut fork, libc::RLIMIT_AS, 64 * 1024 * 1024);
    let result = fork.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        branch.join("db.jsqnl").metadata().unwrap().len()
    );
}

#[cfg(unix)]
#[test]
fn external_sort_merges_runs_and_honors_offset_without_a_limit() {
    let mut data = String::from("-- SCHEMA: t (n INT, s TEXT)\n");
    for n in (0..400).rev() {
        writeln!(
            data,
            "{{\"n\":{n},\"s\":\"{}\"}}",
            if n % 2 == 0 { "a" } else { "b" }
        )
        .unwrap();
    }
    let db = Fixture::new(Some(&data));
    let temp = db.file("temporary");
    fs::create_dir(&temp).unwrap();
    let run = |query: &str, quota: &str| {
        command_for("exec", &db.file("db.jsqnl"))
            .arg(query)
            .env("TMPDIR", &temp)
            .env("JSQNL_SORT_MEMORY_BYTES", "1024")
            .env("JSQNL_TEMP_LIMIT_BYTES", quota)
            .output()
            .unwrap()
    };
    let query = "SELECT n FROM t ORDER BY s DESC, n ASC OFFSET 3;";
    let expected: Vec<_> = (1..400)
        .step_by(2)
        .skip(3)
        .chain((0..400).step_by(2))
        .map(|n| json!({"n":n}))
        .collect();
    assert_eq!(rows(run(query, "8388608")), expected);
    let report = rows(run(&format!("EXPLAIN ANALYZE {query}"), "8388608")).remove(0);
    assert!(
        report["operators"]
            .as_array()
            .unwrap()
            .contains(&json!("external_sort"))
    );
    assert_eq!(report["rows_output"], 397);
    assert!(report["spill_bytes_written"].as_u64().unwrap() > 0);
    assert_eq!(
        rows(run(
            "SELECT n FROM t ORDER BY n DESC LIMIT 90 OFFSET 5;",
            "8388608"
        )),
        (305..395).rev().map(|n| json!({"n":n})).collect::<Vec<_>>()
    );
    let failed = run(query, "1024");
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("temporary storage quota exceeded"));
    assert_eq!(scratch_directories(&temp), 0);
}

#[test]
fn byte_limits_reject_records_frames_and_materialized_replay_cleanly() {
    let db = Fixture::new(Some(&format!(
        "-- SCHEMA: t (id INT PRIMARY KEY, value TEXT)\n{{\"id\":1,\"value\":\"{}\"}}\n",
        "x".repeat(2800),
    )));
    let path = db.file("db.jsqnl");
    let run = |sql: &str, name: &str, value: &str| {
        command_for("exec", &path)
            .arg(sql)
            .env(name, value)
            .output()
            .unwrap()
    };
    let fail = |output: Output, message: &str| {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    fail(
        run("SELECT id FROM t;", "JSQNL_RECORD_LIMIT_BYTES", "1024"),
        "record exceeds byte limit",
    );
    fail(
        run("UPDATE t SET id = 2;", "JSQNL_FRAME_LIMIT_BYTES", "1024"),
        "journal frame exceeds byte limit",
    );
    assert!(!db.file("db.jsqnl.log").exists());
    db.exec("UPDATE t SET id = 2;");
    fail(
        run("SELECT id FROM t;", "JSQNL_FRAME_LIMIT_BYTES", "1024"),
        "journal frame exceeds byte limit",
    );
    fail(
        run(
            "SELECT id FROM t;",
            "JSQNL_MATERIALIZED_LIMIT_BYTES",
            "1024",
        ),
        "materialization byte limit",
    );
    assert_eq!(db.exec("SELECT id FROM t;"), [json!({"id":2})]);
}

#[cfg(unix)]
#[test]
fn authenticated_journal_rejects_modified_frames_and_missing_or_wrong_keys() {
    use std::os::unix::fs::PermissionsExt;
    let db = Fixture::new(Some("-- SCHEMA: t (n INT)\n{\"n\":1}\n"));
    let path = db.file("db.jsqnl");
    let key = db.file("key");
    let wrong = db.file("wrong-key");
    for (key, value) in [(&key, 9), (&wrong, 10)] {
        fs::write(key, [value; 32]).unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let run = |path: &Path, sql: &str, key: &Path| {
        command_for("exec", path)
            .arg(sql)
            .env("JSQNL_JOURNAL_KEY_FILE", key)
            .output()
            .unwrap()
    };
    assert!(run(&path, "UPDATE t SET n = 2;", &key).status.success());
    assert!(run(&path, "UPDATE t SET n = 3;", &key).status.success());
    assert_eq!(rows(run(&path, "SELECT n FROM t;", &key)), [json!({"n":3})]);
    db.reject("SELECT n FROM t;", "required for authenticated journals");
    let rejected = run(&path, "SELECT n FROM t;", &wrong);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("journal authentication failed"));

    let log = db.file("db.jsqnl.log");
    let original = fs::read_to_string(&log).unwrap();
    let altered = original.replace("\"n\":3", "\"n\":4");
    assert_ne!(altered, original);
    fs::write(&log, altered).unwrap();
    let rejected = run(&path, "SELECT n FROM t;", &key);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("journal authentication failed"));
    fs::write(log, original).unwrap();
    assert_eq!(rows(run(&path, "SELECT n FROM t;", &key)), [json!({"n":3})]);

    let legacy = Fixture::new(Some("-- SCHEMA: t (n INT)\n{\"n\":1}\n"));
    legacy.exec("UPDATE t SET n = 2;");
    let rejected = run(&legacy.file("db.jsqnl"), "SELECT n FROM t;", &key);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("legacy frames cannot be mixed"));
}

fn convert(db: &Fixture, input: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_in2jsqnl"))
        .args(["--schema"])
        .arg(db.file("schema.sql"))
        .args(["--input"])
        .arg(db.file(input))
        .args(["--output"])
        .arg(db.file("db.jsqnl"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn import_jsonl_and_existing_sql_schema_then_query_jsqnl() {
    let db = Fixture::new(None);
    fs::write(db.file("schema.sql"), concat!(
        "/* Standard SQL with ANSI-quoted names */\n",
        "CREATE TABLE \"users\" (\"id\" BIGINT, name VARCHAR(12) NOT NULL, active BOOLEAN, profile JSONB, ",
        "v VECTOR(2), CONSTRAINT users_pk PRIMARY KEY (id));\n",
        "CREATE TABLE other (id INT PRIMARY KEY);\n"
    )).unwrap();
    fs::write(db.file("data.ndjson"), concat!(
        "\u{feff}{\"id\":1,\"name\":\"雪\",\"active\":true,\"profile\":{\"scores\":[1,2]},\"v\":[1,2]}\r\n",
        "\r\n{\"id\":2,\"name\":\"second\",\"active\":null}\n"
    )).unwrap();
    let missing_table = convert(&db, "data.ndjson", &[]);
    assert!(!missing_table.status.success());
    assert!(!db.file("db.jsqnl").exists());
    assert_eq!(
        rows(convert(&db, "data.ndjson", &["--table", "users"])),
        [json!({"table":"users","rows":2})]
    );
    let source = fs::read_to_string(db.file("db.jsqnl")).unwrap();
    assert!(source.starts_with("-- JSQNL: 1\n-- SCHEMA: users"));
    assert_eq!(
        db.exec("SELECT _line, name, profile FROM users WHERE active = true;"),
        [json!({"_line":1,"name":"雪","profile":{"scores":[1,2]}}),]
    );
    assert_eq!(db.exec("SELECT COUNT(*) FROM users;"), [json!({"count":2})]);
    assert!(
        !convert(&db, "data.ndjson", &["--table", "users"])
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(db.file("db.jsqnl")).unwrap(), source);
}

#[test]
fn import_json_object_array_and_empty_array() {
    for (content, count) in [
        ("{\"id\":1,\"n\":\"a\"}", 1),
        ("[\n{\"id\":1,\"n\":\"a\"},\n{\"id\":2,\"n\":\"b\"}\n]", 2),
        ("[ ]", 0),
    ] {
        let db = Fixture::new(None);
        fs::write(
            db.file("schema.sql"),
            "CREATE TABLE IF NOT EXISTS `t` ([id] INTEGER PRIMARY KEY, n CHARACTER VARYING(6));",
        )
        .unwrap();
        fs::write(db.file("data.txt"), content).unwrap();
        assert_eq!(
            rows(convert(&db, "data.txt", &["--format", "json"])),
            [json!({"table":"t","rows":count})]
        );
        assert_eq!(db.exec("SELECT COUNT(*) FROM t;"), [json!({"count":count})]);
    }
}

#[test]
fn import_rejects_unrepresentable_sql_and_invalid_data_atomically() {
    let db = Fixture::new(None);
    let schema = db.file("schema.sql");
    let input = db.file("data.json");
    fs::write(
        &schema,
        "CREATE TABLE t (id INT PRIMARY KEY, n VARCHAR(2) NOT NULL);",
    )
    .unwrap();
    for (content, diagnostic) in [
        (
            "[{\"id\":1,\"n\":\"a\"},{\"id\":1,\"n\":\"b\"}]",
            "PRIMARY KEY",
        ),
        ("[{\"id\":1,\"n\":\"abc\"}]", "character width"),
        ("[{\"id\":1,\"n\":null}]", "invalid value"),
        ("[{\"id\":1,\"n\":\"ok\"},]", "expected a JSON object"),
        ("[{\"id\":1,\"n\":\"ok\"}] {}", "trailing content"),
    ] {
        fs::write(&input, content).unwrap();
        let result = convert(&db, "data.json", &[]);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!db.file("db.jsqnl").exists());
        assert_eq!(fs::read_dir(&db.0).unwrap().count(), 2);
    }
    fs::write(
        &schema,
        "CREATE TABLE t (id INT, n TEXT, PRIMARY KEY (id,n));",
    )
    .unwrap();
    let result = convert(&db, "data.json", &[]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("single-column"));
    assert!(!db.file("db.jsqnl").exists());
}

#[test]
fn import_rejects_duplicate_members_in_both_formats() {
    let db = Fixture::new(None);
    fs::write(
        db.file("schema.sql"),
        "CREATE TABLE t (id INT PRIMARY KEY, payload JSON);",
    )
    .unwrap();
    for (file, data) in [
        ("data.json", "{\"id\":1,\"\\u0069d\":2}"),
        (
            "data.json",
            "[{\"id\":1},{\"id\":2,\"payload\":[{\"flag\":true,\"flag\":false}]}]",
        ),
        ("data.jsonl", "{\"id\":1}\n{\"id\":2,\"id\":3}\n"),
        ("data.jsonl", "{\"id\":1,\"payload\":{\"n\":1,\"n\":2}}\n"),
    ] {
        fs::write(db.file(file), data).unwrap();
        let result = convert(&db, file, &[]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("duplicate JSON member"));
        assert!(!db.file("db.jsqnl").exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn import_rejects_oversized_ddl_and_json_complexity_without_aborting() {
    let db = Fixture::new(None);
    fs::write(db.file("data.json"), "{\"id\":1}").unwrap();
    let output = db.file("db.jsqnl");
    let run = |expect: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_in2jsqnl"));
        command
            .arg("--schema")
            .arg(db.file("schema.sql"))
            .arg("--input")
            .arg(db.file("data.json"))
            .arg("--output")
            .arg(&output);
        constrained(&mut command, libc::RLIMIT_AS, 96 * 1024 * 1024);
        let result = command.output().unwrap();
        assert_eq!(result.status.code(), Some(1), "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expect),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!output.exists());
    };
    fs::write(db.file("schema.sql"), " ".repeat(4 * 1024 * 1024 + 1)).unwrap();
    run("SQL definition exceeds byte limit");
    fs::write(
        db.file("schema.sql"),
        "CREATE TABLE t (id INT);\n".repeat(10_000),
    )
    .unwrap();
    run("SQL definition exceeds token limit");
    fs::write(
        db.file("schema.sql"),
        "CREATE TABLE t (id INT, payload JSON);",
    )
    .unwrap();
    let data = format!("{{\"id\":1,\"payload\":[{}0]}}", "0,".repeat(262_144));
    fs::write(db.file("data.json"), data).unwrap();
    run("JSON record exceeds node limit");
}

#[cfg(unix)]
#[test]
fn concurrent_import_publishes_once_without_multiple_links() {
    use std::os::unix::fs::MetadataExt;
    let db = Fixture::new(None);
    fs::write(
        db.file("schema.sql"),
        "CREATE TABLE t (id INT PRIMARY KEY);",
    )
    .unwrap();
    fs::write(db.file("data.json"), "[{\"id\":1},{\"id\":2}]").unwrap();
    let path = db.file("db.jsqnl");
    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_in2jsqnl"))
                .arg("--schema")
                .arg(db.file("schema.sql"))
                .arg("--input")
                .arg(db.file("data.json"))
                .arg("--output")
                .arg(&path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    assert_eq!(
        outputs.iter().filter(|item| item.status.success()).count(),
        1
    );
    assert_eq!(path.metadata().unwrap().nlink(), 1);
    assert_eq!(db.exec("SELECT COUNT(*) FROM t;"), [json!({"count":2})]);
    assert_eq!(fs::read_dir(&db.0).unwrap().count(), 3);
}

#[test]
fn diagnostics_escape_control_characters_in_both_clis() {
    let db = Fixture::new(None);
    fs::write(db.file("schema.sql"), "CREATE TABLE t (id INT);").unwrap();
    let record = "{\"id\":1,\"\\u001bfield\\nother\":true}\n";
    fs::write(db.file("data.jsonl"), record).unwrap();
    let import = convert(&db, "data.jsonl", &[]);
    assert!(!import.status.success());
    assert!(!db.file("db.jsqnl").exists());
    fs::write(
        db.file("db.jsqnl"),
        format!("-- SCHEMA: t (id INT)\n{record}"),
    )
    .unwrap();
    let query = db.call("exec", Some("SELECT * FROM t;"));
    for output in [import, query] {
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.contains("unknown column") && diagnostic.contains("field"));
        assert!(
            diagnostic
                .trim_end_matches('\n')
                .chars()
                .all(|c| !c.is_control())
        );
    }
}

#[test]
fn commit_does_not_publish_a_frame_that_prevents_reopening() {
    let db = Fixture::new(Some(&format!(
        "--{}\n-- SCHEMA: t (n INT)\n{{\"n\":1}}\n",
        "x".repeat(900)
    )));
    let run = |sql| {
        command_for("exec", &db.file("db.jsqnl"))
            .arg(sql)
            .env("JSQNL_MATERIALIZED_LIMIT_BYTES", "1024")
            .output()
            .unwrap()
    };
    let failed = run("UPDATE t SET n = 2;");
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("transaction would exceed"));
    assert!(!db.file("db.jsqnl.log").exists());
    assert_eq!(rows(run("SELECT n FROM t;")), [json!({"n":1})]);
}

#[test]
fn replaced_or_unsealed_disk_index_is_never_used() {
    let db = Fixture::new(Some(BASE));
    assert!(db.call("disk-index", None).status.success());
    let persisted = db.file("db.jsqnl.seek.sqlite");
    let seal = db.file("db.jsqnl.seek.sqlite.seal");
    assert_eq!(rows(db.call("check", None))[0]["disk_index"], "current");
    {
        let index = rusqlite::Connection::open(&persisted).unwrap();
        index
            .execute("DELETE FROM offsets WHERE id = 2", [])
            .unwrap();
    }
    let checked = rows(db.call("check", None)).remove(0);
    assert_eq!(checked["disk_index"], "invalid");
    assert_eq!(checked["rows_by_table"], json!({"users":2}));
    assert_eq!(db.exec("SELECT COUNT(*) FROM users;"), [json!({"count":2})]);
    assert_eq!(
        db.exec("SELECT id FROM users WHERE _line = 2;"),
        [json!({"id":20})]
    );
    assert_eq!(
        db.exec("EXPLAIN ANALYZE SELECT COUNT(*) FROM users;")[0]["access"],
        "sequential_scan"
    );

    assert!(db.call("disk-index", None).status.success());
    assert_eq!(
        db.exec("EXPLAIN ANALYZE SELECT COUNT(*) FROM users;")[0]["access"],
        "index_count"
    );
    fs::remove_file(seal).unwrap();
    assert_eq!(rows(db.call("check", None))[0]["disk_index"], "unsealed");
    assert_eq!(
        db.exec("EXPLAIN ANALYZE SELECT COUNT(*) FROM users;")[0]["access"],
        "sequential_scan"
    );
}

#[cfg(unix)]
#[test]
fn authenticated_journal_is_bound_to_the_database_path_and_fork_rebinds_it() {
    use std::os::unix::fs::PermissionsExt;
    let a = Fixture::new(Some("-- SCHEMA: t (n INT)\n{\"n\":1}\n"));
    let b = Fixture::new(Some("-- SCHEMA: t (n INT)\n{\"n\":1}\n"));
    let key = a.file("key");
    fs::write(&key, [9; 32]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let keyed = |cmd, path: &Path| {
        let mut command = command_for(cmd, path);
        command.env("JSQNL_JOURNAL_KEY_FILE", &key);
        command
    };
    let a_path = a.file("db.jsqnl");
    let b_path = b.file("db.jsqnl");
    for sql in ["UPDATE t SET n = 7;", "UPDATE t SET n = 8;"] {
        assert!(
            keyed("exec", &a_path)
                .arg(sql)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fs::copy(a.file("db.jsqnl.log"), b.file("db.jsqnl.log")).unwrap();
    let refused = keyed("exec", &b_path)
        .arg("SELECT n FROM t;")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("authentication failed for this database path")
    );

    let branch = a.file("branch");
    let forked = keyed("fork", &a_path).arg(&branch).output().unwrap();
    assert!(
        forked.status.success(),
        "{}",
        String::from_utf8_lossy(&forked.stderr)
    );
    let new_path = branch.join("db.jsqnl");
    assert_ne!(
        fs::read(a.file("db.jsqnl.log")).unwrap(),
        fs::read(branch.join("db.jsqnl.log")).unwrap()
    );
    assert_eq!(
        rows(
            keyed("exec", &new_path)
                .arg("SELECT n FROM t;")
                .output()
                .unwrap()
        ),
        [json!({"n":8})]
    );
    assert!(
        keyed("exec", &new_path)
            .arg("UPDATE t SET n = 9;")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        rows(
            keyed("exec", &a_path)
                .arg("SELECT n FROM t;")
                .output()
                .unwrap()
        ),
        [json!({"n":8})]
    );
    assert_eq!(
        rows(
            keyed("exec", &new_path)
                .arg("SELECT n FROM t;")
                .output()
                .unwrap()
        ),
        [json!({"n":9})]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn jsqnl_rejects_dense_sql_and_json_before_exhausting_memory() {
    let db = Fixture::new(Some("-- SCHEMA: t (n INT, p JSON)\n{\"n\":1}\n"));
    let path = db.file("db.jsqnl");
    let mut sql = command_for("exec", &path);
    sql.arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    constrained(&mut sql, libc::RLIMIT_AS, 96 * 1024 * 1024);
    let mut child = sql.spawn().unwrap();
    let input = "SELECT n FROM t;\n".repeat(200_000);
    let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("SQL or script exceeds token limit"));

    let body = format!(
        "-- SCHEMA: t (n INT, p JSON)\n{{\"n\":1,\"p\":[{}0]}}\n",
        "0,".repeat(3_500_000)
    );
    fs::write(&path, body).unwrap();
    let mut query = command_for("exec", &path);
    query.arg("SELECT n FROM t;");
    constrained(&mut query, libc::RLIMIT_AS, 96 * 1024 * 1024);
    let result = query.output().unwrap();
    assert_eq!(result.status.code(), Some(1), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("JSON record exceeds node limit"));
}

#[cfg(unix)]
#[test]
fn legacy_authentication_is_explicit_read_only_and_fork_upgrades_it() {
    use std::os::unix::fs::PermissionsExt;
    let db = Fixture::new(Some("-- SCHEMA: t (n INT)\n{\"n\":1}\n"));
    let path = db.file("db.jsqnl");
    let log = db.file("db.jsqnl.log");
    let key = db.file("key");
    fs::write(&key, [9; 32]).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    // Known HMAC-SHA-256 v2 frame generated with the same 32-byte test key.
    let original = concat!(
        "hmac:1c7103ae8e8f941bb7bbe989a3b4e9326bbc9016f7347620ab2b9191b7971f35\t",
        "{\"version\":2,\"base\":\"d8775694ddfec4a104dd9ad05bf717903959f85b13c7adf4db1bae64fa69cac2\",",
        "\"previous\":\"\",\"changes\":[{\"Update\":{\"table\":\"t\",\"id\":1,\"row\":{\"n\":7}}}]}\n"
    );
    fs::write(&log, original).unwrap();
    let command = |verb, path: &Path, allow: bool| {
        let mut command = command_for(verb, path);
        command
            .env("JSQNL_JOURNAL_KEY_FILE", &key)
            .env("JSQNL_ALLOW_LEGACY_AUTH", if allow { "1" } else { "0" });
        command
    };
    let blocked = command("exec", &path, false)
        .arg("SELECT n FROM t;")
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("requires JSQNL_ALLOW_LEGACY_AUTH=1")
    );
    assert_eq!(
        rows(
            command("exec", &path, true)
                .arg("SELECT n FROM t;")
                .output()
                .unwrap()
        ),
        [json!({"n":7})]
    );
    let refused = command("exec", &path, true)
        .arg("UPDATE t SET n = 8;")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("read-only"));
    assert_eq!(fs::read_to_string(&log).unwrap(), original);
    let branch = db.file("upgraded");
    let result = command("fork", &path, true).arg(&branch).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let updated_path = branch.join("db.jsqnl");
    assert!(
        fs::read_to_string(branch.join("db.jsqnl.log"))
            .unwrap()
            .starts_with("hmac3:")
    );
    assert_eq!(
        rows(
            command("exec", &updated_path, false)
                .arg("SELECT n FROM t;")
                .output()
                .unwrap()
        ),
        [json!({"n":7})]
    );
}

#[test]
fn disk_index_respects_current_physical_line_limit() {
    let base = format!(
        "-- SCHEMA: t (id INT PRIMARY KEY)\n{}{{\"id\":1}}\n",
        " ".repeat(1024)
    );
    let db = Fixture::new(Some(&base));
    let path = db.file("db.jsqnl");
    let build = command_for("disk-index", &path)
        .env("JSQNL_RECORD_LIMIT_BYTES", "4096")
        .output()
        .unwrap();
    assert!(build.status.success(), "{build:?}");
    for (query, access) in [
        ("SELECT id FROM t WHERE id = 1;", "index_seek"),
        ("SELECT COUNT(*) FROM t;", "index_count"),
    ] {
        let execute = |limit, sql: &str| {
            command_for("exec", &path)
                .env("JSQNL_RECORD_LIMIT_BYTES", limit)
                .arg(sql)
                .output()
                .unwrap()
        };
        let report = rows(execute("4096", &format!("EXPLAIN ANALYZE {query}")));
        assert_eq!(report[0]["access"], access);
        let error = execute("1024", query);
        assert!(!error.status.success(), "{error:?}");
        assert!(String::from_utf8_lossy(&error.stderr).contains("record exceeds byte limit"));
    }
}

#[test]
fn import_can_use_full_scratch_budget_for_primary_keys() {
    let db = Fixture::new(None);
    let schema = db.file("input.sql");
    let input = db.file("input.jsonl");
    let output = db.file("db.jsqnl");
    let temp = db.file("temp");
    fs::create_dir(&temp).unwrap();
    fs::write(&schema, "CREATE TABLE t (id INT PRIMARY KEY);").unwrap();
    fs::write(&input, "{\"id\":1}\n").unwrap();
    let import = |limit| {
        Command::new(env!("CARGO_BIN_EXE_in2jsqnl"))
            .arg("--schema")
            .arg(&schema)
            .arg("--input")
            .arg(&input)
            .arg("--output")
            .arg(&output)
            .env("TMPDIR", &temp)
            .env("JSQNL_TEMP_LIMIT_BYTES", limit)
            .output()
            .unwrap()
    };
    assert!(!import("4096").status.success());
    assert!(!output.exists());
    assert_eq!(rows(import("12288")), [json!({"table":"t","rows":1})]);
    assert_eq!(db.exec("SELECT id FROM t;"), [json!({"id":1})]);
}
