//! Plugin discovery, native CLI facts, and decisions. None of these modules runs the CLI.

use std::fmt;

use serde::de::value::MapAccessDeserializer;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub mod catalog;
mod name_map;
pub use name_map::NameMap;
pub mod native;
mod plan;
pub use plan::{Action, Blocker, Choice, Operation, Plan, Status, plan, plan_source_removal};

/// The identity must already be in Copilot's normal form. Never normalise an argument to the CLI.
pub(crate) fn normal_name(name: &str) -> bool {
    name.len() <= 64 && name.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// An unsafe spelling can still be a duplicate or prevent us from proving that a name went away.
fn name_key(name: &str) -> String {
    let mut key = String::new();
    let mut separator = false;
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() {
            if separator && !key.is_empty() {
                key.push('-');
            }
            key.push(char::from(b.to_ascii_lowercase()));
            separator = false;
        } else {
            separator = true;
        }
    }
    key
}

// Plugin parsers already report a fixed error for bad JSON; don't format a second error to discard it.
fn read_json_object<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Option<T> {
    serde_json::from_slice(crate::config::json_object_bytes(bytes)?).ok()
}

/// For evidence-bearing objects, don't let a positional array stand in for named fields.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Fields<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Fields<T> {
            type Value = Object<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            #[inline(never)]
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }
        d.deserialize_map(Fields(std::marker::PhantomData))
    }
}
