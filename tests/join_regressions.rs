use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "jsqnl-join-regression-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("temp")).unwrap();
        // Logical IDs deliberately run opposite to key order. Wide group keys
        // force even a single spilled group through the SQLite reduction path.
        let mut base = String::from("-- SCHEMA: a (k INT, v INT, pad TEXT)\n");
        for i in 0..48 {
            let key = if i == 0 { -1 } else { 48 - i };
            base.push_str(&format!(
                "{}\n",
                json!({"k":key,"v":0,"pad":"x".repeat(600)})
            ));
        }
        base.push_str("-- SCHEMA: b (k INT)\n");
        for key in 1..48 {
            for _ in 0..2 {
                base.push_str(&format!("{}\n", json!({"k":key})));
            }
        }
        // A hot key also exercises the oversized Grace partition path.
        for _ in 0..20 {
            base.push_str("{\"k\":1}\n");
        }
        fs::write(root.join("db.jsqnl"), base).unwrap();
        Self(root)
    }

    fn query(&self, sql: &str, join_budget: &str, operator_budget: &str) -> Vec<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_jsqnl"))
            .arg("exec")
            .arg(self.0.join("db.jsqnl"))
            .arg(sql)
            .env("TMPDIR", self.0.join("temp"))
            .env("JSQNL_JOIN_MEMORY_BYTES", join_budget)
            .env("JSQNL_SORT_MEMORY_BYTES", operator_budget)
            .env("JSQNL_GROUP_MEMORY_BYTES", operator_budget)
            .env("JSQNL_TEMP_LIMIT_BYTES", "33554432")
            .output()
            .unwrap();
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn join_sort_ties_use_source_ids_across_plans_and_spills() {
    let db = Fixture::new();
    // Include an unmatched LEFT row and two right rows per matched left row.
    let query = "SELECT a._line, b._line FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.v LIMIT 8;";
    let expected = vec![
        json!({"a._line":1,"b._line":null}),
        json!({"a._line":2,"b._line":93}),
        json!({"a._line":2,"b._line":94}),
        json!({"a._line":3,"b._line":91}),
        json!({"a._line":3,"b._line":92}),
        json!({"a._line":4,"b._line":89}),
        json!({"a._line":4,"b._line":90}),
        json!({"a._line":5,"b._line":87}),
    ];
    for join in ["1048576", "1024"] {
        for sort in ["1048576", "1024"] {
            for _ in 0..3 {
                assert_eq!(
                    db.query(query, join, sort),
                    expected,
                    "join={join}, sort={sort}"
                );
                let page = query.replace("LIMIT 8", "LIMIT 4 OFFSET 2");
                assert_eq!(db.query(&page, join, sort), expected[2..6]);
            }
        }
    }
    // Materialized execution (a batch, then a nonempty log) uses the same ties.
    assert_eq!(
        db.query(&format!("{query} SELECT COUNT(*) FROM a;"), "1024", "1024")[..8],
        expected
    );
    db.query("UPDATE a SET v = 0 WHERE _line = 1;", "1024", "1024");
    assert_eq!(db.query(query, "1024", "1024"), expected);
}

#[test]
fn grouped_join_ties_survive_hash_and_sqlite_aggregation() {
    let db = Fixture::new();
    let query = "SELECT a.k, COUNT(*) AS n FROM a LEFT JOIN b ON a.k = b.k GROUP BY a.k, a.pad ORDER BY n DESC LIMIT 5 OFFSET 1;";
    let expected: Vec<_> = (43..=47)
        .rev()
        .map(|key| json!({"a.k":key,"n":2}))
        .collect();
    for join in ["1048576", "1024"] {
        for group in ["1048576", "1024"] {
            for _ in 0..3 {
                assert_eq!(
                    db.query(query, join, group),
                    expected,
                    "join={join}, group={group}"
                );
            }
        }
    }
}

fn assert_spill_report(sql: &str, external: &str, logical: &str) {
    let db = Fixture::new();
    for (budget, access) in [("1048576", "streaming_join"), ("1024", "grace_hash_join")] {
        let report = db.query(sql, budget, "1024").remove(0);
        assert_eq!(report["access"], access);
        assert!(
            report["spill_bytes_written"].as_u64().unwrap_or(0) > 0,
            "{report}"
        );
        let operators = report["operators"].as_array().unwrap();
        assert!(operators.contains(&json!(external)), "{report}");
        if logical == "sort" {
            assert!(!operators.contains(&json!(logical)), "{report}");
        } else {
            let join = operators
                .iter()
                .position(|op| op == "indexed_equi_join" || op == "grace_hash_join")
                .unwrap();
            let spill = operators.iter().position(|op| op == external).unwrap();
            let group = operators.iter().position(|op| op == logical).unwrap();
            assert!(join < spill && spill < group, "{report}");
        }
        if access == "grace_hash_join" {
            assert!(report["oversized_partitions"].as_u64().unwrap() > 0);
        }
    }
}

#[test]
fn explain_join_reports_external_sort() {
    assert_spill_report(
        "EXPLAIN ANALYZE SELECT a.k FROM a JOIN b ON a.k = b.k ORDER BY a.k DESC;",
        "external_sort",
        "sort",
    );
}

#[test]
fn explain_join_reports_external_aggregation() {
    assert_spill_report(
        "EXPLAIN ANALYZE SELECT a.k, COUNT(*) AS n FROM a JOIN b ON a.k = b.k GROUP BY a.k, a.pad;",
        "external_hash_aggregation",
        "group",
    );
}

#[test]
fn grace_join_without_order_by_paginates_in_source_order() {
    let db = Fixture::new();
    let query = "SELECT a._line, b._line FROM a LEFT JOIN b ON a.k = b.k LIMIT 4 OFFSET 2;";
    let expected = vec![
        json!({"a._line":2,"b._line":94}),
        json!({"a._line":3,"b._line":91}),
        json!({"a._line":3,"b._line":92}),
        json!({"a._line":4,"b._line":89}),
    ];
    assert_eq!(db.query(query, "1048576", "1024"), expected);
    for _ in 0..3 {
        assert_eq!(db.query(query, "1024", "1024"), expected);
    }
}
