use crate::aggregate::Aggregation;
use crate::scratch::{Scratch, ScratchSlot};
use crate::sort::Sorter;
use crate::syntax::{Column, InsertData, Kind, Op, Predicate, SelectOptions, Statement};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::rc::Rc;

pub type Row = Map<String, Value>;

#[derive(Debug, Serialize, Deserialize)]
pub enum Change {
    Schema { table: String, columns: Vec<Column> },
    Insert { table: String, id: u64, row: Row },
    Update { table: String, id: u64, row: Row },
    Delete { table: String, id: u64 },
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Location {
    pub source: &'static str,
    pub offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<usize>,
}

impl Location {
    pub fn new(source: &'static str, offset: u64) -> Self {
        Self {
            source,
            offset,
            change: None,
        }
    }

    pub fn log(offset: u64, change: usize) -> Self {
        Self {
            source: "log",
            offset,
            change: Some(change),
        }
    }
}

#[derive(Default)]
pub struct Database {
    tables: BTreeMap<String, Table>,
    active_schema: Option<String>,
    materialized: Option<(u64, u64)>,
}

struct Table {
    columns: Vec<Column>,
    column_names: HashSet<String>,
    rows: BTreeMap<u64, (Row, Location)>,
    keys: BTreeMap<String, u64>,
    next_id: u64,
}

impl Table {
    fn new(columns: Vec<Column>) -> Result<Self> {
        let mut column_names = HashSet::with_capacity(columns.len());
        ensure!(!columns.is_empty(), "SCHEMA requires columns");
        for column in &columns {
            ensure!(column.name != "_line", "_line is reserved");
            ensure!(
                column_names.insert(column.name.clone()),
                "duplicate column {}",
                column.name
            );
            if column.primary {
                ensure!(
                    column.required && matches!(column.kind, Kind::Int | Kind::Text),
                    "PRIMARY KEY requires INT or TEXT"
                );
            }
            ensure!(
                !matches!(column.kind, Kind::Vector(0)),
                "VECTOR requires a positive dimension"
            );
        }
        ensure!(
            columns.iter().filter(|c| c.primary).count() <= 1,
            "only one PRIMARY KEY is supported"
        );
        Ok(Self {
            columns,
            column_names,
            rows: BTreeMap::new(),
            keys: BTreeMap::new(),
            next_id: 1,
        })
    }

    fn has_column(&self, name: &str) -> bool {
        self.column_names.contains(name)
    }

    fn validate(&self, row: &Row) -> Result<()> {
        for name in row.keys() {
            ensure!(
                self.has_column(name),
                "unknown column {}",
                name.escape_debug()
            );
        }
        for column in &self.columns {
            let value = row.get(&column.name).unwrap_or(&Value::Null);
            let valid = match (&column.kind, value) {
                (_, Value::Null) => !column.required,
                (Kind::Int, value) => value.as_i64().is_some(),
                (Kind::Float, value) => value.is_number(),
                (Kind::Text, value) => value.is_string(),
                (Kind::Bool, value) => value.is_boolean(),
                (Kind::Json, _) => true,
                (Kind::Vector(size), Value::Array(values)) => {
                    values.len() == *size && values.iter().all(Value::is_number)
                }
                (Kind::Vector(_), _) => false,
            };
            ensure!(
                valid,
                "invalid value for column {} ({:?})",
                column.name,
                column.kind
            );
        }
        Ok(())
    }

    fn key(&self, row: &Row) -> Option<String> {
        self.columns
            .iter()
            .find(|column| column.primary)
            .map(|column| row[&column.name].to_string())
    }

    fn put(&mut self, id: u64, row: Row, location: Location) -> Result<()> {
        self.validate(&row)?;
        if let Some(key) = self.key(&row) {
            ensure!(
                self.keys.get(&key).is_none_or(|existing| *existing == id),
                "duplicate PRIMARY KEY {key}"
            );
            if let Some((old, _)) = self.rows.get(&id) {
                self.keys.remove(&self.key(old).unwrap());
            }
            self.keys.insert(key, id);
        }
        self.rows.insert(id, (row, location));
        Ok(())
    }

    fn filter(&self, predicate: &Predicate) -> Result<()> {
        match predicate {
            Predicate::Compare(name, _, _) => ensure!(
                name == "_line" || self.has_column(name),
                "unknown column {name}"
            ),
            Predicate::And(terms) | Predicate::Or(terms) => {
                for term in terms {
                    self.filter(term)?;
                }
            }
        }
        Ok(())
    }

