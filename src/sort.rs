use crate::engine::{OrderedRow, order_row};
use crate::scratch::{ScratchSlot, configured_bytes};
use crate::storage::private_options;
use anyhow::Result;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;

const FAN_IN: usize = 8;

struct Run {
    path: PathBuf,
    bytes: u64,
}

struct Entry<'a> {
    value: OrderedRow,
    source: usize,
    order: &'a [(String, bool)],
}

impl Ord for Entry<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        order_row(&other.value, &self.value, self.order)
    }
}
impl PartialOrd for Entry<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Entry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Entry<'_> {}

pub(crate) struct Sorter<'a> {
    rows: Vec<OrderedRow>,
    memory_bytes: u64,
    budget: u64,
    limit: Option<usize>,
    order: &'a [(String, bool)],
    scratch: ScratchSlot,
    levels: Vec<Vec<Run>>,
    sequence: u64,
    written: u64,
    frame_limit: u64,
}

impl<'a> Sorter<'a> {
    pub(crate) fn new(
        order: &'a [(String, bool)],
        limit: Option<usize>,
        scratch: ScratchSlot,
    ) -> Result<Self> {
        Ok(Self {
            rows: Vec::new(),
            memory_bytes: 0,
            budget: configured_bytes("JSQNL_SORT_MEMORY_BYTES", 16 * 1024 * 1024)?,
            limit,
            order,
            scratch,
            levels: Vec::new(),
            sequence: 0,
            written: 0,
            frame_limit: crate::limits::frame()?,
        })
    }

    fn weight(row: &OrderedRow) -> u64 {
        128 + row
            .1
            .iter()
            .map(crate::aggregate::value_weight)
            .sum::<u64>()
            + crate::aggregate::value_weight(&row.2)
    }

    pub(crate) fn push(&mut self, row: OrderedRow) -> Result<()> {
        self.memory_bytes += Self::weight(&row);
        self.rows.push(row);
        if let Some(keep) = self.limit
            && self.rows.len() > keep.saturating_mul(2)
        {
            self.rows
                .select_nth_unstable_by(keep, |a, b| order_row(a, b, self.order));
            self.rows.truncate(keep);
            self.memory_bytes = self.rows.iter().map(Self::weight).sum();
        }
        if self.memory_bytes > self.budget {
            let run = self.flush()?;
            self.add_run(run, 0)?;
        }
        Ok(())
    }

    fn new_run(&mut self) -> Result<(Run, BufWriter<File>)> {
        let scratch = self.scratch.get()?;
        let path = scratch.path(&format!("sort-{}", self.sequence));
        self.sequence += 1;
        let file = private_options().create_new(true).open(&path)?;
        Ok((
            Run { path, bytes: 0 },
            BufWriter::with_capacity(64 * 1024, file),
        ))
    }

    fn write(&self, writer: &mut BufWriter<File>, bytes: &mut u64, row: OrderedRow) -> Result<()> {
        let data = serde_json::to_vec(&row)?;
        let size = data.len() as u64 + 1;
        anyhow::ensure!(
            size <= self.frame_limit,
            "sort record exceeds byte limit ({})",
            self.frame_limit
        );
        self.scratch.get()?.account(size)?;
        writer.write_all(&data)?;
        writer.write_all(b"\n")?;
        *bytes += size;
        Ok(())
    }

    fn flush(&mut self) -> Result<Run> {
        let (mut run, mut writer) = self.new_run()?;
        self.rows
            .sort_unstable_by(|a, b| order_row(a, b, self.order));
        let rows = std::mem::take(&mut self.rows);
        for row in rows.into_iter().take(self.limit.unwrap_or(usize::MAX)) {
            self.write(&mut writer, &mut run.bytes, row)?;
        }
        writer.flush()?;
        self.memory_bytes = 0;
        self.written += run.bytes;
        Ok(run)
    }

    fn add_run(&mut self, mut run: Run, mut level: usize) -> Result<()> {
        loop {
            if self.levels.len() <= level {
                self.levels.push(Vec::new());
            }
            self.levels[level].push(run);
            if self.levels[level].len() < FAN_IN {
                return Ok(());
            }
            let runs = std::mem::take(&mut self.levels[level]);
            run = self.merge_to_run(runs)?;
            level += 1;
        }
    }

    fn merge_to_run(&mut self, inputs: Vec<Run>) -> Result<Run> {
        let (mut run, mut writer) = self.new_run()?;
        self.merge(&inputs, 0, self.limit.unwrap_or(usize::MAX), |row| {
            self.write(&mut writer, &mut run.bytes, row)
        })?;
        writer.flush()?;
        self.written += run.bytes;
        self.remove(inputs)?;
        Ok(run)
    }

    fn remove(&self, runs: Vec<Run>) -> Result<()> {
        let scratch = self.scratch.get()?;
        for run in runs {
            fs::remove_file(run.path)?;
            scratch.release(run.bytes);
        }
        Ok(())
    }

    fn merge(
        &self,
        runs: &[Run],
        skip: usize,
        take: usize,
        mut emit: impl FnMut(OrderedRow) -> Result<()>,
    ) -> Result<()> {
        let mut readers: Vec<_> = runs
            .iter()
            .map(|run| File::open(&run.path).map(|file| BufReader::with_capacity(32 * 1024, file)))
            .collect::<std::io::Result<_>>()?;
        let mut text = String::new();
        let mut head = BinaryHeap::new();
        let mut read = |reader: &mut BufReader<File>| -> Result<Option<OrderedRow>> {
            text.clear();
            if crate::limits::line(reader, &mut text, self.frame_limit)? == 0 {
                return Ok(None);
            }
            Ok(Some(crate::internal_json::decode(text.as_bytes())?))
        };
        for (source, reader) in readers.iter_mut().enumerate() {
            if let Some(value) = read(reader)? {
                head.push(Entry {
                    value,
                    source,
                    order: self.order,
                });
            }
        }
        let mut seen = 0usize;
        let mut emitted = 0usize;
        while emitted < take {
            let Some(entry) = head.pop() else { break };
            if seen >= skip {
                emit(entry.value)?;
                emitted += 1;
            }
            seen += 1;
            if let Some(value) = read(&mut readers[entry.source])? {
                head.push(Entry {
                    value,
                    source: entry.source,
                    order: self.order,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        skip: usize,
        take: usize,
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<u64> {
        if self.levels.is_empty() {
            self.rows
                .sort_unstable_by(|a, b| order_row(a, b, self.order));
            for (_, _, row) in self.rows.into_iter().skip(skip).take(take) {
                emit(row)?;
            }
            return Ok(0);
        }
        let mut all: Vec<_> = std::mem::take(&mut self.levels)
            .into_iter()
            .flatten()
            .collect();
        if !self.rows.is_empty() {
            all.push(self.flush()?);
        }
        while all.len() > FAN_IN {
            let remaining = all.split_off(FAN_IN);
            let merged = self.merge_to_run(all)?;
            all = remaining;
            all.push(merged);
        }
        self.merge(&all, skip, take, |(_, _, row)| emit(row))?;
        self.remove(all)?;
        Ok(self.written)
    }
}
