use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Kind {
    Int,
    Float,
    Text,
    Bool,
    Json,
    Vector(usize),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub kind: Kind,
    pub required: bool,
    pub primary: bool,
}

#[derive(Clone, Debug)]
pub enum Statement {
    Schema(String, Vec<Column>),
    Row(Value),
    ExplainAnalyze(Box<Statement>),
    Select(
        String,
        Option<Vec<String>>,
        Option<Predicate>,
        SelectOptions,
    ),
    Resonate {
        table: String,
        column: String,
        query: Value,
        predicate: Option<Predicate>,
        limit: usize,
    },
    Fuse {
        table: String,
        source: u64,
        target: u64,
        keep_target: bool,
    },
    Decay {
        table: String,
        column: String,
        factor: f64,
        predicate: Option<Predicate>,
    },
    Insert(String, InsertData),
    Update(String, Vec<(String, Value)>, Option<Predicate>),
    Delete(String, Option<Predicate>),
    Inject(String, String, u64, Op, Value),
}

#[derive(Clone, Debug, Default)]
pub struct SelectOptions {
    pub count: Option<String>,
    pub order: Vec<(String, bool)>,
    pub limit: Option<u64>,
    pub offset: u64,
    pub group: Vec<String>,
    pub alias: Option<String>,
    pub join: Option<Join>,
}

#[derive(Clone, Debug)]
pub struct Join {
    pub table: String,
    pub alias: Option<String>,
    pub left: bool,
    pub first_key: String,
    pub second_key: String,
}

#[derive(Clone, Debug)]
pub enum InsertData {
    Object(serde_json::Map<String, Value>),
    Values(Option<Vec<String>>, Vec<Value>),
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug)]
pub enum Predicate {
    Compare(String, Op, Value),
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Identifier(String),
    Value(Value),
    Symbol(String),
    Header,
}

#[derive(Debug)]
struct Located {
    token: Token,
    offset: usize,
}

pub(crate) fn validate_version_line(line: &str, first_line: bool) -> Result<()> {
    let Some(body) = line.trim_start().strip_prefix("--") else {
        return Ok(());
    };
    let body = body.trim_start();
    if body
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("JSQNL:"))
    {
        ensure!(
            first_line,
            "JSQNL version header must be on the first physical line"
        );
        let version = body[6..].trim();
        ensure!(
            version == "1",
            "unsupported JSQNL file format version {version:?}"
        );
    }
    Ok(())
}

fn lex(source: &str) -> Result<Vec<Located>> {
    lex_mode(source, false)
}

