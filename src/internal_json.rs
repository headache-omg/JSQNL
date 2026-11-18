//! Internal envelopes add nesting to JSON already accepted by the base reader.
use anyhow::{Result, ensure};
use serde::de::DeserializeOwned;

const INTERNAL_DEPTH: usize = 144;

pub(crate) fn check_depth(bytes: &[u8]) -> Result<()> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escape = false;
    for &byte in bytes {
        if quoted {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    ensure!(
                        depth <= INTERNAL_DEPTH,
                        "internal JSON nesting limit exceeded"
                    );
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Ok(value),
        Err(error) if error.to_string().starts_with("recursion limit exceeded") => {
            check_depth(bytes)?;
            let mut parser = serde_json::Deserializer::from_slice(bytes);
            parser.disable_recursion_limit();
            let value = T::deserialize(&mut parser)?;
            parser.end()?;
            Ok(value)
        }
        Err(error) => Err(error.into()),
    }
}
