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

pub(crate) fn valid_line_ending(line: &str) -> bool {
    let body = line.strip_suffix('\n').unwrap_or(line);
    !body.strip_suffix('\r').unwrap_or(body).contains('\r')
}

fn read_limited_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    limit: u64,
    message: &str,
) -> Result<usize> {
    let mut read = 0usize;
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if available.is_empty() {
            return Ok(read);
        }
        let (chunk, done) = match available.iter().position(|&byte| byte == b'\n') {
            Some(pos) => (&available[..=pos], true),
            None => (available, false),
        };
        ensure!(
            (read as u64).saturating_add(chunk.len() as u64) <= limit,
            "{message} ({limit})"
        );
        output
            .try_reserve(chunk.len())
            .map_err(std::io::Error::other)?;
        output.extend_from_slice(chunk);
        let consumed = chunk.len();
        reader.consume(consumed);
        read += consumed;
        if done {
            return Ok(read);
        }
    }
}

pub(crate) fn read_bytes(mut reader: impl Read, limit: u64, message: &str) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => return Ok(output),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            (output.len() as u64).saturating_add(count as u64) <= limit,
            "{message} ({limit})"
        );
        output.try_reserve(count).map_err(std::io::Error::other)?;
        output.extend_from_slice(&chunk[..count]);
    }
}

pub(crate) fn line(reader: &mut impl BufRead, output: &mut String, limit: u64) -> Result<usize> {
    let mut bytes = Vec::new();
    let count = read_limited_line(reader, &mut bytes, limit, "record exceeds byte limit")?;
    let text = String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })?;
    if output.is_empty() {
        *output = text;
    } else {
        output.push_str(&text);
    }
    Ok(count)
}

pub(crate) fn frame_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    limit: u64,
) -> Result<usize> {
    read_limited_line(reader, output, limit, "journal frame exceeds byte limit")
}

pub(crate) fn encode(value: &impl serde::Serialize, limit: u64) -> Result<Vec<u8>> {
    encode_with(value, limit, "journal frame exceeds byte limit")
}

pub(crate) fn encode_with(
    value: &impl serde::Serialize,
    limit: u64,
    message: &'static str,
) -> Result<Vec<u8>> {
    struct Limited {
        data: Vec<u8>,
        limit: u64,
        message: &'static str,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if (self.data.len() as u64).saturating_add(bytes.len() as u64) > self.limit {
                return Err(std::io::Error::other(self.message));
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
        message,
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
