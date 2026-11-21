//! Ordered plugin maps share one tree implementation: the tree indexes a dense vector of values.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Index;

use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug)]
pub struct NameMap<V> {
    slots: BTreeMap<String, usize>,
    values: Vec<V>,
}

impl<V> NameMap<V> {
    pub const fn new() -> Self {
        Self { slots: BTreeMap::new(), values: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.slots.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        Some(&self.values[*self.slots.get(key)?])
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        Some(&mut self.values[*self.slots.get(key)?])
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.slots.keys()
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.slots.iter().map(|(key, &i)| (key, &self.values[i]))
    }

    pub(crate) fn into_entries(self) -> impl Iterator<Item = (String, V)> {
        let mut values: Vec<_> = self.values.into_iter().map(Some).collect();
        self.slots.into_iter().map(move |(key, i)| (key, values[i].take().expect("distinct slots")))
    }

    pub fn insert(&mut self, key: String, value: V) -> Option<V> {
        let next = self.len();
        let slot = index_slot(&mut self.slots, key, next);
        if slot == next {
            self.values.push(value);
            None
        } else {
            Some(std::mem::replace(&mut self.values[slot], value))
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<V> {
        Some(self.values.swap_remove(remove_slot(&mut self.slots, key)?))
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.values.clear();
    }
}

#[inline(never)]
fn index_slot(slots: &mut BTreeMap<String, usize>, key: String, next: usize) -> usize {
    *slots.entry(key).or_insert(next)
}

#[inline(never)]
fn remove_slot(slots: &mut BTreeMap<String, usize>, key: &str) -> Option<usize> {
    let removed = slots.remove(key)?;
    let last = slots.len();
    if removed != last {
        // Removal is explicit, not a poll operation. Keep the values dense without cloning keys.
        *slots.values_mut().find(|i| **i == last).expect("index for the last value") = removed;
    }
    Some(removed)
}

impl<V> Default for NameMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: PartialEq> PartialEq for NameMap<V> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl<V: Eq> Eq for NameMap<V> {}

impl<V> Index<&str> for NameMap<V> {
    type Output = V;

    fn index(&self, name: &str) -> &V {
        self.get(name).expect("no entry found for key")
    }
}

impl<V, const N: usize> From<[(String, V); N]> for NameMap<V> {
    fn from(entries: [(String, V); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl<V> FromIterator<(String, V)> for NameMap<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (name, value) in iter {
            map.insert(name, value);
        }
        map
    }
}

impl<V: Serialize> Serialize for NameMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for NameMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Unique<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for Unique<V> {
            type Value = NameMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with unique keys")
            }

            #[inline(never)]
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = NameMap::new();
                while let Some(key) = read_key(&mut map)? {
                    if entries.insert(key, map.next_value()?).is_some() {
                        return Err(A::Error::custom("a plugin record has a duplicate key"));
                    }
                }
                Ok(entries)
            }
        }
        d.deserialize_map(Unique(std::marker::PhantomData))
    }
}

// Share the string-key decoder across the visitors for the different value types.
#[inline(never)]
fn read_key<'de, A: MapAccess<'de>>(map: &mut A) -> Result<Option<String>, A::Error> {
    map.next_key()
}