    // The outer option indicates whether an index applies. The inner option is its hit.
    fn indexed_candidate(&self, predicate: &Predicate) -> Option<Option<u64>> {
        match predicate {
            Predicate::And(terms) => terms.iter().find_map(|term| self.indexed_candidate(term)),
            Predicate::Compare(name, Op::Eq, value) if name == "_line" => {
                Some(integral_literal(value).and_then(|id| u64::try_from(id).ok()))
            }
            Predicate::Compare(name, Op::Eq, value) => {
                let column = self
                    .columns
                    .iter()
                    .find(|column| column.primary && column.name == *name)?;
                let key = match column.kind {
                    Kind::Int => integral_literal(value)
                        .and_then(|key| i64::try_from(key).ok())
                        .map(|key| key.to_string()),
                    Kind::Text => value.is_string().then(|| value.to_string()),
                    _ => unreachable!("schema validates primary key types"),
                };
                Some(key.and_then(|key| self.keys.get(&key).copied()))
            }
            _ => None,
        }
    }

    fn matching(&self, predicate: Option<&Predicate>) -> Result<Vec<u64>> {
        if let Some(predicate) = predicate {
            self.filter(predicate)?;
            if let Some(candidate) = self.indexed_candidate(predicate) {
                return Ok(candidate
                    .into_iter()
                    .filter(|id| {
                        self.rows
                            .get(id)
                            .is_some_and(|(row, _)| matches(predicate, *id, row))
                    })
                    .collect());
            }
        }
        Ok(self
            .rows
            .iter()
            .filter(|(id, (row, _))| predicate.is_none_or(|p| matches(p, **id, row)))
            .map(|(id, _)| *id)
            .collect())
    }
}

pub(crate) fn field(id: u64, row: &Row, name: &str) -> Value {
    row.get(name).cloned().unwrap_or_else(|| {
        if name == "_line" {
            Value::from(id)
        } else {
            Value::Null
        }
    })
}

pub(crate) fn project(id: u64, row: &Row, columns: Option<&[String]>) -> Row {
    match columns {
        None => row.clone(),
        Some(columns) => columns
            .iter()
            .map(|name| (name.clone(), field(id, row, name)))
            .collect(),
    }
}

fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

pub(crate) fn integral_literal(value: &Value) -> Option<i128> {
    integer(value).or_else(|| {
        let float = value.as_f64()?;
        (float.fract() == 0.0).then_some(float as i128)
    })
}

fn int_float_cmp(left: i128, right: f64) -> Ordering {
    // All integral JSON numbers fit in i128, with room to compare against
    // the saturated conversion of floats outside that range.
    let truncated = right as i128;
    left.cmp(&truncated).then_with(|| {
        if right.fract() > 0.0 {
            Ordering::Less
        } else if right.fract() < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })
}

fn unit_vector(value: &Value, dimension: usize) -> Option<Vec<f64>> {
    let values = value.as_array()?;
    if values.len() != dimension {
        return None;
    }
    let mut vector = values
        .iter()
        .map(Value::as_f64)
        .collect::<Option<Vec<_>>>()?;
    // Scale first so squaring very large or very small components is safe.
    let scale = vector.iter().map(|value| value.abs()).fold(0.0, f64::max);
    if scale == 0.0 {
        return None;
    }
    for value in &mut vector {
        *value /= scale;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    for value in &mut vector {
        *value /= norm;
    }
    Some(vector)
}

fn compare(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (integer(left), integer(right)) {
            (Some(a), Some(b)) => Some(a.cmp(&b)),
            (Some(a), None) => Some(int_float_cmp(a, b.as_f64()?)),
            (None, Some(b)) => Some(int_float_cmp(b, a.as_f64()?).reverse()),
            (None, None) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        },
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

struct SqlKey(Vec<Value>);

impl Ord for SqlKey {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(&other.0) {
            let order = order_value(a, b);
            if !order.is_eq() {
                return order;
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

impl PartialOrd for SqlKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SqlKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for SqlKey {}

// Stable source IDs survive changes to join partitioning and spill order.
pub(crate) type RowOrder = (u64, u64);
pub(crate) type OrderedRow = (RowOrder, Vec<Value>, Value);

pub(crate) type JoinVisitor<'a> = dyn FnMut(u64, &Row, Option<(u64, &Row)>) -> Result<()> + 'a;

pub(crate) fn join_key_names<'a>(
    table: &'a str,
    options: &'a SelectOptions,
) -> Result<(&'a str, &'a str)> {
    let join = options.join.as_ref().expect("JOIN options are present");
    let left_alias = options.alias.as_deref().unwrap_or(table);
    let right_alias = join.alias.as_deref().unwrap_or(&join.table);
    ensure!(
        left_alias != right_alias,
        "JOIN requires distinct table names or aliases"
    );
    let split = |reference: &'a str| -> Result<(bool, &'a str)> {
        let (alias, key) = reference
            .split_once('.')
            .ok_or_else(|| anyhow!("JOIN ON requires qualified column names"))?;
        if alias == left_alias {
            Ok((true, key))
        } else if alias == right_alias {
            Ok((false, key))
        } else {
            bail!("unknown JOIN column {reference}")
        }
    };
    let (first_left, first) = split(&join.first_key)?;
    let (second_left, second) = split(&join.second_key)?;
    ensure!(
        first_left != second_left,
        "JOIN ON must compare columns from opposite tables"
    );
    Ok(if first_left {
        (first, second)
    } else {
        (second, first)
    })
}

#[derive(Default)]
pub(crate) struct ResultSpill {
    pub group: u64,
    pub sort: u64,
}

impl ResultSpill {
    pub(crate) fn bytes(&self) -> u64 {
        self.group + self.sort
    }
}

pub(crate) struct SelectRows<'a> {
    columns: Option<&'a [String]>,
    options: &'a SelectOptions,
    matched: u64,
    ordered: Option<Sorter<'a>>,
    scratch: ScratchSlot,
    keep: Option<usize>,
    groups: Option<Aggregation>,
}

impl<'a> SelectRows<'a> {
    pub(crate) fn new(columns: Option<&'a [String]>, options: &'a SelectOptions) -> Result<Self> {
        Self::with_scratch(columns, options, None)
    }

    pub(crate) fn with_scratch(
        columns: Option<&'a [String]>,
        options: &'a SelectOptions,
        scratch: Option<Rc<Scratch>>,
    ) -> Result<Self> {
        Self::with_slot(columns, options, ScratchSlot::new(scratch), false)
    }

    fn with_slot(
        columns: Option<&'a [String]>,
        options: &'a SelectOptions,
        scratch: ScratchSlot,
        restore_source_order: bool,
    ) -> Result<Self> {
        let keep = options.limit.and_then(|limit| {
            if limit == 0 {
                Some(0)
            } else {
                usize::try_from(options.offset.checked_add(limit)?).ok()
            }
        });
        Ok(Self {
            columns,
            options,
            matched: 0,
            ordered: (options.group.is_empty()
                && options.count.is_none()
                && (!options.order.is_empty() || restore_source_order))
                .then(|| Sorter::new(&options.order, keep, scratch.clone()))
                .transpose()?,
            groups: (!options.group.is_empty())
                .then(|| Aggregation::new(scratch.clone()))
                .transpose()?,
            scratch,
            keep,
        })
    }

    pub(crate) fn set_count(&mut self, count: u64) {
        self.matched = count;
    }

    fn in_window(&self, index: u64) -> bool {
        index
            .checked_sub(self.options.offset)
            .is_some_and(|position| self.options.limit.is_none_or(|limit| position < limit))
    }

    pub(crate) fn push(&mut self, id: u64, row: &Row) -> Result<Option<Value>> {
        self.push_ordered(id, row, (id, 0))
    }

    fn push_ordered(&mut self, id: u64, row: &Row, order: RowOrder) -> Result<Option<Value>> {
        let position = self.matched;
        self.matched += 1;
        if self.keep == Some(0) {
            return Ok(None);
        }
        if let Some(groups) = &mut self.groups {
            groups.push(
                order,
                self.options
                    .group
                    .iter()
                    .map(|name| field(id, row, name))
                    .collect(),
            )?;
            return Ok(None);
        }
        if self.options.count.is_some() {
            return Ok(None);
        }
        if let Some(sorter) = &mut self.ordered {
            let keys = self
                .options
                .order
                .iter()
                .map(|(column, _)| field(id, row, column))
                .collect();
            sorter.push((order, keys, Value::Object(project(id, row, self.columns))))?;
            return Ok(None);
        }
        Ok(self
            .in_window(position)
            .then(|| Value::Object(project(id, row, self.columns))))
    }

    pub(crate) fn finish_with(
        mut self,
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<ResultSpill> {
        if let Some(groups) = self.groups.take() {
            let mut columns = self.columns.unwrap_or_default().to_vec();
            columns.extend(self.options.count.iter().cloned());
            let mut options = self.options.clone();
            options.group.clear();
            options.count = None;
            let mut results =
                SelectRows::with_slot(Some(&columns), &options, self.scratch.clone(), true)?;
            let written = groups.finish(|id, count, values| {
                let mut row: Row = self.options.group.iter().cloned().zip(values).collect();
                if let Some(name) = &self.options.count {
                    row.insert(name.clone(), Value::from(count));
                }
                if let Some(value) = results.push_ordered(id.0, &row, id)? {
                    emit(value)?;
                }
                Ok(())
            })?;
            return Ok(ResultSpill {
                group: written,
                sort: results.finish_sort(emit)?,
            });
        }
        if let Some(name) = &self.options.count {
            if self.in_window(0) {
                emit(Value::Object(
                    [(name.clone(), Value::from(self.matched))]
                        .into_iter()
                        .collect(),
                ))?;
            }
        } else {
            return Ok(ResultSpill {
                group: 0,
                sort: self.finish_sort(emit)?,
            });
        }
        Ok(ResultSpill::default())
    }

    fn finish_sort(self, emit: &mut impl FnMut(Value) -> Result<()>) -> Result<u64> {
        let skip = usize::try_from(self.options.offset).unwrap_or(usize::MAX);
        let take = self
            .options
            .limit
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(usize::MAX);
        match self.ordered {
            Some(sorter) => sorter.finish(skip, take, emit),
            None => Ok(0),
        }
    }
}

pub(crate) fn order_row(
    left: &OrderedRow,
    right: &OrderedRow,
    columns: &[(String, bool)],
) -> Ordering {
    for ((a, b), (_, descending)) in left.1.iter().zip(&right.1).zip(columns) {
        let order = order_value(a, b);
        if !order.is_eq() {
            return if *descending { order.reverse() } else { order };
        }
    }
    left.0.cmp(&right.0)
}

// serde_json compares the two floating-point zeros as equal, even inside
// arrays and objects. Preserve all other nested numeric representations.
pub(crate) fn nested_json_key(value: &Value) -> String {
    fn normalize(value: &mut Value) {
        match value {
            Value::Number(n) if n.is_f64() && n.as_f64() == Some(0.0) => {
                *value = Value::from(0.0);
            }
            Value::Array(values) => values.iter_mut().for_each(normalize),
            Value::Object(values) => values.values_mut().for_each(normalize),
            _ => {}
        }
    }

    let mut value = value.clone();
    normalize(&mut value);
    value.to_string()
}

fn values_equal(a: &Value, b: &Value) -> bool {
    a == b || compare(a, b).is_some_and(Ordering::is_eq)
}

fn order_value(a: &Value, b: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Number(_) => 2,
            Value::String(_) => 3,
            Value::Array(_) => 4,
            Value::Object(_) => 5,
        }
    }
    rank(a).cmp(&rank(b)).then_with(|| {
        compare(a, b).unwrap_or_else(|| match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            _ => nested_json_key(a).cmp(&nested_json_key(b)),
        })
    })
}

pub(crate) fn select_references(
    columns: Option<&[String]>,
    predicate: Option<&Predicate>,
    options: &SelectOptions,
) -> BTreeSet<String> {
    let mut names: BTreeSet<_> = columns
        .into_iter()
        .flatten()
        .chain(&options.group)
        .chain(options.order.iter().map(|(name, _)| name))
        .cloned()
        .collect();
    if let Some(predicate) = predicate {
        predicate_columns(predicate, &mut |name| {
            names.insert(name.to_owned());
            Ok(())
        })
        .expect("collecting predicate columns cannot fail");
    }
    names
}

fn predicate_columns(
    predicate: &Predicate,
    validate: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    match predicate {
        Predicate::Compare(name, ..) => validate(name),
        Predicate::And(terms) | Predicate::Or(terms) => terms
            .iter()
            .try_for_each(|term| predicate_columns(term, validate)),
    }
}

pub(crate) fn matches(predicate: &Predicate, id: u64, row: &Row) -> bool {
    match predicate {
        Predicate::And(terms) => terms.iter().all(|term| matches(term, id, row)),
        Predicate::Or(terms) => terms.iter().any(|term| matches(term, id, row)),
        Predicate::Compare(name, op, right) => {
            let left = field(id, row, name);
            let order = compare(&left, right);
            let equal = left == *right || order == Some(Ordering::Equal);
            match op {
                Op::Eq => equal,
                Op::Ne => !equal,
                Op::Lt => order.is_some_and(Ordering::is_lt),
                Op::Le => order.is_some_and(Ordering::is_le),
                Op::Gt => order.is_some_and(Ordering::is_gt),
                Op::Ge => order.is_some_and(Ordering::is_ge),
                _ => unreachable!("parser validates comparisons"),
            }
        }
    }
}

fn inject(old: Value, op: Op, value: Value, kind: &Kind) -> Result<Value> {
    if matches!(op, Op::Eq) {
        return Ok(value);
    }
    if matches!(op, Op::Add) {
        match (old.as_str(), value.as_str()) {
            (Some(old), Some(value)) => return Ok(Value::String(format!("{old}{value}"))),
            _ if old.is_array() && matches!(kind, Kind::Json) => {
                let mut items = old.as_array().unwrap().clone();
                match value {
                    Value::Array(values) => items.extend(values),
                    value => items.push(value),
                }
                return Ok(Value::Array(items));
            }
            _ => {}
        }
    }
    if let (Some(a), Some(b)) = (integer(&old), integer(&value)) {
        let result = match op {
            Op::Add => a.checked_add(b),
            Op::Sub => a.checked_sub(b),
            Op::Mul => a.checked_mul(b),
            Op::Div => {
                ensure!(b != 0, "division by zero");
                if a % b == 0 {
                    Some(a / b)
                } else {
                    ensure!(!matches!(kind, Kind::Int), "non-integral division for INT");
                    None
                }
            }
            _ => unreachable!(),
        };
        if !matches!(op, Op::Div) || result.is_some() {
            if let Some(result) = result {
                if let Ok(number) = i64::try_from(result) {
                    return Ok(Value::from(number));
                }
                if let Ok(number) = u64::try_from(result) {
                    return Ok(Value::from(number));
                }
            }
            ensure!(!matches!(kind, Kind::Int | Kind::Json), "integer overflow");
            // FLOAT can represent finite results outside the integer range.
            if let Some(result) = result {
                return Ok(Value::from(result as f64));
            }
        }
    }
    let a = old
        .as_f64()
        .ok_or_else(|| anyhow!("WHO operator requires compatible operands"))?;
    let b = value
        .as_f64()
        .ok_or_else(|| anyhow!("WHO operator requires compatible operands"))?;
    let result = match op {
        Op::Add => a + b,
        Op::Sub => a - b,
        Op::Mul => a * b,
        Op::Div => {
            ensure!(b != 0.0, "division by zero");
            a / b
        }
        _ => unreachable!(),
    };
    Ok(Value::Number(
        Number::from_f64(result).ok_or_else(|| anyhow!("non-finite result"))?,
    ))
}

impl Database {
    fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(name)
            .ok_or_else(|| anyhow!("unknown table {name}"))
    }

    fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(name)
            .ok_or_else(|| anyhow!("unknown table {name}"))
    }

    pub(crate) fn register_base_row(
        &mut self,
        row: &Row,
        remember_key: bool,
        extract_key: bool,
    ) -> Result<(&str, u64, Option<String>)> {
        let name = self
            .active_schema
            .as_deref()
            .ok_or_else(|| anyhow!("JSONL row requires a preceding SCHEMA"))?;
        let table = self.tables.get_mut(name).expect("active table exists");
        table.validate(row)?;
        let id = table.next_id;
        table.next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow!("row ID exhausted"))?;
        let key = (remember_key || extract_key)
            .then(|| table.key(row))
            .flatten();
        if remember_key && let Some(key) = &key {
            ensure!(!table.keys.contains_key(key), "duplicate PRIMARY KEY {key}");
            table.keys.insert(key.clone(), id);
        }
        Ok((name, id, key))
    }

    pub(crate) fn validate_group_order(&self, table: &str, options: &SelectOptions) -> Result<()> {
        self.validate_select(table, Some(&options.group), None)?;
        if options.group.is_empty() {
            let names: Vec<_> = options.order.iter().map(|(name, _)| name.clone()).collect();
            self.validate_select(table, Some(&names), None)?;
        }
        Ok(())
    }

    pub(crate) fn primary_column(&self, name: &str) -> Result<Option<&Column>> {
        Ok(self
            .table(name)?
            .columns
            .iter()
            .find(|column| column.primary))
    }

    pub(crate) fn limit_materialization(&mut self, bytes: u64) {
        self.materialized = Some((0, bytes));
    }

    fn charge_row(&mut self, row: &Row) -> Result<()> {
        if let Some((used, limit)) = &mut self.materialized {
            let size = row
                .iter()
                .map(|(name, value)| 96 + name.len() as u64 + crate::aggregate::value_weight(value))
                .sum::<u64>();
            *used = used.saturating_add(size).saturating_add(128);
            ensure!(
                *used <= *limit,
                "materialization memory budget exceeded ({limit} bytes)"
            );
        }
        Ok(())
    }

    pub(crate) fn retain_base_row(
        &mut self,
        name: &str,
        id: u64,
        row: Row,
        offset: u64,
    ) -> Result<()> {
        self.charge_row(&row)?;
        self.tables
            .get_mut(name)
            .expect("registered table exists")
            .rows
            .insert(id, (row, Location::new("base", offset)));
        Ok(())
    }

    pub(crate) fn validate_select(
        &self,
        name: &str,
        columns: Option<&[String]>,
        predicate: Option<&Predicate>,
    ) -> Result<()> {
        let table = self.table(name)?;
        for name in columns.into_iter().flatten() {
            ensure!(
                name == "_line" || table.has_column(name),
                "unknown column {name}"
            );
        }
        if let Some(predicate) = predicate {
            table.filter(predicate)?;
        }
        Ok(())
    }

    pub(crate) fn base_row_counts(&self) -> impl Iterator<Item = (&str, u64)> {
        self.tables
            .iter()
            .map(|(name, table)| (name.as_str(), table.next_id - 1))
    }

    pub fn index(&self) -> BTreeMap<&str, BTreeMap<u64, Location>> {
        self.tables
            .iter()
            .map(|(name, table)| {
                (
                    name.as_str(),
                    table
                        .rows
                        .iter()
                        .map(|(id, (_, location))| (*id, *location))
                        .collect(),
                )
            })
            .collect()
    }

    pub fn locate(&mut self, changes: &[Change], offset: u64) {
        for (position, change) in changes.iter().enumerate().rev() {
            if let Change::Insert { table, id, .. } | Change::Update { table, id, .. } = change
                && let Some((_, location)) = self.tables.get_mut(table).unwrap().rows.get_mut(id)
                && location.source == "pending"
            {
                *location = Location::log(offset, position);
            }
        }
    }

    pub fn apply(&mut self, change: &Change, location: Location) -> Result<()> {
        if let Change::Insert { row, .. } | Change::Update { row, .. } = change {
            crate::import_json::validate_row_nodes(row)?;
            crate::limits::measure(row, crate::limits::record()? - 1)?;
            self.charge_row(row)?;
        }
        match change {
            Change::Schema { table, columns } => {
                ensure!(!self.tables.contains_key(table), "duplicate table {table}");
                self.tables
                    .insert(table.clone(), Table::new(columns.clone())?);
                self.active_schema = Some(table.clone());
            }
            Change::Insert { table, id, row } => {
                let table = self.table_mut(table)?;
                ensure!(*id == table.next_id, "unexpected row ID {id}");
                let next = id
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("row ID exhausted"))?;
                table.put(*id, row.clone(), location)?;
                table.next_id = next;
            }
            Change::Update { table, id, row } => {
                let table = self.table_mut(table)?;
                ensure!(table.rows.contains_key(id), "unknown logical row ID {id}");
                table.put(*id, row.clone(), location)?;
            }
            Change::Delete { table, id } => {
                let table = self.table_mut(table)?;
                let (row, _) = table
                    .rows
                    .remove(id)
                    .ok_or_else(|| anyhow!("unknown logical row ID {id}"))?;
                if let Some(key) = table.key(&row) {
                    table.keys.remove(&key);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn join_rows<R: Borrow<Row>>(
        &self,
        table: &str,
        columns: Option<&[String]>,
        predicate: Option<&Predicate>,
        options: &SelectOptions,
        right_rows: impl IntoIterator<Item = (u64, R)>,
        scan_left: impl FnOnce(&mut dyn FnMut(u64, &Row) -> Result<()>) -> Result<()>,
        emit: impl FnMut(Value) -> Result<()>,
    ) -> Result<ResultSpill> {
        self.join_with_pairs(
            table,
            columns,
            predicate,
            options,
            None,
            false,
            |left_key, right_key, visit| {
                let join = options.join.as_ref().expect("JOIN options are present");
                let mut index: BTreeMap<SqlKey, Vec<_>> = BTreeMap::new();
                for (id, row) in right_rows {
                    let key = field(id, row.borrow(), right_key);
                    if !key.is_null() {
                        index.entry(SqlKey(vec![key])).or_default().push((id, row));
                    }
                }
                scan_left(&mut |id, row| {
                    let key = SqlKey(vec![field(id, row, left_key)]);
                    if let Some(hits) = index.get(&key) {
                        for (right_id, right_row) in hits {
                            visit(id, row, Some((*right_id, right_row.borrow())))?;
                        }
                    } else if join.left {
                        visit(id, row, None)?;
                    }
                    Ok(())
                })
            },
            emit,
        )
    }

    pub(crate) fn join_with_pairs(
        &self,
        table: &str,
        columns: Option<&[String]>,
        predicate: Option<&Predicate>,
        options: &SelectOptions,
        scratch: Option<Rc<Scratch>>,
        restore_source_order: bool,
        pairs: impl FnOnce(&str, &str, &mut JoinVisitor<'_>) -> Result<()>,
        mut emit: impl FnMut(Value) -> Result<()>,
    ) -> Result<ResultSpill> {
        let join = options.join.as_ref().expect("JOIN options are present");
        let left = self.table(table)?;
        let right = self.table(&join.table)?;
        let left_alias = options.alias.as_deref().unwrap_or(table);
        let right_alias = join.alias.as_deref().unwrap_or(&join.table);
        ensure!(
            left_alias != right_alias,
            "JOIN requires distinct table names or aliases"
        );

        let left_fields: Vec<_> = left
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .chain(["_line"])
            .collect();
        let right_fields: Vec<_> = right
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .chain(["_line"])
            .collect();
        let (left_key, right_key) = join_key_names(table, options)?;
        ensure!(
            left_fields.contains(&left_key),
            "unknown JOIN column {left_alias}.{left_key}"
        );
        ensure!(
            right_fields.contains(&right_key),
            "unknown JOIN column {right_alias}.{right_key}"
        );

        let mut bindings = Vec::new();
        let mut qualified = Vec::new();
        for (from_left, alias, fields, other) in [
            (true, left_alias, &left_fields, &right_fields),
            (false, right_alias, &right_fields, &left_fields),
        ] {
            for &name in fields {
                let full = format!("{alias}.{name}");
                qualified.push(full.clone());
                bindings.push((full, from_left, name));
                if !other.contains(&name) {
                    bindings.push((name.to_owned(), from_left, name));
                }
            }
        }
        let validation_columns = bindings
            .iter()
            .map(|(name, _, _)| Column {
                name: name.clone(),
                kind: Kind::Json,
                required: false,
                primary: false,
            })
            .collect();
        let validation = Table::new(validation_columns)?;
        for name in columns.into_iter().flatten().chain(&options.group) {
            ensure!(
                validation.has_column(name),
                "unknown or ambiguous JOIN column {name}"
            );
        }
        if options.group.is_empty() {
            for (name, _) in &options.order {
                ensure!(
                    validation.has_column(name),
                    "unknown or ambiguous JOIN column {name}"
                );
            }
        }
        if let Some(predicate) = predicate {
            // Joined row numbers are internal. The two source IDs are qualified columns.
            predicate_columns(predicate, &mut |name| {
                ensure!(
                    validation.has_column(name),
                    "unknown or ambiguous JOIN column {name}"
                );
                Ok(())
            })?;
        }

        if columns.is_some() || options.count.is_some() {
            let needed = select_references(columns, predicate, options);
            bindings.retain(|(binding, _, _)| needed.contains(binding));
        }
        let projection =
            columns.or_else(|| options.count.is_none().then_some(qualified.as_slice()));
        let mut results = SelectRows::with_slot(
            projection,
            options,
            ScratchSlot::new(scratch),
            restore_source_order,
        )?;
        let mut sequence = 0;
        pairs(left_key, right_key, &mut |left_id, left_row, hit| {
            sequence += 1;
            let row: Row = bindings
                .iter()
                .map(|(alias, from_left, name)| {
                    let value = if *from_left {
                        field(left_id, left_row, name)
                    } else {
                        hit.map_or(Value::Null, |(id, row)| field(id, row, name))
                    };
                    (alias.clone(), value)
                })
                .collect();
            if predicate.is_none_or(|filter| matches(filter, sequence, &row))
                && let Some(value) =
                    results.push_ordered(sequence, &row, (left_id, hit.map_or(0, |(id, _)| id)))?
            {
                emit(value)?;
            }
            Ok(())
        })?;
        results.finish_with(&mut emit)
    }

    pub fn execute(
        &mut self,
        statement: Statement,
        location: Location,
    ) -> Result<(Vec<Change>, Vec<Value>, ResultSpill)> {
        let mut output = Vec::new();
        let mut spill = ResultSpill::default();
        let changes = match statement {
            Statement::ExplainAnalyze(_) => bail!("EXPLAIN ANALYZE requires a database query"),
            Statement::Schema(table, columns) => vec![Change::Schema { table, columns }],
            Statement::Row(Value::Object(row)) => {
                let table = self
                    .active_schema
                    .clone()
                    .ok_or_else(|| anyhow!("JSONL row requires a preceding SCHEMA"))?;
                vec![Change::Insert {
                    id: self.table(&table)?.next_id,
                    table,
                    row,
                }]
            }
            Statement::Row(_) => bail!("expected JSON object"),
            Statement::Insert(table, data) => {
                let state = self.table(&table)?;
                let row = match data {
                    InsertData::Object(row) => row,
                    InsertData::Values(columns, values) => {
                        let columns = columns.unwrap_or_else(|| {
                            state.columns.iter().map(|c| c.name.clone()).collect()
                        });
                        ensure!(
                            columns.len() == values.len(),
                            "column and value counts differ"
                        );
                        let mut row = Row::new();
                        for (name, value) in columns.into_iter().zip(values) {
                            ensure!(
                                row.insert(name.clone(), value).is_none(),
                                "duplicate column {name}"
                            );
                        }
                        row
                    }
                };
                vec![Change::Insert {
                    id: state.next_id,
                    table,
                    row,
                }]
            }
            Statement::Fuse {
                table,
                source,
                target,
                keep_target,
            } => {
                ensure!(
                    source != target,
                    "FUSE requires two distinct logical row IDs"
                );
                let state = self.table(&table)?;
                let (source_row, _) = state
                    .rows
                    .get(&source)
                    .ok_or_else(|| anyhow!("unknown logical row ID {source}"))?;
                let (target_row, _) = state
                    .rows
                    .get(&target)
                    .ok_or_else(|| anyhow!("unknown logical row ID {target}"))?;
                let mut row = target_row.clone();
                for column in state.columns.iter().filter(|column| !column.primary) {
                    let Some(value) = source_row
                        .get(&column.name)
                        .filter(|value| !value.is_null())
                    else {
                        continue;
                    };
                    match row.get(&column.name) {
                        None | Some(Value::Null) => {
                            row.insert(column.name.clone(), value.clone());
                        }
                        Some(existing) => ensure!(
                            keep_target || values_equal(existing, value),
                            "FUSE conflict in column {}",
                            column.name
                        ),
                    }
                }
                vec![
                    Change::Update {
                        table: table.clone(),
                        id: target,
                        row,
                    },
                    Change::Delete { table, id: source },
                ]
            }
            Statement::Decay {
                table,
                column,
                factor,
                predicate,
            } => {
                let state = self.table(&table)?;
                let definition = state
                    .columns
                    .iter()
                    .find(|item| item.name == column)
                    .ok_or_else(|| anyhow!("unknown column {column}"))?;
                ensure!(
                    matches!(definition.kind, Kind::Float | Kind::Vector(_)),
                    "DECAY requires a FLOAT or VECTOR column"
                );
                let ids = state.matching(predicate.as_ref())?;
                let mut changes = Vec::new();
                for id in ids {
                    let (row, _) = &state.rows[&id];
                    let Some(value) = row
                        .get(&column)
                        .filter(|value| !value.is_null() && factor != 1.0)
                    else {
                        continue;
                    };
                    let scale = |value: &Value| {
                        let number = value.as_f64().expect("validated numeric value");
                        Value::from(number * factor)
                    };
                    let updated = match value {
                        Value::Array(values) => Value::Array(values.iter().map(scale).collect()),
                        value => scale(value),
                    };
                    if updated != *value {
                        let mut row = row.clone();
                        row.insert(column.clone(), updated);
                        changes.push(Change::Update {
                            table: table.clone(),
                            id,
                            row,
                        });
                    }
                }
                changes
            }
            Statement::Resonate {
                table,
                column,
                query,
                predicate,
                limit,
            } => {
                let state = self.table(&table)?;
                let definition = state
                    .columns
                    .iter()
                    .find(|item| item.name == column)
                    .ok_or_else(|| anyhow!("unknown column {column}"))?;
                let Kind::Vector(dimension) = definition.kind else {
                    bail!("RESONATE requires a VECTOR column");
                };
                let query = unit_vector(&query, dimension).ok_or_else(|| {
                    anyhow!("RESONATE requires a nonzero VECTOR({dimension}) query")
                })?;
                let mut scores = Vec::new();
                for id in state.matching(predicate.as_ref())? {
                    let (row, _) = &state.rows[&id];
                    if let Some(vector) = row
                        .get(&column)
                        .and_then(|value| unit_vector(value, dimension))
                    {
                        let score = query
                            .iter()
                            .zip(vector)
                            .map(|(a, b)| a * b)
                            .sum::<f64>()
                            .clamp(-1.0, 1.0);
                        scores.push((id, score));
                    }
                }
                scores.sort_unstable_by(|(a_id, a), (b_id, b)| {
                    b.partial_cmp(a)
                        .expect("cosine scores are finite")
                        .then_with(|| a_id.cmp(b_id))
                });
                output.extend(
                    scores
                        .into_iter()
                        .take(limit)
                        .map(|(id, score)| serde_json::json!({ "_line": id, "_score": score })),
                );
                Vec::new()
            }
            Statement::Select(table, columns, predicate, options) => {
                let result_limit = crate::limits::materialized()?;
                let mut result_bytes = 0;
                if let Some(join) = &options.join {
                    let left = self.table(&table)?;
                    let right = self.table(&join.table)?;
                    spill = self.join_rows(
                        &table,
                        columns.as_deref(),
                        predicate.as_ref(),
                        &options,
                        right.rows.iter().map(|(&id, (row, _))| (id, row)),
                        |consume| {
                            for (&id, (row, _)) in &left.rows {
                                consume(id, row)?;
                            }
                            Ok(())
                        },
                        |value| {
                            crate::limits::charge_result(&mut result_bytes, result_limit, &value)?;
                            output.push(value);
                            Ok(())
                        },
                    )?;
                    return Ok((Vec::new(), output, spill));
                }
                self.validate_select(&table, columns.as_deref(), predicate.as_ref())?;
                self.validate_group_order(&table, &options)?;
                let state = self.table(&table)?;
                let mut results = SelectRows::new(columns.as_deref(), &options)?;
                for id in state.matching(predicate.as_ref())? {
                    let (row, _) = &state.rows[&id];
                    if let Some(value) = results.push(id, row)? {
                        crate::limits::charge_result(&mut result_bytes, result_limit, &value)?;
                        output.push(value);
                    }
                }
                spill = results.finish_with(&mut |value| {
                    crate::limits::charge_result(&mut result_bytes, result_limit, &value)?;
                    output.push(value);
                    Ok(())
                })?;
                Vec::new()
            }
            Statement::Update(table, assignments, predicate) => {
                let state = self.table(&table)?;
                let mut seen = BTreeSet::new();
                for (name, _) in &assignments {
                    ensure!(state.has_column(name), "unknown column {name}");
                    ensure!(seen.insert(name), "duplicate assignment {name}");
                }
                let ids = state.matching(predicate.as_ref())?;
                let mut changes = Vec::new();
                for id in ids {
                    let mut row = self.table(&table)?.rows[&id].0.clone();
                    row.extend(assignments.iter().cloned());
                    let change = Change::Update {
                        table: table.clone(),
                        id,
                        row,
                    };
                    self.apply(&change, location)?;
                    changes.push(change);
                }
                return Ok((changes, output, spill));
            }
            Statement::Delete(table, predicate) => self
                .table(&table)?
                .matching(predicate.as_ref())?
                .into_iter()
                .map(|id| Change::Delete {
                    table: table.clone(),
                    id,
                })
                .collect(),
            Statement::Inject(table, name, id, op, value) => {
                let state = self.table(&table)?;
                let column = state
                    .columns
                    .iter()
                    .find(|column| column.name == name)
                    .ok_or_else(|| anyhow!("unknown column {name}"))?;
                let (mut row, _) = state
                    .rows
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown logical row ID {id}"))?;
                let value = inject(field(id, &row, &name), op, value, &column.kind)?;
                row.insert(name, value);
                vec![Change::Update { table, id, row }]
            }
        };
        for change in &changes {
            self.apply(change, location)?;
        }
        Ok((changes, output, spill))
    }
}
