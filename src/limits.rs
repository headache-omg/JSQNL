use anyhow::{Result, ensure};
use std::io::{BufRead, Read};

const MIB: u64 = 1024 * 1024;

pub(crate) fn record() -> Result<u64> {
    crate::scratch::configured_bytes("JSQNL_RECORD_LIMIT_BYTES", 8 * MIB)
}

pub(crate) fn frame() -> Result<u64> {
    crate::scratch::configured_bytes("JSQNL_FRAME_LIMIT_BYTES", 64 * MIB)
}

pub(crate) fn materialized() -> Result<u64> {
    crate::scratch::configured_bytes("JSQNL_MATERIALIZED_LIMIT_BYTES", 128 * MIB)
}

pub(crate) fn line(reader: &mut impl BufRead, output: &mut String, limit: u64) -> Result<usize> {
    let count = reader.take(limit.saturating_add(1)).read_line(output)?;
    ensure!(count as u64 <= limit, "record exceeds byte limit ({limit})");
    Ok(count)
}

pub(crate) fn frame_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    limit: u64,
) -> Result<usize> {
    let count = reader
        .take(limit.saturating_add(1))
        .read_until(b'\n', output)?;
    ensure!(
        count as u64 <= limit,
        "journal frame exceeds byte limit ({limit})"
    );
    Ok(count)
}

pub(crate) fn encode(value: &impl serde::Serialize, limit: u64) -> Result<Vec<u8>> {
    struct Limited {
        data: Vec<u8>,
        limit: u64,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if (self.data.len() as u64).saturating_add(bytes.len() as u64) > self.limit {
                return Err(std::io::Error::other("journal frame exceeds byte limit"));
            }
            self.data
                .try_reserve(bytes.len())
                .map_err(std::io::Error::other)?;
            self.data.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited {
        data: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, value)?;
    Ok(output.data)
}

pub(crate) fn measure(value: &impl serde::Serialize, limit: u64) -> Result<()> {
    struct Counter(u64);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("record exceeds byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(limit), value)?;
    Ok(())
}

pub(crate) fn charge_result(used: &mut u64, limit: u64, value: &serde_json::Value) -> Result<()> {
    *used = used.saturating_add(96 + crate::aggregate::value_weight(value));
    ensure!(
        *used <= limit,
        "materialized query results exceed memory budget ({limit} bytes)"
    );
    Ok(())
}