fn lex_mode(source: &str, ddl: bool) -> Result<Vec<Located>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < source.len() {
        let rest = &source[offset..];
        let first = rest.chars().next().unwrap();
        if first.is_whitespace() || (offset == 0 && first == '\u{feff}') {
            offset += first.len_utf8();
            continue;
        }
        if ddl && let Some(comment) = rest.strip_prefix("/*") {
            let end = comment
                .find("*/")
                .ok_or_else(|| anyhow!("byte {offset}: unterminated SQL comment"))?;
            offset += end + 4;
            continue;
        }
        let (token, len) = if rest.starts_with("--") {
            let line = rest.split('\n').next().unwrap();
            if !ddl {
                validate_version_line(line, !source[..offset].contains('\n'))?;
            }
            let body = line[2..].trim_start();
            if !ddl
                && body
                    .get(..7)
                    .is_some_and(|p| p.eq_ignore_ascii_case("SCHEMA:"))
            {
                (Token::Header, line.len() - body.len() + 7)
            } else {
                offset += line.len();
                continue;
            }
        } else if first.is_ascii_digit() {
            let bytes = rest.as_bytes();
            let mut len = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if bytes.get(len) == Some(&b'.') {
                len += 1;
                len += bytes[len..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
            }
            if matches!(bytes.get(len), Some(b'e' | b'E')) {
                len += 1;
                if matches!(bytes.get(len), Some(b'+' | b'-')) {
                    len += 1;
                }
                len += bytes[len..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
            }
            let value =
                serde_json::from_str(&rest[..len]).map_err(|e| anyhow!("byte {offset}: {e}"))?;
            (Token::Value(value), len)
        } else if ddl && matches!(first, '"' | '`' | '[') {
            let close = if first == '[' { ']' } else { first };
            let mut chars = rest.char_indices().skip(1).peekable();
            let mut name = String::new();
            let len = loop {
                let (pos, character) = chars
                    .next()
                    .ok_or_else(|| anyhow!("byte {offset}: unterminated SQL identifier"))?;
                if character == close && chars.next_if(|(_, next)| *next == close).is_none() {
                    break pos + close.len_utf8();
                }
                name.push(character);
            };
            validate_import_identifier(&name)?;
            (Token::Identifier(name), len)
        } else if matches!(first, '"' | '{' | '[') {
            let mut values = serde_json::Deserializer::from_str(rest)
                .into_iter::<crate::import_json::BoundedValue>();
            let value = values
                .next()
                .unwrap()
                .map_err(|e| anyhow!("byte {offset}: {e}"))?;
            (Token::Value(value.0), values.byte_offset())
        } else if first == '\'' {
            let mut value = String::new();
            let mut chars = rest.char_indices().skip(1).peekable();
            let len = loop {
                let (pos, c) = chars
                    .next()
                    .ok_or_else(|| anyhow!("byte {offset}: unterminated string"))?;
                if c == '\'' {
                    if chars.next_if(|(_, next)| *next == '\'').is_none() {
                        break pos + 1;
                    }
                }
                value.push(c);
            };
            (Token::Value(Value::String(value)), len)
        } else if first.is_ascii_alphabetic() || first == '_' {
            let len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            let word = &rest[..len];
            (Token::Word(word.to_owned()), len)
        } else {
            let len = ["<=", ">=", "!=", "<>", "=="]
                .iter()
                .find(|s| rest.starts_with(**s))
                .map_or(1, |s| s.len());
            ensure!(
                "(),.;*=+-/<>".contains(first) || len == 2,
                "byte {offset}: unexpected {first:?}"
            );
            (Token::Symbol(rest[..len].to_owned()), len)
        };
        const TOKEN_LIMIT: usize = 65_536;
        ensure!(
            result.len() < TOKEN_LIMIT,
            "{} exceeds token limit ({TOKEN_LIMIT})",
            if ddl {
                "SQL definition"
            } else {
                "SQL or script"
            }
        );
        result.push(Located { token, offset });
        offset += len;
    }
    Ok(result)
}

struct Parser {
    tokens: Vec<Located>,
    pos: usize,
}

pub fn parse(source: &str) -> Result<Vec<(u64, Statement)>> {
    let mut parser = Parser {
        tokens: lex(source)?,
        pos: 0,
    };
    let mut statements = Vec::new();
    let mut line = 1;
    let mut previous_offset = 0;
    while let Some(item) = parser.tokens.get(parser.pos) {
        let offset = item.offset;
        line += source[previous_offset..offset]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        previous_offset = offset;
        let statement = parser
            .statement()
            .map_err(|e| anyhow!("line {line}, byte {offset}: {e}"))?;
        if matches!(statement, Statement::Row(_) | Statement::Schema(..)) {
            if matches!(statement, Statement::Row(_)) {
                let end = source[offset..].split('\n').next().unwrap();
                let prefix = source[..offset]
                    .rsplit('\n')
                    .next()
                    .unwrap()
                    .trim_start_matches('\u{feff}');
                ensure!(
                    prefix.trim().is_empty()
                        && serde_json::from_str::<crate::import_json::BoundedValue>(end).is_ok(),
                    "line {line}: JSONL requires one object per physical line"
                );
            } else {
                let last = parser.tokens[parser.pos - 1].offset;
                ensure!(
                    !source[offset..last].contains('\n'),
                    "line {line}: SCHEMA must fit on one line"
                );
                parser.symbol(";");
                if let Some(next) = parser.tokens.get(parser.pos) {
                    ensure!(
                        source[offset..next.offset].contains('\n'),
                        "line {line}: SCHEMA must occupy its own line"
                    );
                }
            }
        } else {
            ensure!(parser.symbol(";"), "line {line}: expected ';'");
        }
        statements.push((offset as u64, statement));
    }
    Ok(statements)
}

pub(crate) struct ImportTable {
    pub name: String,
    pub columns: Vec<Column>,
    pub widths: Vec<(String, u64)>,
}

fn validate_import_identifier(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    ensure!(
        bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "SQL identifier {name:?} cannot be represented in JSQNL"
    );
    Ok(())
}

pub(crate) fn parse_import_tables(source: &str) -> Result<Vec<ImportTable>> {
    let mut parser = Parser {
        tokens: lex_mode(source, true)?,
        pos: 0,
    };
    let mut tables = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(token) = parser.tokens.get(parser.pos) {
        let offset = token.offset;
        let table = parser
            .import_table()
            .map_err(|error| anyhow!("SQL DDL at byte {offset}: {error}"))?;
        ensure!(
            seen.insert(table.name.clone()),
            "duplicate SQL table {}",
            table.name
        );
        tables.push(table);
    }
    ensure!(!tables.is_empty(), "expected a CREATE TABLE definition");
    Ok(tables)
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|item| &item.token)
    }

    fn take(&mut self, predicate: impl FnOnce(&Token) -> bool) -> bool {
        if self.peek().is_some_and(predicate) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn word(&mut self, key: &str) -> bool {
        self.take(|token| matches!(token, Token::Word(word) if word.eq_ignore_ascii_case(key)))
    }

    fn symbol(&mut self, key: &str) -> bool {
        self.take(|token| matches!(token, Token::Symbol(symbol) if symbol == key))
    }

    fn expect_word(&mut self, key: &str) -> Result<()> {
        ensure!(self.word(key), "expected {key}");
        Ok(())
    }

    fn expect_symbol(&mut self, key: &str) -> Result<()> {
        ensure!(self.symbol(key), "expected {key:?}");
        Ok(())
    }

    fn name(&mut self) -> Result<String> {
        let Some(Token::Word(name) | Token::Identifier(name)) = self.peek() else {
            bail!("expected identifier")
        };
        let name = name.clone();
        self.pos += 1;
        Ok(name)
    }

    fn value(&mut self) -> Result<Value> {
        if self.word("NULL") {
            return Ok(Value::Null);
        }
        if self.word("TRUE") {
            return Ok(Value::Bool(true));
        }
        if self.word("FALSE") {
            return Ok(Value::Bool(false));
        }
        let negative = self.symbol("-");
        let Some(Token::Value(value)) = self.peek() else {
            bail!("expected literal")
        };
        let value = if negative {
            ensure!(value.is_number(), "expected number after '-'");
            serde_json::from_str(&format!("-{value}"))?
        } else {
            value.clone()
        };
        self.pos += 1;
        Ok(value)
    }

    fn list<T>(&mut self, item: impl Fn(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let mut result = vec![item(self)?];
        while self.symbol(",") {
            result.push(item(self)?);
        }
        Ok(result)
    }

    fn op(&mut self) -> Result<Op> {
        let Some(Token::Symbol(symbol)) = self.peek() else {
            bail!("expected operator")
        };
        let op = match symbol.as_str() {
            "=" | "==" => Op::Eq,
            "!=" | "<>" => Op::Ne,
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "+" => Op::Add,
            "-" => Op::Sub,
            "*" => Op::Mul,
            "/" => Op::Div,
            _ => bail!("expected operator"),
        };
        self.pos += 1;
        Ok(op)
    }

    fn predicate(&mut self, depth: usize) -> Result<Predicate> {
        ensure!(depth < 64, "WHERE nesting limit exceeded");
        let mut groups = Vec::new();
        loop {
            let mut terms = Vec::new();
            loop {
                let term = if self.symbol("(") {
                    let inner = self.predicate(depth + 1)?;
                    self.expect_symbol(")")?;
                    inner
                } else {
                    self.comparison()?
                };
                terms.push(term);
                if !self.word("AND") {
                    break;
                }
            }
            groups.push(if terms.len() == 1 {
                terms.pop().unwrap()
            } else {
                Predicate::And(terms)
            });
            if !self.word("OR") {
                break;
            }
        }
        Ok(if groups.len() == 1 {
            groups.pop().unwrap()
        } else {
            Predicate::Or(groups)
        })
    }

    fn column_reference(&mut self) -> Result<String> {
        let mut name = self.name()?;
        if self.symbol(".") {
            name.push('.');
            name.push_str(&self.name()?);
        }
        Ok(name)
    }

    fn table_alias(&mut self) -> Result<Option<String>> {
        if self.word("AS") {
            Ok(Some(self.name()?))
        } else {
            Ok(None)
        }
    }

    fn comparison(&mut self) -> Result<Predicate> {
        let name = self.column_reference()?;
        if self.word("IS") {
            let negate = self.word("NOT");
            self.expect_word("NULL")?;
            return Ok(Predicate::Compare(
                name,
                if negate { Op::Ne } else { Op::Eq },
                Value::Null,
            ));
        }
        if self.word("BETWEEN") {
            let lower = self.value()?;
            self.expect_word("AND")?;
            let upper = self.value()?;
            return Ok(Predicate::And(vec![
                Predicate::Compare(name.clone(), Op::Ge, lower),
                Predicate::Compare(name, Op::Le, upper),
            ]));
        }
        if self.word("IN") {
            self.expect_symbol("(")?;
            let values = self.list(Self::value)?;
            self.expect_symbol(")")?;
            return Ok(Predicate::Or(
                values
                    .into_iter()
                    .map(|v| Predicate::Compare(name.clone(), Op::Eq, v))
                    .collect(),
            ));
        }
        let op = self.op()?;
        ensure!(
            matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge),
            "expected comparison operator"
        );
        Ok(Predicate::Compare(name, op, self.value()?))
    }

    fn filter(&mut self) -> Result<Option<Predicate>> {
        if self.word("WHERE") {
            Ok(Some(self.predicate(0)?))
        } else {
            Ok(None)
        }
    }

    fn row_id(&mut self) -> Result<u64> {
        self.value()?
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| anyhow!("THE requires a positive logical row ID"))
    }

    fn import_table(&mut self) -> Result<ImportTable> {
        self.expect_word("CREATE")?;
        self.expect_word("TABLE")?;
        if self.word("IF") {
            self.expect_word("NOT")?;
            self.expect_word("EXISTS")?;
        }
        let name = self.name()?;
        self.expect_symbol("(")?;
        let mut columns = Vec::new();
        let mut widths = Vec::new();
        let mut primary = None;
        loop {
            let position = self.pos;
            if self.word("CONSTRAINT") {
                self.name()?;
            }
            if self.word("PRIMARY") {
                self.expect_word("KEY")?;
                self.expect_symbol("(")?;
                let key = self.name()?;
                ensure!(
                    self.symbol(")"),
                    "only single-column PRIMARY KEY is supported"
                );
                ensure!(primary.replace(key).is_none(), "duplicate PRIMARY KEY");
            } else {
                ensure!(
                    self.pos == position,
                    "only named PRIMARY KEY constraints are supported"
                );
                let (column, width) = self.import_column()?;
                if let Some(width) = width {
                    widths.push((column.name.clone(), width));
                }
                columns.push(column);
            }
            if !self.symbol(",") {
                break;
            }
        }
        self.expect_symbol(")")
            .context("unsupported SQL constraint or type modifier")?;
        self.expect_symbol(";")?;
        if let Some(key) = primary {
            ensure!(
                !columns.iter().any(|column| column.primary),
                "duplicate PRIMARY KEY"
            );
            let column = columns
                .iter_mut()
                .find(|column| column.name == key)
                .ok_or_else(|| anyhow!("unknown PRIMARY KEY column {key}"))?;
            column.primary = true;
            column.required = true;
        }
        Ok(ImportTable {
            name,
            columns,
            widths,
        })
    }

    fn import_column(&mut self) -> Result<(Column, Option<u64>)> {
        let name = self.name()?;
        let sql_type = self.name()?.to_ascii_uppercase();
        let mut width = None;
        let kind = match sql_type.as_str() {
            "INT" | "INTEGER" | "SMALLINT" | "BIGINT" | "TINYINT" => Kind::Int,
            "FLOAT" | "REAL" => Kind::Float,
            "DOUBLE" => {
                self.word("PRECISION");
                Kind::Float
            }
            "TEXT" | "UUID" | "DATE" | "TIMESTAMP" | "DATETIME" => Kind::Text,
            "CHAR" | "CHARACTER" | "VARCHAR" => {
                if sql_type == "CHARACTER" {
                    self.word("VARYING");
                }
                if self.symbol("(") {
                    width = Some(
                        self.value()?
                            .as_u64()
                            .filter(|limit| *limit > 0)
                            .ok_or_else(|| anyhow!("character width must be a positive integer"))?,
                    );
                    self.expect_symbol(")")?;
                }
                Kind::Text
            }
            "BOOL" | "BOOLEAN" => Kind::Bool,
            "JSON" | "JSONB" => Kind::Json,
            "VECTOR" => {
                self.expect_symbol("(")?;
                let size = self
                    .value()?
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n != 0)
                    .ok_or_else(|| anyhow!("VECTOR requires a positive dimension"))?;
                self.expect_symbol(")")?;
                Kind::Vector(size)
            }
            _ => bail!("unsupported SQL type {sql_type}"),
        };
        let mut required = false;
        let mut primary = false;
        loop {
            if !required && self.word("NOT") {
                self.expect_word("NULL")?;
                required = true;
            } else if !primary && self.word("PRIMARY") {
                self.expect_word("KEY")?;
                primary = true;
            } else {
                break;
            }
        }
        Ok((
            Column {
                name,
                kind,
                required: required || primary,
                primary,
            },
            width,
        ))
    }

    fn column(&mut self) -> Result<Column> {
        let name = self.name()?;
        let kind = match self.name()?.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" => Kind::Int,
            "FLOAT" | "REAL" | "NUMBER" => Kind::Float,
            "TEXT" | "STRING" => Kind::Text,
            "BOOL" | "BOOLEAN" => Kind::Bool,
            "JSON" => Kind::Json,
            "VECTOR" => {
                self.expect_symbol("(")?;
                let size = self
                    .value()?
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .ok_or_else(|| anyhow!("VECTOR requires a positive dimension"))?;
                self.expect_symbol(")")?;
                Kind::Vector(size)
            }
            other => bail!("unknown column type {other}"),
        };
        let mut required = false;
        let mut primary = false;
        loop {
            if !required && self.word("NOT") {
                self.expect_word("NULL")?;
                required = true;
            } else if !primary && self.word("PRIMARY") {
                self.expect_word("KEY")?;
                primary = true;
            } else {
                break;
            }
        }
        Ok(Column {
            name,
            kind,
            required: required || primary,
            primary,
        })
    }

    fn statement(&mut self) -> Result<Statement> {
        if self.take(|t| *t == Token::Header) || self.word("SCHEMA") {
            let name = self.name()?;
            self.expect_symbol("(")?;
            let columns = self.list(Self::column)?;
            self.expect_symbol(")")?;
            Ok(Statement::Schema(name, columns))
        } else if matches!(self.peek(), Some(Token::Value(Value::Object(_)))) {
            Ok(Statement::Row(self.value()?))
        } else if self.word("EXPLAIN") {
            self.expect_word("ANALYZE")?;
            ensure!(
                matches!(self.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case("SELECT")),
                "EXPLAIN ANALYZE requires SELECT"
            );
            Ok(Statement::ExplainAnalyze(Box::new(self.statement()?)))
        } else if self.word("SELECT") {
            let mut options = SelectOptions::default();
            let columns = if self.symbol("*") {
                None
            } else {
                let mut columns = Vec::new();
                loop {
                    let start = self.pos;
                    if self.word("COUNT") && self.symbol("(") {
                        ensure!(options.count.is_none(), "only one COUNT(*) is supported");
                        self.expect_symbol("*")?;
                        self.expect_symbol(")")?;
                        options.count = Some(self.table_alias()?.unwrap_or_else(|| "count".into()));
                    } else {
                        self.pos = start;
                        columns.push(self.column_reference()?);
                    }
                    if !self.symbol(",") {
                        break;
                    }
                }
                (!columns.is_empty()).then_some(columns)
            };
            self.expect_word("FROM")?;
            let table = self.name()?;
            options.alias = self.table_alias()?;
            let left = self.word("LEFT");
            if left {
                self.word("OUTER");
            }
            let inner = !left && self.word("INNER");
            if left || inner || self.word("JOIN") {
                if left || inner {
                    self.expect_word("JOIN")?;
                }
                let table = self.name()?;
                let alias = self.table_alias()?;
                self.expect_word("ON")?;
                let first_key = self.column_reference()?;
                self.expect_symbol("=")?;
                let second_key = self.column_reference()?;
                options.join = Some(Join {
                    table,
                    alias,
                    left,
                    first_key,
                    second_key,
                });
            }
            ensure!(
                options.join.is_some() || options.alias.is_none(),
                "table aliases require JOIN"
            );
            let predicate = self.filter()?;
            if self.word("GROUP") {
                self.expect_word("BY")?;
                options.group = self.list(Self::column_reference)?;
                ensure!(
                    columns.as_ref().is_some_and(|columns| columns
                        .iter()
                        .all(|column| options.group.contains(column)))
                        || (columns.is_none() && options.count.is_some()),
                    "GROUP BY requires explicit grouped columns or COUNT(*)"
                );
                ensure!(
                    options
                        .count
                        .as_ref()
                        .is_none_or(|name| !options.group.contains(name)),
                    "COUNT alias conflicts with a grouped column"
                );
            } else {
                ensure!(
                    options.count.is_none() || columns.is_none(),
                    "SELECT columns with COUNT(*) requires GROUP BY"
                );
            }
            if self.word("ORDER") {
                ensure!(
                    options.count.is_none() || !options.group.is_empty(),
                    "ORDER BY is unsupported with ungrouped COUNT(*)"
                );
                self.expect_word("BY")?;
                options.order = self.list(|parser| {
                    let column = parser.column_reference()?;
                    let descending = parser.word("DESC");
                    if !descending {
                        parser.word("ASC");
                    }
                    Ok((column, descending))
                })?;
            }
            if !options.group.is_empty() {
                ensure!(
                    options
                        .order
                        .iter()
                        .all(|(name, _)| options.group.contains(name)
                            || options.count.as_ref() == Some(name)),
                    "ORDER BY requires a grouped column or COUNT alias"
                );
            }
            if self.word("LIMIT") {
                options.limit = Some(
                    self.value()?
                        .as_u64()
                        .ok_or_else(|| anyhow!("LIMIT requires a nonnegative integer"))?,
                );
            }
            if self.word("OFFSET") {
                options.offset = self
                    .value()?
                    .as_u64()
                    .ok_or_else(|| anyhow!("OFFSET requires a nonnegative integer"))?;
            }
            Ok(Statement::Select(table, columns, predicate, options))
        } else if self.word("FUSE") {
            let table = self.name()?;
            self.expect_word("THE")?;
            let source = self.row_id()?;
            self.expect_word("INTO")?;
            self.expect_word("THE")?;
            let target = self.row_id()?;
            let keep_target = self.word("KEEP");
            if keep_target {
                self.expect_word("TARGET")?;
            }
            Ok(Statement::Fuse {
                table,
                source,
                target,
                keep_target,
            })
        } else if self.word("DECAY") {
            let column = self.name()?;
            self.expect_word("FROM")?;
            let table = self.name()?;
            self.expect_word("BY")?;
            let factor = self
                .value()?
                .as_f64()
                .filter(|factor| (0.0..=1.0).contains(factor))
                .ok_or_else(|| anyhow!("DECAY factor must be a number between 0 and 1"))?;
            Ok(Statement::Decay {
                table,
                column,
                factor,
                predicate: self.filter()?,
            })
        } else if self.word("RESONATE") {
            let column = self.name()?;
            self.expect_word("FROM")?;
            let table = self.name()?;
            self.expect_word("WITH")?;
            let query = self.value()?;
            let predicate = self.filter()?;
            self.expect_word("LIMIT")?;
            let limit = self
                .value()?
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| anyhow!("LIMIT requires a positive integer"))?;
            Ok(Statement::Resonate {
                table,
                column,
                query,
                predicate,
                limit,
            })
        } else if self.word("INSERT") {
            self.expect_word("INTO")?;
            let table = self.name()?;
            let columns = if self.symbol("(") {
                let columns = self.list(Self::name)?;
                self.expect_symbol(")")?;
                Some(columns)
            } else {
                None
            };
            self.expect_word("VALUES")?;
            let data = if self.symbol("(") {
                let values = self.list(Self::value)?;
                self.expect_symbol(")")?;
                InsertData::Values(columns, values)
            } else {
                ensure!(
                    columns.is_none(),
                    "column lists require parenthesized VALUES"
                );
                let Value::Object(row) = self.value()? else {
                    bail!("expected a JSON object or parenthesized VALUES");
                };
                InsertData::Object(row)
            };
            Ok(Statement::Insert(table, data))
        } else if self.word("UPDATE") {
            let table = self.name()?;
            self.expect_word("SET")?;
            let assignments = self.list(|p| {
                let name = p.name()?;
                p.expect_symbol("=")?;
                Ok((name, p.value()?))
            })?;
            Ok(Statement::Update(table, assignments, self.filter()?))
        } else if self.word("DELETE") {
            self.expect_word("FROM")?;
            Ok(Statement::Delete(self.name()?, self.filter()?))
        } else if self.word("INJECT") {
            let column = self.name()?;
            self.expect_word("FROM")?;
            let table = self.name()?;
            self.expect_word("THE")?;
            let id = self.row_id()?;
            self.expect_word("WHO")?;
            let op = self.op()?;
            ensure!(
                matches!(op, Op::Eq | Op::Add | Op::Sub | Op::Mul | Op::Div),
                "unsupported WHO operator"
            );
            Ok(Statement::Inject(table, column, id, op, self.value()?))
        } else {
            bail!(
                "expected SCHEMA, JSONL row, SELECT, INSERT, UPDATE, DELETE, INJECT, RESONATE, DECAY, or FUSE"
            );
        }
    }
}
