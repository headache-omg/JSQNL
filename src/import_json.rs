use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::fmt;

pub(crate) const NODE_LIMIT: usize = 262_144;

pub(crate) struct StrictValue(pub Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ValueSeed {
            count: &Cell::new(0),
            strict: true,
        }
        .deserialize(deserializer)
        .map(Self)
    }
}

#[derive(Clone, Copy)]
struct ValueSeed<'a> {
    count: &'a Cell<usize>,
    strict: bool,
}

pub(crate) struct BoundedValue(pub Value);

impl<'de> Deserialize<'de> for BoundedValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ValueSeed {
            count: &Cell::new(0),
            strict: false,
        }
        .deserialize(deserializer)
        .map(Self)
    }
}

pub(crate) fn bounded_row(text: &str) -> anyhow::Result<crate::engine::Row> {
    match serde_json::from_str::<BoundedValue>(text)?.0 {
        Value::Object(row) => Ok(row),
        _ => anyhow::bail!("expected a JSON object"),
    }
}

pub(crate) fn validate_row_nodes(row: &crate::engine::Row) -> anyhow::Result<()> {
    fn charge(value: &Value, left: &mut usize) -> anyhow::Result<()> {
        *left = left
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("JSON record exceeds node limit ({NODE_LIMIT})"))?;
        match value {
            Value::Array(values) => values.iter().try_for_each(|value| charge(value, left)),
            Value::Object(values) => values.values().try_for_each(|value| charge(value, left)),
            _ => Ok(()),
        }
    }
    let mut left = NODE_LIMIT - 1;
    row.values().try_for_each(|value| charge(value, &mut left))
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        let used = self.count.get();
        if used >= NODE_LIMIT {
            return Err(de::Error::custom(format!(
                "JSON record exceeds node limit ({NODE_LIMIT})"
            )));
        }
        self.count.set(used + 1);
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ValueSeed<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.strict {
            formatter.write_str("a JSON value with unique object member names")
        } else {
            formatter.write_str("a JSON value")
        }
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Value, A::Error> {
        let mut output = Vec::new();
        while let Some(value) = input.next_element_seed(self)? {
            output.push(value);
        }
        Ok(Value::Array(output))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut input: A) -> Result<Value, A::Error> {
        let mut output = Map::new();
        while let Some(name) = input.next_key::<String>()? {
            if self.strict && output.contains_key(&name) {
                return Err(de::Error::custom(format!("duplicate JSON member {name:?}")));
            }
            let value = input.next_value_seed(self)?;
            output.insert(name, value);
        }
        Ok(Value::Object(output))
    }
}
