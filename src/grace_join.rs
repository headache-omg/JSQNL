use crate::engine::{
    JoinVisitor, ResultSpill, Row, field, integral_literal, join_key_names, select_references,
};
use crate::scratch::{KeyValidation, PartitionWriters, Scratch};
use crate::storage::{BaseRead, walk_base};
use crate::syntax::{Predicate, SelectOptions};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::rc::Rc;

const PARTITIONS: usize = 64;
pub(crate) const DEFAULT_BUDGET: u64 = 16 * 1024 * 1024;

#[derive(Default)]
struct Bucket {
    bytes: u64,
    rows: u64,
}

impl Bucket {
    fn append(
        &mut self,
        scratch: &Scratch,
        writers: &mut PartitionWriters,
        path: &Path,
        id: u64,
        row: &Row,
        fields: Option<&BTreeSet<String>>,
    ) -> Result<()> {
        let data = match fields {
            Some(fields) => {
                let projected: Row = row
                    .iter()
                    .filter(|(name, _)| fields.contains(*name))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect();
                serde_json::to_vec(&(id, projected))?
            }
            None => serde_json::to_vec(&(id, row))?,
        };
        scratch.account(data.len() as u64 + 1)?;
        writers.append(path, &data)?;
        self.bytes += data.len() as u64 + 1;
        self.rows += 1;
        Ok(())
    }
}

// Match SQL numeric equality across the signed/unsigned integer and binary64
// representations. Within nested JSON, preserve the existing JSON equality.
pub(crate) fn hash_key(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Number(ref number) => {
            if let Some(n) = integral_literal(&value)
                && (i64::try_from(n).is_ok() || u64::try_from(n).is_ok())
            {
                return Some(format!("i:{n}"));
            }
            Some(format!("f:{:016x}", number.as_f64()?.to_bits()))
        }
        value => Some(format!("j:{value}")),
    }
}

fn records(path: &Path, mut emit: impl FnMut(u64, Row) -> Result<()>) -> Result<()> {
    let mut input = BufReader::with_capacity(128 * 1024, File::open(path)?);
    let mut line = String::new();
    while input.read_line(&mut line)? != 0 {
        let (id, row) = crate::internal_json::decode(line.as_bytes())?;
        emit(id, row)?;
        line.clear();
    }
    Ok(())
}

struct Partition<'a> {
    left: &'a Path,
    right: &'a Path,
    left_key: &'a str,
    right_key: &'a str,
    outer: bool,
}

impl Partition<'_> {
    fn probe_memory(&self, visit: &mut JoinVisitor<'_>) -> Result<()> {
        let mut hash = HashMap::<String, Vec<(u64, Row)>>::new();
        records(self.right, |id, row| {
            if let Some(key) = hash_key(field(id, &row, self.right_key)) {
                hash.entry(key).or_default().push((id, row));
            }
            Ok(())
        })?;
        records(self.left, |id, row| {
            if let Some(matches) =
                hash_key(field(id, &row, self.left_key)).and_then(|key| hash.get(&key))
            {
                for (right_id, right_row) in matches {
                    visit(id, &row, Some((*right_id, right_row)))?;
                }
            } else if self.outer {
                visit(id, &row, None)?;
            }
            Ok(())
        })
    }

    // An oversized or skewed bucket keeps its build side in a temporary B-tree.
    // This also handles a hot key whose rows cannot be separated by rehashing.
    fn probe_disk(&self, mut db: Connection, visit: &mut JoinVisitor<'_>) -> Result<()> {
        db.execute_batch("CREATE TABLE rhs (key TEXT NOT NULL, id INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY (key, id)) WITHOUT ROWID")?;
        let tx = db.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO rhs VALUES (?, ?, ?)")?;
            records(self.right, |id, row| {
                if let Some(key) = hash_key(field(id, &row, self.right_key)) {
                    insert.execute(params![key, id, serde_json::to_string(&row)?])?;
                }
                Ok(())
            })?;
        }
        tx.commit()?;
        let mut select = db.prepare("SELECT id, payload FROM rhs WHERE key = ? ORDER BY id")?;
        records(self.left, |id, row| {
            let mut matched = false;
            if let Some(key) = hash_key(field(id, &row, self.left_key)) {
                let mut hits = select.query([key])?;
                while let Some(hit) = hits.next()? {
                    let right_id = hit.get(0)?;
                    let payload = hit.get_ref(1)?.as_str()?;
                    let right_row: Row = crate::internal_json::decode(payload.as_bytes())?;
                    visit(id, &row, Some((right_id, &right_row)))?;
                    matched = true;
                }
            }
            if self.outer && !matched {
                visit(id, &row, None)?;
            }
            Ok(())
        })
    }
}

