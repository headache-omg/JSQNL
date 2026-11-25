// Independent reader for the format-v1 compatibility fixtures.
use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

static HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^--\s*(?i-u:JSQNL):\s*(.*)$").unwrap());
static SCHEMA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:--\s*(?i-u:SCHEMA):\s*|(?i-u:SCHEMA)\s+)(.*)$").unwrap());
static TABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)\s*\((.*)\)\s*;?\s*$").unwrap());
static COLUMN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^([A-Za-z_][A-Za-z0-9_]*)\s+",
        r"((?i-u:INTEGER|INT|FLOAT|NUMBER|REAL|TEXT|STRING|BOOLEAN|BOOL|JSON|VECTOR\s*\(\s*([0-9]+)\s*\)))",
        r"(.*)$"
    ))
    .unwrap()
});

enum Kind {
    Int,
    Float,
    Text,
    Bool,
    Json,
    Vector(usize),
}

struct Column {
    kind: Kind,
    required: bool,
}

impl Column {
    fn accepts(&self, value: &Value) -> bool {
        if value.is_null() {
            return !self.required;
        }
        match self.kind {
            Kind::Int => value.is_i64(),
            Kind::Float => value.is_number(),
            Kind::Text => value.is_string(),
            Kind::Bool => value.is_boolean(),
            Kind::Json => true,
            Kind::Vector(size) => value
                .as_array()
                .is_some_and(|values| values.len() == size && values.iter().all(Value::is_number)),
        }
    }
}

struct Table {
    columns: HashMap<String, Column>,
    primary: Option<String>,
    keys: HashSet<String>,
    rows: Vec<Value>,
}

impl Table {
    fn parse(source: &str) -> Result<(String, Self)> {
        let definition = source.split("--").next().unwrap_or_default();
        let table = TABLE.captures(definition).context("invalid SCHEMA")?;
        let mut columns = HashMap::new();
        let mut primary = None;
        for raw in table[2].split(',') {
            let column = COLUMN.captures(raw.trim()).context("invalid column")?;
            let name = &column[1];
            ensure!(
                name != "_line" && !columns.contains_key(name),
                "reserved or duplicate column"
            );
            let kind = if let Some(dimension) = column.get(3) {
                ensure!(
                    !dimension.as_str().starts_with('0'),
                    "invalid VECTOR dimension"
                );
                Kind::Vector(dimension.as_str().parse()?)
            } else {
                match column[2].to_ascii_uppercase().as_str() {
                    "INT" | "INTEGER" => Kind::Int,
                    "FLOAT" | "NUMBER" | "REAL" => Kind::Float,
                    "TEXT" | "STRING" => Kind::Text,
                    "BOOL" | "BOOLEAN" => Kind::Bool,
                    "JSON" => Kind::Json,
                    _ => unreachable!(),
                }
            };
            let constraints = &column[4];
            ensure!(
                constraints.is_empty()
                    || constraints.starts_with(' ')
                    || matches!(kind, Kind::Vector(_)),
                "expected a separator before constraints"
            );
            let constraints = constraints.to_ascii_uppercase();
            let mut words = constraints.split_whitespace();
            let mut not_null = false;
            let mut primary_key = false;
            while let Some(word) = words.next() {
                match (word, words.next()) {
                    ("NOT", Some("NULL")) if !not_null => not_null = true,
                    ("PRIMARY", Some("KEY")) if !primary_key => primary_key = true,
                    _ => anyhow::bail!("invalid or duplicate constraint"),
                }
            }
            if primary_key {
                ensure!(
                    primary.is_none() && matches!(kind, Kind::Int | Kind::Text),
                    "invalid primary key"
                );
                primary = Some(name.to_owned());
            }
            columns.insert(
                name.to_owned(),
                Column {
                    kind,
                    required: not_null || primary_key,
                },
            );
        }
        Ok((
            table[1].to_owned(),
            Self {
                columns,
                primary,
                keys: HashSet::new(),
                rows: Vec::new(),
            },
        ))
    }

    fn push(&mut self, text: &str) -> Result<()> {
        let row: Value = serde_json::from_str(text)?;
        let object = row.as_object().context("expected a JSON object")?;
        ensure!(
            object.keys().all(|key| self.columns.contains_key(key))
                && self
                    .columns
                    .iter()
                    .all(|(name, column)| column.accepts(&row[name])),
            "row violates SCHEMA"
        );
        if let Some(primary) = &self.primary {
            ensure!(
                self.keys.insert(row[primary].to_string()),
                "duplicate primary key"
            );
        }
        self.rows.push(row);
        Ok(())
    }
}

pub fn read(path: &Path, table: &str) -> Result<Vec<Value>> {
    let source = std::fs::read_to_string(path)?;
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    let mut tables: HashMap<String, Table> = HashMap::new();
    let mut current = None;
    for (index, physical_line) in source.split_inclusive('\n').enumerate() {
        let line = match physical_line.strip_suffix('\n') {
            Some(line) => line.strip_suffix('\r').unwrap_or(line),
            None => physical_line,
        };
        ensure!(!line.contains('\r'), "invalid physical line");
        let text = line.trim();
        if text.starts_with('{') {
            let name = current.as_ref().context("JSONL row before SCHEMA")?;
            tables
                .get_mut(name)
                .context("missing current table")?
                .push(text)?;
            continue;
        }
        let directive: String = text
            .chars()
            .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
            .collect();
        if let Some(header) = HEADER.captures(&directive) {
            ensure!(
                index == 0 && &header[1] == "1",
                "unsupported version or position"
            );
        } else if let Some(schema) = SCHEMA.captures(&directive) {
            let (name, table) = Table::parse(&schema[1])?;
            ensure!(
                tables.insert(name.clone(), table).is_none(),
                "duplicate table"
            );
            current = Some(name);
        } else {
            ensure!(
                text.is_empty() || text.starts_with("--"),
                "expected a JSON object"
            );
        }
    }
    Ok(tables.remove(table).context("unknown table")?.rows)
}
