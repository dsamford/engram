//! The row: variable bindings as a SORTED small vector.
//!
//! A row holds a handful of bindings (a dozen is a wide statement). The
//! interpreter's row was a `BTreeMap<String, Value>`: every clone allocated
//! a tree node per binding, every access walked the tree, and the
//! interpreter clones and reads rows at every clause boundary — measured as
//! a material share of the per-row toll on the production port. This map
//! keeps the BTreeMap's CONTRACT — keys iterate in sorted order, so
//! `RETURN *` columns, `input_names`, and every `keys()` consumer see exactly
//! what they saw — while a clone is one contiguous allocation and a lookup is
//! a binary search over a few entries.
//!
//! It is deliberately a subset of the `BTreeMap` API: the methods the
//! interpreter uses, with the same return shapes, so the swap is mechanical
//! and the semantics are pinned by the equivalence test below rather than
//! re-derived.

use std::collections::BTreeMap;

use crate::value::Value;

/// Variable bindings, sorted by name.
#[derive(Clone, Default, PartialEq)]
pub struct VarMap {
    entries: Vec<(String, Value)>,
}

impl VarMap {
    /// An empty row.
    pub const fn new() -> Self {
        VarMap {
            entries: Vec::new(),
        }
    }

    fn position(&self, name: &str) -> Result<usize, usize> {
        self.entries.binary_search_by(|(k, _)| k.as_str().cmp(name))
    }

    /// The binding, if present.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self.position(name) {
            Ok(i) => Some(&self.entries[i].1),
            Err(_) => None,
        }
    }

    /// Mutable access to a binding.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        match self.position(name) {
            Ok(i) => Some(&mut self.entries[i].1),
            Err(_) => None,
        }
    }

    /// Whether the name is bound.
    pub fn contains_key(&self, name: &str) -> bool {
        self.position(name).is_ok()
    }

    /// Bind (or rebind); the previous value comes back, as `BTreeMap::insert`.
    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        match self.position(&name) {
            Ok(i) => Some(std::mem::replace(&mut self.entries[i].1, value)),
            Err(i) => {
                self.entries.insert(i, (name, value));
                None
            }
        }
    }

    /// Unbind; the value comes back if it was bound.
    pub fn remove(&mut self, name: &str) -> Option<Value> {
        match self.position(name) {
            Ok(i) => Some(self.entries.remove(i).1),
            Err(_) => None,
        }
    }

    /// The entry API, in the two forms the interpreter uses.
    pub fn entry(&mut self, name: String) -> Entry<'_> {
        let pos = self.position(&name);
        Entry {
            map: self,
            name,
            pos,
        }
    }

    /// Names, in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &String> + '_ {
        self.entries.iter().map(|(k, _)| k)
    }

    /// Values, in key order.
    pub fn values(&self) -> impl Iterator<Item = &Value> + '_ {
        self.entries.iter().map(|(_, v)| v)
    }

    /// Mutable values, in key order.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> + '_ {
        self.entries.iter_mut().map(|(_, v)| v)
    }

    /// (name, value) pairs, in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> + '_ {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    /// How many bindings.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bind every pair; later pairs win, as with `BTreeMap::extend`.
    pub fn extend<I: IntoIterator<Item = (String, Value)>>(&mut self, pairs: I) {
        for (k, v) in pairs {
            self.insert(k, v);
        }
    }

    /// The bindings as the tree map the rest of the world speaks.
    pub fn to_btree(&self) -> BTreeMap<String, Value> {
        self.entries.iter().cloned().collect()
    }
}

/// A vacant-or-occupied slot, for `entry(name).or_insert(..)`.
pub struct Entry<'a> {
    map: &'a mut VarMap,
    name: String,
    pos: Result<usize, usize>,
}

impl<'a> Entry<'a> {
    /// The bound value, binding `default` first if the name was free.
    pub fn or_insert(self, default: Value) -> &'a mut Value {
        self.or_insert_with(|| default)
    }

    /// The bound value, binding `f()` first if the name was free.
    pub fn or_insert_with<F: FnOnce() -> Value>(self, f: F) -> &'a mut Value {
        let i = match self.pos {
            Ok(i) => i,
            Err(i) => {
                self.map.entries.insert(i, (self.name, f()));
                i
            }
        };
        &mut self.map.entries[i].1
    }
}

impl IntoIterator for VarMap {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a VarMap {
    type Item = (&'a String, &'a Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a String, &'a Value)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.entries.iter().map(|(k, v)| (k, v)))
    }
}

impl FromIterator<(String, Value)> for VarMap {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut m = VarMap::new();
        m.extend(iter);
        m
    }
}

impl From<BTreeMap<String, Value>> for VarMap {
    fn from(m: BTreeMap<String, Value>) -> Self {
        // Already sorted: no per-insert search.
        VarMap {
            entries: m.into_iter().collect(),
        }
    }
}

impl std::fmt::Debug for VarMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract: under any sequence of inserts, rebinds, removes and
    /// entry-or-inserts, the map reports exactly what a BTreeMap reports —
    /// including iteration ORDER, which `RETURN *` column order rides on.
    #[test]
    fn behaves_exactly_like_a_btreemap() {
        let mut ours = VarMap::new();
        let mut theirs: BTreeMap<String, Value> = BTreeMap::new();
        // A deterministic LCG drives the operation mix.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let names = [
            "n", "m", "r", "zeta", "alpha", "a", "b", "path", "d", "count",
        ];
        for step in 0..2_000i64 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let name = names[(x >> 33) as usize % names.len()].to_string();
            let v = Value::Int(step);
            match (x >> 20) % 5 {
                0 | 1 => assert_eq!(ours.insert(name.clone(), v.clone()), theirs.insert(name, v)),
                2 => assert_eq!(ours.remove(&name), theirs.remove(&name)),
                3 => {
                    let a = ours.entry(name.clone()).or_insert(v.clone()).clone();
                    let b = theirs.entry(name).or_insert(v).clone();
                    assert_eq!(a, b);
                }
                _ => {
                    assert_eq!(ours.get(&name), theirs.get(&name));
                    assert_eq!(ours.contains_key(&name), theirs.contains_key(&name));
                }
            }
            assert_eq!(ours.len(), theirs.len());
            let k1: Vec<&String> = ours.keys().collect();
            let k2: Vec<&String> = theirs.keys().collect();
            assert_eq!(k1, k2, "iteration order is the sorted order");
        }
        assert_eq!(ours.to_btree(), theirs);
        let back: VarMap = theirs.clone().into();
        assert_eq!(back, ours);
    }
}
