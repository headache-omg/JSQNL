use crate::disk_index;
use crate::engine::{Database, ResultSpill};
use crate::syntax::{Predicate, SelectOptions, Statement};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::fs::File;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessPath {
    SequentialScan,
    IndexSeek,
    IndexCount,
    StreamingJoin,
    GraceHashJoin,
    Materialized,
}

pub(crate) enum BasePlan {
    Scan {
        indexed_validation: bool,
    },
    Seek(Option<(u64, u64)>),
    Count(u64),
    Join {
        indexed_validation: bool,
    },
    GraceJoin {
        indexed_validation: bool,
        budget: u64,
    },
}

impl BasePlan {
    pub(crate) fn prepare(
        path: &Path,
        base: &File,
        table: &str,
        predicate: Option<&Predicate>,
        options: &SelectOptions,
        validate: impl FnOnce(&Database) -> Result<()>,
    ) -> Result<Self> {
        if options.join.is_some() {
            let indexed_validation = disk_index::is_current(path, base)?;
            let budget = match std::env::var("JSQNL_JOIN_MEMORY_BYTES") {
                Ok(value) => {
                    let bytes = value
                        .parse::<u64>()
                        .context("invalid JSQNL_JOIN_MEMORY_BYTES")?;
                    ensure!(
                        bytes >= 1024,
                        "JSQNL_JOIN_MEMORY_BYTES must be at least 1024"
                    );
                    bytes
                }
                Err(std::env::VarError::NotPresent) => crate::grace_join::DEFAULT_BUDGET,
                Err(error) => return Err(error.into()),
            };
            return Ok(if base.metadata()?.len() > budget {
                Self::GraceJoin {
                    indexed_validation,
                    budget,
                }
            } else {
                Self::Join { indexed_validation }
            });
        }
        if options.count.is_some()
            && options.group.is_empty()
            && predicate.is_none()
            && let Some(count) = disk_index::count(path, base, table)?
        {
            return Ok(Self::Count(count));
        }
        if let Some(location) = disk_index::locate(path, base, table, predicate, validate)? {
            return Ok(Self::Seek(location));
        }
        Ok(Self::Scan {
            indexed_validation: disk_index::is_current(path, base)?,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct QueryReport {
    pub access: AccessPath,
    pub operators: Vec<&'static str>,
    pub rows_read: u64,
    pub base_bytes_read: u64,
    pub journal_bytes_read: u64,
    pub rows_output: u64,
    pub elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spill_bytes_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oversized_partitions: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl QueryReport {
    pub(crate) fn record_spill(&mut self, spill: ResultSpill) {
        if spill.group != 0
            && let Some(index) = self.operators.iter().position(|op| *op == "group")
        {
            self.operators.insert(index, "external_hash_aggregation");
        }
        if spill.sort != 0
            && let Some(operator) = self.operators.iter_mut().find(|op| **op == "sort")
        {
            *operator = "external_sort";
        }
        if spill.bytes() != 0 {
            self.spill_bytes_written = Some(self.spill_bytes_written.unwrap_or(0) + spill.bytes());
        }
    }

    pub(crate) fn new(access: AccessPath, statements: &[(u64, Statement)]) -> Self {
        let mut operators = vec![match access {
            AccessPath::SequentialScan => "sequential_scan",
            AccessPath::IndexSeek => "index_seek",
            AccessPath::IndexCount => "index_count",
            AccessPath::StreamingJoin | AccessPath::GraceHashJoin => "sequential_scan",
            AccessPath::Materialized => "materialize_database",
        }];
        if let [(_, Statement::Select(_, _, predicate, options))] = statements {
            if options.join.is_some() {
                if access == AccessPath::GraceHashJoin {
                    operators.extend(["hash_partition", "grace_hash_join"]);
                } else {
                    operators.push("indexed_equi_join");
                }
            }
            if predicate.is_some() {
                operators.push("filter");
            }
            if !options.group.is_empty() {
                operators.push("group");
            }
            if options.count.is_some() {
                operators.push("count");
            }
            if !options.order.is_empty()
                || !options.group.is_empty()
                || (access == AccessPath::GraceHashJoin
                    && options.group.is_empty()
                    && options.count.is_none())
            {
                operators.push("sort");
            }
            if options.limit.is_some() || options.offset != 0 {
                operators.push("limit_offset");
            }
            operators.push("project");
        }
        Self {
            access,
            operators,
            rows_read: 0,
            base_bytes_read: 0,
            journal_bytes_read: 0,
            rows_output: 0,
            elapsed_ms: 0.0,
            spill_bytes_written: None,
            oversized_partitions: None,
            warnings: Vec::new(),
        }
    }
}
