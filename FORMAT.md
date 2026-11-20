# JSQNL Base File Format 1

This specification defines the base file exchanged between JSQNL implementations.

## Encoding and version

- Files use UTF-8. A UTF-8 BOM is permitted at byte zero.
- The optional header `-- JSQNL: 1` belongs on the first physical line. An absent header denotes version 1. Readers must reject unsupported versions and misplaced version headers.
- Writers should emit LF. Readers accept LF, CRLF, and a final line without a terminator. An embedded raw CR inside a record is invalid.
- A physical line contains a schema, a JSON object, an ordinary `--` comment, or whitespace. A JSON data line cannot have a trailing comment.

## Schema and rows

```sql
-- JSQNL: 1
-- SCHEMA: articles (id INT PRIMARY KEY, name TEXT NOT NULL, metadata JSON, embedding VECTOR(3))
{"id":1,"name":"雪\nSnow","metadata":{"tags":["winter"]},"embedding":[1,0,0]}
{"id":2,"name":"Rain","metadata":null}
```

The schema spelling is `-- SCHEMA: table (column TYPE [NOT NULL] [PRIMARY KEY], ...)`. Readers also accept `SCHEMA table (...)` and an optional terminating semicolon. Format keywords and type names are ASCII case insensitive. Names are case sensitive and match `[A-Za-z_][A-Za-z_0-9]*`. Table and column names must be unique. A schema requires at least one column. `_line` is reserved.

Each data object belongs to the most recent schema. A table has one contiguous section, which may contain comments and blank lines. A data object before any schema or an undeclared field is invalid. Members can appear in any order. Producers must write unique JSON member names. The current Rust and Python readers use the last value for duplicated JSON members.

| Type | JSON value |
| --- | --- |
| `INT` or `INTEGER` | Signed 64-bit integer literal. Use `0` for zero. Negative zero, decimal points and exponents are rejected for `INT`. |
| `FLOAT`, `REAL`, `NUMBER` | Finite JSON number. Fractional values are read as binary64. |
| `TEXT` or `STRING` | JSON string. |
| `BOOL` or `BOOLEAN` | JSON `true` or `false`. |
| `JSON` | Any supported JSON value, including objects and arrays. |
| `VECTOR(n)` | Array of exactly `n` finite JSON numbers, where `n` is a positive integer written without leading zeros and representable as a platform array length. |

JSON strings and member names use double quotes and JSON escapes. Use `\n` for a newline, `\t` for a tab, `\"` for a quotation mark and `\\` for a backslash. Unicode can be literal UTF-8 or a JSON Unicode escape. Surrogate escapes must form valid pairs. Writers must emit finite numbers. For exact integer interchange, use signed 64-bit values. Larger integers in JSON or FLOAT fields can be rounded by readers.

## NULL, keys and row numbers

Missing members and explicit JSON `null` are accepted by nullable columns. Both fail `NOT NULL` and `PRIMARY KEY`. Explicit column selection and filters evaluate missing members as null. `SELECT *` preserves omitted members. A table may declare one INT or TEXT primary key. Its value must be unique in the table.

Each table assigns logical IDs beginning at 1, in JSON object order. Headers, comments and blank lines do not consume IDs. The virtual `_line` field exposes that number and is omitted from stored objects.

## Persistence boundary

A database with mutations also has an adjacent `.log` companion. Keep the base and log together. A reader implementing this base format can exchange the base rows independently.

The log version is independent of the base version. In the Rust engine, each log frame consists of a tag, a tab, a UTF-8 JSON transaction and LF. For transaction version 1, the tag is the lowercase ASCII-hex SHA-256 digest of the transaction bytes. For authenticated version 3, the tag is `hmac3:` followed by the lowercase ASCII-hex HMAC-SHA-256 using a separate 32-byte key. The authenticated byte sequence is the ASCII domain string `JSQNL journal v3`, a single zero byte, the canonical path byte length as unsigned 64-bit big-endian, the canonical path's platform-native Rust encoded bytes, and the transaction bytes. A fork re-authenticates committed transactions for the destination path. Legacy version 2 uses `hmac:` followed by HMAC-SHA-256 over only the transaction bytes. It can be read with an explicit compatibility flag or upgraded through a fork. The chain field in either version stores the SHA-256 digest of the preceding transaction bytes. The transaction binds the exact base bytes by SHA-256 and links to the preceding frame's hash. A final suffix without LF is uncommitted. Journal replay semantics remain implementation-specific. The `.idx`, `.seek.sqlite` and `.seek.sqlite.seal` indexes are derived files.

The independent Python reader in `tests/reference_reader.py` and the embedded Rust API share the fixtures in `tests/api.rs`. Tests cover versioned and legacy files along with representative malformed input.
