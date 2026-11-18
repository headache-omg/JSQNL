use crate::grace_join::hash_key;
use crate::scratch::{PartitionWriters, Scratch, ScratchSlot, configured_bytes};
use anyhow::{Context, Result};
use rusqlite::{Statement, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::rc::Rc;

const PARTITIONS: usize = 256;
pub(crate) const DEFAULT_BUDGET: u64 = 16 * 1024 * 1024;
const UPSERT: &str = "INSERT INTO groups VALUES (?1, ?2, ?3, ?4)
    ON CONFLICT(key) DO UPDATE SET
        total = groups.total + excluded.total,
        payload = CASE WHEN excluded.first_id < groups.first_id THEN excluded.payload ELSE groups.payload END,
        first_id = min(groups.first_id, excluded.first_id)";

#[derive(Serialize, Deserialize)]
struct Partial {
    id: u64,
    count: u64,
    values: Vec<Value>,
}

type Groups = HashMap<String, Partial>;

fn key(values: &[Value]) -> Result<String> {
    let names: Vec<_> = values.iter().cloned().map(hash_key).collect();
    Ok(serde_json::to_string(&names)?)
}

pub(crate) fn value_weight(value: &Value) -> u64 {
    match value {
        Value::String(text) => text.len() as u64,
        Value::Array(items) => {
            items.len() as u64 * 32 + items.iter().map(value_weight).sum::<u64>()
        }
        Value::Object(items) => items
            .iter()
            .map(|(key, value)| 96 + key.len() as u64 + value_weight(value))
            .sum(),
        _ => 0,
    }
}

fn estimate(key: &str, values: &[Value]) -> u64 {
    // Include hash-table buckets, owned keys, the Vec and nested JSON allocations.
    192 + key.len() as u64 + values.len() as u64 * 48 + values.iter().map(value_weight).sum::<u64>()
}

fn merge(groups: &mut Groups, bytes: &mut u64, key: String, value: Partial) {
    match groups.entry(key) {
        std::collections::hash_map::Entry::Vacant(slot) => {
            *bytes += estimate(slot.key(), &value.values);
            slot.insert(value);
        }
        std::collections::hash_map::Entry::Occupied(mut slot) => {
            let existing = slot.get_mut();
            if value.id < existing.id {
                *existing = Partial {
                    count: value.count + existing.count,
                    ..value
                };
            } else {
                existing.count += value.count;
            }
        }
    }
}

fn read(input: &mut impl BufRead, line: &mut String) -> Result<Option<(String, Partial)>> {
    line.clear();
    if input.read_line(line)? == 0 {
        return Ok(None);
    }
    Ok(Some(crate::internal_json::decode(line.as_bytes())?))
}

fn upsert(insert: &mut Statement<'_>, key: String, group: Partial) -> Result<()> {
    insert
        .execute(params![
            key,
            group.id,
            group.count,
            serde_json::to_string(&group.values)?
        ])
        .context("writing spill aggregation index (temporary quota or disk may be exhausted)")?;
    Ok(())
}

struct Spill {
    root: Rc<Scratch>,
    hash: RandomState,
    partitions: PartitionWriters,
    bytes: [u64; PARTITIONS],
}

impl Spill {
    fn new(root: Rc<Scratch>) -> Self {
        Self {
            root,
            hash: RandomState::new(),
            partitions: PartitionWriters::default(),
            bytes: [0; PARTITIONS],
        }
    }

    fn write(&mut self, key: String, group: Partial) -> Result<()> {
        let bucket = self.hash.hash_one(&key) as usize % PARTITIONS;
        let encoded = serde_json::to_vec(&(key, group))?;
        let size = encoded.len() as u64 + 1;
        self.root.account(size)?;
        self.partitions
            .append(&self.root.path(&format!("group-{bucket}")), &encoded)?;
        self.bytes[bucket] += size;
        Ok(())
    }

    fn finish(
        mut self,
        budget: u64,
        emit: &mut impl FnMut(u64, u64, Vec<Value>) -> Result<()>,
    ) -> Result<u64> {
        std::mem::take(&mut self.partitions).finish()?;
        let mut written = self.bytes.iter().sum();
        for (bucket, bytes) in self.bytes.into_iter().enumerate() {
            if bytes == 0 {
                continue;
            }
            let file = self.root.path(&format!("group-{bucket}"));
            written += self.reduce(&file, budget, emit)?;
            fs::remove_file(file)?;
            self.root.release(bytes);
        }
        Ok(written)
    }

    fn reduce(
        &self,
        path: &Path,
        budget: u64,
        emit: &mut impl FnMut(u64, u64, Vec<Value>) -> Result<()>,
    ) -> Result<u64> {
        let mut input = BufReader::with_capacity(128 * 1024, File::open(path)?);
        let mut line = String::new();
        let mut groups = Groups::new();
        let mut bytes = 0;
        while let Some((key, group)) = read(&mut input, &mut line)? {
            merge(&mut groups, &mut bytes, key, group);
            if bytes > budget {
                return self.reduce_disk(groups, &mut input, &mut line, emit);
            }
        }
        for group in groups.into_values() {
            emit(group.id, group.count, group.values)?;
        }
        Ok(0)
    }

    fn reduce_disk(
        &self,
        groups: Groups,
        input: &mut impl BufRead,
        line: &mut String,
        emit: &mut impl FnMut(u64, u64, Vec<Value>) -> Result<()>,
    ) -> Result<u64> {
        let name = "aggregation.sqlite";
        let (mut db, reservation) = self.root.reserve_sqlite(name)?;
        db.execute_batch("CREATE TABLE groups (
            key TEXT PRIMARY KEY, first_id INTEGER NOT NULL, total INTEGER NOT NULL, payload TEXT NOT NULL
        ) WITHOUT ROWID")?;
        let tx = db.transaction()?;
        {
            let mut insert = tx.prepare(UPSERT)?;
            for (key, group) in groups {
                upsert(&mut insert, key, group)?;
            }
            while let Some((key, group)) = read(input, line)? {
                upsert(&mut insert, key, group)?;
            }
        }
        tx.commit()?;
        {
            let mut select = db.prepare("SELECT first_id, total, payload FROM groups")?;
            let mut cursor = select.query([])?;
            while let Some(row) = cursor.next()? {
                emit(
                    row.get(0)?,
                    row.get(1)?,
                    crate::internal_json::decode(row.get_ref(2)?.as_str()?.as_bytes())?,
                )?;
            }
        }
        drop(db);
        let path = self.root.path(name);
        let bytes = path.metadata()?.len();
        fs::remove_file(path)?;
        self.root.release(reservation);
        Ok(bytes)
    }
}

pub(crate) struct Aggregation {
    groups: Groups,
    bytes: u64,
    budget: u64,
    root: ScratchSlot,
    spill: Option<Spill>,
}

impl Aggregation {
    pub(crate) fn new(root: ScratchSlot) -> Result<Self> {
        Ok(Self {
            groups: Groups::new(),
            bytes: 0,
            budget: configured_bytes("JSQNL_GROUP_MEMORY_BYTES", DEFAULT_BUDGET)?,
            root,
            spill: None,
        })
    }

    pub(crate) fn push(&mut self, id: u64, values: Vec<Value>) -> Result<()> {
        let key = key(&values)?;
        let group = Partial {
            id,
            count: 1,
            values,
        };
        if let Some(spill) = &mut self.spill {
            return spill.write(key, group);
        }
        merge(&mut self.groups, &mut self.bytes, key, group);
        if self.bytes > self.budget {
            let mut spill = Spill::new(self.root.get()?);
            for (key, group) in std::mem::take(&mut self.groups) {
                spill.write(key, group)?;
            }
            self.spill = Some(spill);
        }
        Ok(())
    }

    pub(crate) fn finish(
        self,
        mut emit: impl FnMut(u64, u64, Vec<Value>) -> Result<()>,
    ) -> Result<u64> {
        if let Some(spill) = self.spill {
            return spill.finish(self.budget, &mut emit);
        }
        for group in self.groups.into_values() {
            emit(group.id, group.count, group.values)?;
        }
        Ok(0)
    }
}
