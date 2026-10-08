//! Bound expanded YAML, not just input bytes: aliases can multiply small inputs.
use serde::de::{self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde_yaml_ng::{
    Mapping, Value,
    value::{Tag, TaggedValue},
};
use std::fmt;

struct Budget {
    nodes: usize,
    bytes: usize,
}
struct Bounded<'a>(&'a mut Budget);

impl<'de> DeserializeSeed<'de> for Bounded<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        self.0.nodes = self
            .0
            .nodes
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("expanded YAML node limit exceeded"))?;
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Bounded<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded YAML value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        self.0.bytes = self
            .0
            .bytes
            .checked_sub(v.len())
            .ok_or_else(|| E::custom("expanded YAML byte limit exceeded"))?;
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.visit_str(&v)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Bounded(self.0))? {
            values.push(value);
        }
        Ok(Value::Sequence(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Mapping::new();
        while let Some(key) = map.next_key_seed(Bounded(self.0))? {
            let value = map.next_value_seed(Bounded(self.0))?;
            if values.insert(key, value).is_some() {
                return Err(de::Error::custom("duplicate YAML key"));
            }
        }
        Ok(Value::Mapping(values))
    }
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        let (tag, data) = data.variant::<String>()?;
        self.0.bytes = self
            .0
            .bytes
            .checked_sub(tag.len())
            .ok_or_else(|| de::Error::custom("expanded YAML byte limit exceeded"))?;
        let value = data.newtype_variant_seed(Bounded(self.0))?;
        Ok(Value::Tagged(Box::new(TaggedValue {
            tag: Tag::new(tag),
            value,
        })))
    }
}

pub(super) fn parse(text: &str) -> anyhow::Result<Mapping> {
    let mut budget = Budget {
        nodes: 8192,
        bytes: 256 * 1024,
    };
    let value = Bounded(&mut budget).deserialize(serde_yaml_ng::Deserializer::from_str(text))?;
    match value {
        Value::Mapping(mapping) => Ok(mapping),
        _ => anyhow::bail!("OKF frontmatter must be a mapping"),
    }
}
