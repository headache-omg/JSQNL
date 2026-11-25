# JSQNL v0.1 user guide

[日本語](../ja/README.md) · [English](../en/README.md) · [繁體中文](../zh-Hant/README.md) · [한국어](../ko/README.md) · [Français](../fr/README.md) · [Italiano](../it/README.md) · [Português](../pt/README.md) · [Русский](../ru/README.md)

JSQNL is an embedded database for local files. It provides a Rust library and two command-line programs, `jsqnl` for queries and `in2jsqnl` for imports.

## Build and run

Install Rust 1.89 or later, Cargo, and the SQLite development libraries for your system. Run the commands from the repository root.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked
```

Copy the example before issuing updates. `exec` queries a persistent database and writes query results as JSON Lines. `run` evaluates a mixed script in memory. Use `jsqnl init db.jsqnl` to create an empty database.

```sh
cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl run examples/demo.jsqnl
```

## File format

A `.jsqnl` base file contains UTF-8 schema lines and JSON objects. Each object occupies one physical line and belongs to the most recent schema. The optional `-- JSQNL: 1` header goes on the first line.

```sql
-- JSQNL: 1
-- SCHEMA: users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, embedding VECTOR(3))
{"id":1,"name":"Yuki","age":16,"embedding":[1,0,0]}
{"id":2,"name":"Taro","age":20,"embedding":[0,1,0]}
```

The supported types are `INT` (signed 64-bit), `FLOAT`, `TEXT`, `BOOL`, `JSON`, and `VECTOR(n)`. Each supplied vector must contain exactly `n` finite numbers. Missing and null values are allowed in nullable fields. A table can have one `INT` or `TEXT` primary key. See the [file specification](../../FORMAT.md) for precise encoding and compatibility rules.

## Queries and updates

Filter with `WHERE`, `IN`, and `BETWEEN`. Use `IS NULL` to match missing or null values. Ordinary comparisons involving a missing or null value do not match. Use `ORDER BY`, `LIMIT`, and `OFFSET` for ordered pages, and `GROUP BY` with `COUNT(*)` for counts. `INNER JOIN` and `LEFT JOIN` match one qualified column from each of two tables in the same database.

```sql
SELECT name FROM users WHERE age BETWEEN 18 AND 25 ORDER BY age DESC LIMIT 5;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

`THE 1` targets logical row ID 1. The virtual column `_line` exposes each row’s logical ID. These IDs are independent of physical line numbers and primary keys. `WHO =` replaces a value, and `WHO +` can add numbers, concatenate text, or extend a JSON array. `FUSE users THE 1 INTO THE 2 KEEP TARGET;` fills missing or null target fields, retains the target key and ID, and deletes the source.

```sql
RESONATE embedding FROM users WITH [1,0,0] LIMIT 5;
DECAY embedding FROM users BY 0.5 WHERE id = 1;
```

`RESONATE` ranks matching, nonzero vectors by cosine similarity. `DECAY` multiplies a `FLOAT` or all vector components by a factor from 0 through 1.

## JSON, JSONL, and Parquet

IN2JSQNL combines one supported SQL `CREATE TABLE` definition with a JSON object, a JSON array of objects, or JSONL. Specify the input format with `--format json` or `--format jsonl` when needed. Use `--table` to choose a definition from a multi-table SQL file. Composite keys, defaults, and foreign keys are outside the supported SQL subset.

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

`scan` queries Parquet shards with identical schemas in filename order. The table name comes from the directory name, or the parent's name when the directory is named `data`. Use one read-only `SELECT` per scan.

```sh
./target/release/jsqnl scan /path/to/enwiki/data 'SELECT identifier FROM enwiki LIMIT 5;'
```

## Storage and operating limits

Build a disk offset index for a base without an update log. It supports `_line` and primary-key equality lookups and unfiltered `COUNT(*)`. `EXPLAIN ANALYZE` runs a `SELECT` and reports the access path, bytes and rows read, and time. `check` verifies the database and reports index status.

```sh
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
./target/release/jsqnl check users.jsqnl
./target/release/jsqnl fork users.jsqnl experiment
```

Each successful write batch appends one committed frame to the adjacent `.log` file. Keep the base and journal together. `fork` copies a consistent snapshot to a new directory. A failed streaming read can leave partial output, so check the exit status before consuming it.

Large log-free joins, grouping, and sorts can spill to disk. Set operator budgets with `JSQNL_JOIN_MEMORY_BYTES`, `JSQNL_GROUP_MEMORY_BYTES`, and `JSQNL_SORT_MEMORY_BYTES`, and a per-query disk allowance with `JSQNL_TEMP_LIMIT_BYTES` (default 8 GiB). Memory budgets are estimates. Wide intermediate rows and primary-key validation without a usable index can increase memory use. Databases with changes in the journal are rebuilt in memory.

For authenticated journals, set `JSQNL_JOURNAL_KEY_FILE` to a protected file containing a 32-byte key. Version 3 authenticates committed frames and binds them to the canonical database path. Use `fork` to reauthenticate a copy for its destination. Detecting the loss of a committed suffix requires a separately trusted record of the last committed state.

## Rust API

`jsqnl::execute` accepts a path, SQL, and a callback for JSON result rows. The returned `QueryReport` describes execution. A write batch is committed before result callbacks run.

```rust
let report = jsqnl::execute("users.jsqnl", "SELECT name FROM users;", |row| {
    println!("{row}");
    Ok(())
})?;
println!("{}", report.rows_output);
```

## License and corporate inquiries

JSQNL uses the [MIT License](../../LICENSE), including corporate and commercial use under its terms. Contact the maintainers for corporate deployment assistance, maintenance, or separate agreements.
