# JSQNL

JSQNL is a local, embeddable database written in Rust. Store data as schema-validated JSON Lines and query it with SQL-style statements. Use the command-line tools or call the same query engine from a Rust application.

## Documentation

[日本語](docs/ja/README.md) · [English](docs/en/README.md) · [繁體中文](docs/zh-Hant/README.md) · [한국어](docs/ko/README.md) · [Français](docs/fr/README.md) · [Italiano](docs/it/README.md) · [Português](docs/pt/README.md) · [Русский](docs/ru/README.md)

See the [version 1 file format](FORMAT.md) for schemas, types, encoding, logical row IDs, and the journal format.

## Quick start

Build with Rust 1.89 or later, Cargo, and the system SQLite development libraries.

```sh
git clone https://github.com/headache-omg/JSQNL.git
cd JSQNL
cargo build --release --locked
cargo test --locked

cp examples/users.jsqnl users.jsqnl
./target/release/jsqnl exec users.jsqnl 'SELECT _line, name FROM users WHERE age >= 18;'
./target/release/jsqnl disk-index users.jsqnl
./target/release/jsqnl exec users.jsqnl 'EXPLAIN ANALYZE SELECT name FROM users WHERE id = 2;'
```

`SELECT` writes JSONL. Use `jsqnl init db.jsqnl` to create an empty database and `jsqnl run examples/demo.jsqnl` to run an in-memory script containing schemas, JSONL, and SQL.

## Query and update data

```sql
SELECT name, age FROM users ORDER BY age DESC LIMIT 10;
SELECT age, COUNT(*) AS total FROM users GROUP BY age ORDER BY total DESC;
INSERT INTO users (id, name, age) VALUES (3, 'Mika', 21);
UPDATE users SET age = 22 WHERE id = 3;
INJECT age FROM users THE 1 WHO + 2;
DELETE FROM users WHERE id = 3;
```

JSQNL supports inner and left equijoins, grouped counts, sorting, pagination, and fixed-dimension vectors. `THE` and `_line` refer to a table-local logical row ID. See the [English guide](docs/en/README.md) for the Rust API, vector operations, Parquet scans, indexes, and resource settings.

## Import JSON or JSONL

```sh
./target/release/in2jsqnl --schema examples/import.sql \
  --input examples/import.jsonl --output imported.jsqnl
./target/release/jsqnl exec imported.jsqnl 'SELECT name FROM users WHERE active = true;'
```

IN2JSQNL combines one supported SQL `CREATE TABLE` definition with JSON or JSONL data. Parquet is available through a separate read-only `jsqnl scan` command with a locally installed PyArrow.

## Storage and scale

Keep a database's base file and adjacent `.log` together. Use `jsqnl check` to verify them, and `jsqnl fork` to copy a consistent snapshot to a new directory. A disk offset index can accelerate primary-key and `_line` equality lookups on a base with no update log. Large joins, grouping, and sorting can spill to temporary storage.

JSQNL v0.1 rebuilds the database in memory when a non-empty update log is present. Query memory settings are estimates. See the [operating limits](docs/en/README.md#storage-and-operating-limits) before using large datasets.

## License

JSQNL is available under the [MIT License](LICENSE), including corporate and commercial use under its terms. Contact the maintainers for corporate deployment assistance, maintenance, or a separate agreement.