pub(crate) struct GraceStats {
    pub rows: u64,
    pub bytes: u64,
    pub spill_bytes: u64,
    pub skew_partitions: u64,
    pub result_spill: ResultSpill,
}

pub(crate) fn execute(
    path: &Path,
    base: &File,
    indexed_validation: bool,
    budget: u64,
    table: &str,
    columns: Option<&[String]>,
    predicate: Option<&Predicate>,
    options: &SelectOptions,
    emit: impl FnMut(Value) -> Result<()>,
) -> Result<GraceStats> {
    ensure!(
        budget >= 1024,
        "JOIN partition budget must be at least 1024 bytes"
    );
    let join = options.join.as_ref().expect("Grace plan has a JOIN");
    let (left_key, right_key) = join_key_names(table, options)?;
    let scratch = Rc::new(Scratch::new()?);
    let name = |side: &str, bucket| scratch.path(&format!("{side}-{bucket}"));
    let hash = RandomState::new();
    let mut left = std::array::from_fn::<_, PARTITIONS, _>(|_| Bucket::default());
    let mut right = std::array::from_fn::<_, PARTITIONS, _>(|_| Bucket::default());
    let projection = (columns.is_some() || options.count.is_some())
        .then(|| select_references(columns, predicate, options));
    let fields = |alias: &str, key: &str| {
        projection.as_ref().map(|names| {
            names
                .iter()
                .filter_map(|name| match name.split_once('.') {
                    Some((prefix, column)) => (prefix == alias).then(|| column.to_owned()),
                    None => Some(name.clone()),
                })
                .chain([key.to_owned()])
                .collect::<BTreeSet<_>>()
        })
    };
    let left_fields = fields(options.alias.as_deref().unwrap_or(table), left_key);
    let right_fields = fields(join.alias.as_deref().unwrap_or(&join.table), right_key);
    let mut validation = KeyValidation::new(&scratch);
    let mut writers = PartitionWriters::default();
    let (db, rows, bytes) = walk_base(
        path,
        base,
        if indexed_validation {
            BaseRead::Indexed
        } else {
            BaseRead::BuildIndex
        },
        |_| {},
        |_, _, _| Ok(()),
        |relation, id, primary, _, row| {
            if let Some(key) = primary {
                validation.insert(relation, id, key)?;
            }
            if relation == table {
                let key = hash_key(field(id, row, left_key));
                if key.is_some() || join.left {
                    let bucket = key
                        .as_ref()
                        .map_or(0, |key| hash.hash_one(key) as usize % PARTITIONS);
                    left[bucket].append(
                        &scratch,
                        &mut writers,
                        &name("left", bucket),
                        id,
                        row,
                        left_fields.as_ref(),
                    )?;
                }
            }
            if relation == join.table
                && let Some(key) = hash_key(field(id, row, right_key))
            {
                let bucket = hash.hash_one(&key) as usize % PARTITIONS;
                right[bucket].append(
                    &scratch,
                    &mut writers,
                    &name("right", bucket),
                    id,
                    row,
                    right_fields.as_ref(),
                )?;
            }
            Ok(())
        },
    )?;
    let mut spill_bytes = validation.finish()?;
    let mut skew_partitions = 0;
    writers.finish()?;
    spill_bytes += left
        .iter()
        .chain(&right)
        .map(|part| part.bytes)
        .sum::<u64>();
    let aggregate_spill = db.join_with_pairs(
        table,
        columns,
        predicate,
        options,
        Some(Rc::clone(&scratch)),
        |left_key, right_key, visit| {
            for (i, (left, right)) in left.iter().zip(&right).enumerate() {
                let left_path = name("left", i);
                let right_path = name("right", i);
                if left.rows != 0 {
                    if right.rows == 0 {
                        if join.left {
                            records(&left_path, |id, row| visit(id, &row, None))?;
                        }
                    } else {
                        let partition = Partition {
                            left: &left_path,
                            right: &right_path,
                            left_key,
                            right_key,
                            outer: join.left,
                        };
                        if right.bytes > budget || right.rows > budget / 128 {
                            skew_partitions += 1;
                            let db_path = scratch.path("skew.sqlite");
                            let (database, reservation) = scratch.reserve_sqlite("skew.sqlite")?;
                            partition.probe_disk(database, visit)?;
                            spill_bytes += db_path.metadata()?.len();
                            fs::remove_file(db_path)?;
                            scratch.release(reservation);
                        } else {
                            partition.probe_memory(visit)?;
                        }
                    }
                }
                for (bucket, path) in [(left, left_path), (right, right_path)] {
                    if bucket.rows != 0 {
                        fs::remove_file(path)?;
                        scratch.release(bucket.bytes);
                    }
                }
            }
            Ok(())
        },
        emit,
    )?;
    Ok(GraceStats {
        rows,
        bytes,
        spill_bytes,
        result_spill: aggregate_spill,
        skew_partitions,
    })
}
