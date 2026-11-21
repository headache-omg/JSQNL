//! Internal envelopes add nesting to JSON already accepted by the base reader.
use anyhow::{Result, ensure};
use serde::de::DeserializeOwned;

const INTERNAL_DEPTH: usize = 144;

pub(crate) fn default_node_limit() -> Result<usize> {
    let materialized = crate::limits::materialized()?;
    let budget_nodes = usize::try_from(materialized / 32).unwrap_or(usize::MAX);
    Ok(budget_nodes.max(crate::import_json::NODE_LIMIT.saturating_mul(2)))
}

pub(crate) fn check_structure(bytes: &[u8], max_nodes: usize) -> Result<()> {
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut quoted = false;
    let mut escape = false;
    let mut in_primitive = false;
    for &byte in bytes {
        if quoted {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_primitive = false;
                quoted = true;
                nodes += 1;
                ensure!(
                    nodes <= max_nodes,
                    "internal JSON node limit exceeded ({max_nodes})"
                );
            }
            b'{' | b'[' => {
                in_primitive = false;
                depth += 1;
                ensure!(
                    depth <= INTERNAL_DEPTH,
                    "internal JSON nesting limit exceeded"
                );
                nodes += 1;
                ensure!(
                    nodes <= max_nodes,
                    "internal JSON node limit exceeded ({max_nodes})"
                );
            }
            b'}' | b']' => {
                in_primitive = false;
                depth = depth.saturating_sub(1);
            }
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' => {
                in_primitive = false;
            }
            _ => {
                if !in_primitive {
                    in_primitive = true;
                    nodes += 1;
                    ensure!(
                        nodes <= max_nodes,
                        "internal JSON node limit exceeded ({max_nodes})"
                    );
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check_depth(bytes: &[u8]) -> Result<()> {
    check_structure(bytes, default_node_limit()?)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    check_structure(bytes, default_node_limit()?)?;
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    parser.disable_recursion_limit();
    let value = T::deserialize(&mut parser)?;
    parser.end()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_structure_rejects_excessive_nodes_and_depth() {
        let dense = format!("[{}0]", "0,".repeat(100));
        assert!(check_structure(dense.as_bytes(), 50).is_err());
        assert!(check_structure(dense.as_bytes(), 200).is_ok());

        let deep = format!("{}{}", "[".repeat(145), "]".repeat(145));
        assert!(check_structure(deep.as_bytes(), 1000).is_err());
    }
}
