//! `--format jsonl`: one self-contained JSON object per finding.
//!
//! The JSON reports nest findings under analyzer-specific paths
//! (`.files[].functions[]`, `.clusters[].pairs[]`,
//! `.modules[].findings[]`, …), and some keep a finding's file only on
//! the enclosing object, so a consumer needs a bespoke `jq` join per
//! analyzer. A JSONL record instead always opens with the same five
//! keys — `analyzer`, `kind`, `file`, `line`, `name` — followed by the
//! finding's own fields, so `grep` and `jq -c 'select(…)'` work the same
//! on every analyzer and no record depends on its parent.
//!
//! Records are derived from the analyzer's JSON report value rather
//! than from its internal types, so a record's fields are exactly the
//! fields the JSON report carries for that finding, and the two formats
//! cannot drift apart.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::{Map, Value};

use super::AnalyzerError;

/// The analyzers that render `--format jsonl`, as their CLI spelling.
/// `forwarding` renders it for its `wrapper` section only.
pub const SUPPORTED: &[&str] = &[
    "cohesion",
    "complexity",
    "forwarding --section wrapper",
    "narrowable",
    "similarity",
];

/// An analyzer was asked for `--format jsonl` and has no record shape
/// for it yet. Failing beats silently answering in another format: a
/// consumer parsing lines would choke on a pretty-printed report.
#[derive(Debug, thiserror::Error)]
#[error(
    "--format jsonl is not supported by this analyzer yet; supported: {}; use --format json",
    SUPPORTED.join(", ")
)]
pub struct UnsupportedFormat;

/// The keys every record opens with.
const FIXED_KEYS: &[&str] = &["analyzer", "kind", "file", "line", "name"];

/// Item keys a record's `line` is read from, in preference order. The
/// key that supplied it is dropped from the rest of the record.
const LINE_KEYS: &[&str] = &["line", "start_line"];

/// Item keys a record's `name` is read from, in preference order: the
/// item's own `name`, then its fully qualified name — which wins over a
/// display label, since it is the one unique across a corpus — then the
/// identifiers of findings that have no name.
const NAME_KEYS: &[&str] = &["name", "qualified_name", "label", "key", "id"];

/// Collects the JSONL records of one analyzer run.
#[derive(Debug)]
pub(crate) struct JsonlSink {
    analyzer: &'static str,
    out: String,
}

impl JsonlSink {
    pub fn new(analyzer: &'static str) -> Self {
        Self {
            analyzer,
            out: String::new(),
        }
    }

    /// Append one record for `item`, a finding object from the JSON
    /// report. `file` is the enclosing object's file, for reports that
    /// keep it there; an item's own `file` wins over it. `extra` fields
    /// (a cluster index, a section name, …) sit between the fixed keys
    /// and the item's own fields.
    pub fn push(
        &mut self,
        kind: &str,
        file: Option<&str>,
        item: &Value,
        extra: &[(&str, Value)],
    ) -> Result<(), AnalyzerError> {
        let mut fields = match item {
            Value::Object(map) => map.clone(),
            other => {
                let mut map = Map::new();
                map.insert("value".to_owned(), other.clone());
                map
            }
        };
        let file = match fields.shift_remove("file") {
            Some(Value::String(own)) => Some(own),
            Some(other) => {
                fields.insert("file".to_owned(), other);
                file.map(str::to_owned)
            }
            None => file.map(str::to_owned),
        };
        let line = take_first(&mut fields, LINE_KEYS);
        let name = take_first(&mut fields, NAME_KEYS);
        let record = Record {
            analyzer: self.analyzer,
            kind,
            file: file.as_deref(),
            line: line.as_ref(),
            name: name.as_ref(),
            extra,
            fields: &fields,
        };
        let line = serde_json::to_string(&record).map_err(AnalyzerError::Serialize)?;
        self.out.push_str(&line);
        self.out.push('\n');
        Ok(())
    }

    /// [`Self::push`] for every element of `items` (a JSON array);
    /// anything else pushes nothing.
    pub fn push_all(
        &mut self,
        kind: &str,
        file: Option<&str>,
        items: &Value,
        extra: &[(&str, Value)],
    ) -> Result<(), AnalyzerError> {
        for item in items.as_array().into_iter().flatten() {
            self.push(kind, file, item, extra)?;
        }
        Ok(())
    }

    /// The records, one per line. Empty when the run found nothing.
    pub fn finish(self) -> String {
        self.out
    }
}

/// `item` with the location keys (`file`, `name`, `qualified_name`,
/// `start_line`) of its nested object `side` copied to the top, for
/// findings that are a relation — a pair, a chain — rather than a
/// located unit. The nested object stays, so nothing is lost.
pub(crate) fn lifted(item: &Value, side: &str) -> Value {
    let anchor = item[side].as_object();
    let location = ["file", "name", "qualified_name", "start_line"]
        .into_iter()
        .filter_map(|key| Some((key.to_owned(), anchor?.get(key)?.clone())));
    let own = item
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| (key.clone(), value.clone()));
    let mut out = Map::new();
    for (key, value) in location.chain(own) {
        out.entry(key).or_insert(value);
    }
    Value::Object(out)
}

/// Remove and return the first of `keys` present in `fields`, skipping
/// `null`s so an absent optional never masks a later key.
fn take_first(fields: &mut Map<String, Value>, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|key| {
        let present = fields.get(*key).is_some_and(|v| !v.is_null());
        present.then(|| fields.shift_remove(*key)).flatten()
    })
}

/// One JSONL line: the fixed keys first, always present (as `null`
/// when the finding has no such field), then the extras, then the
/// finding's remaining fields.
struct Record<'a> {
    analyzer: &'a str,
    kind: &'a str,
    file: Option<&'a str>,
    line: Option<&'a Value>,
    name: Option<&'a Value>,
    extra: &'a [(&'a str, Value)],
    fields: &'a Map<String, Value>,
}

impl Record<'_> {
    /// `item_<key>`, prefixed again while that collides with another of
    /// the record's keys.
    fn renamed(&self, key: &str) -> String {
        let mut name = format!("item_{key}");
        while self.fields.contains_key(&name) || self.extra.iter().any(|(extra, _)| *extra == name)
        {
            name.insert_str(0, "item_");
        }
        name
    }
}

impl Serialize for Record<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("analyzer", self.analyzer)?;
        map.serialize_entry("kind", self.kind)?;
        map.serialize_entry("file", &self.file)?;
        map.serialize_entry("line", &self.line)?;
        map.serialize_entry("name", &self.name)?;
        for (key, value) in self.extra {
            map.serialize_entry(key, value)?;
        }
        for (key, value) in self.fields {
            if self.extra.iter().any(|(extra, _)| extra == key) {
                continue;
            }
            if FIXED_KEYS.contains(&key.as_str()) {
                // A finding's own `kind` (a type unit's `struct`, an
                // abstraction's `trait`) must not repeat a fixed key: a
                // JSON object with a duplicate key parses differently
                // per reader.
                map.serialize_entry(&self.renamed(key), value)?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn lines(sink: JsonlSink) -> Vec<Value> {
        sink.finish()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_record_opens_with_the_fixed_keys_in_order() {
        let mut sink = JsonlSink::new("complexity");
        sink.push(
            "function",
            Some("src/a.rs"),
            &json!({"name": "f", "start_line": 3, "end_line": 9, "cognitive": 4}),
            &[],
        )
        .unwrap();
        let out = sink.finish();
        assert_eq!(
            out,
            "{\"analyzer\":\"complexity\",\"kind\":\"function\",\"file\":\"src/a.rs\",\
             \"line\":3,\"name\":\"f\",\"end_line\":9,\"cognitive\":4}\n",
        );
    }

    #[rstest]
    #[case::own_file_wins(json!({"file": "own.rs", "name": "f"}), Some("parent.rs"), json!("own.rs"))]
    #[case::parent_file(json!({"name": "f"}), Some("parent.rs"), json!("parent.rs"))]
    #[case::no_file(json!({"name": "f"}), None, Value::Null)]
    fn file_comes_from_the_item_then_its_parent(
        #[case] item: Value,
        #[case] parent: Option<&str>,
        #[case] expected: Value,
    ) {
        let mut sink = JsonlSink::new("a");
        sink.push("k", parent, &item, &[]).unwrap();
        let records = lines(sink);
        assert_eq!(records[0]["file"], expected);
    }

    #[rstest]
    #[case::name(json!({"name": "n", "qualified_name": "q"}), "n", Some("qualified_name"))]
    #[case::qualified(json!({"qualified_name": "q", "display_name": "d"}), "q", Some("display_name"))]
    #[case::null_name_falls_through(json!({"name": null, "qualified_name": "q"}), "q", Some("name"))]
    fn name_takes_the_first_present_key_and_keeps_the_rest(
        #[case] item: Value,
        #[case] expected: &str,
        #[case] kept: Option<&str>,
    ) {
        let mut sink = JsonlSink::new("a");
        sink.push("k", None, &item, &[]).unwrap();
        let record = &lines(sink)[0];
        assert_eq!(record["name"], expected);
        if let Some(kept) = kept {
            assert!(record.get(kept).is_some(), "{kept} kept: {record}");
        }
    }

    #[test]
    fn extras_sit_after_the_fixed_keys_and_shadow_item_fields() {
        let mut sink = JsonlSink::new("similarity");
        sink.push(
            "pair",
            None,
            &json!({"cluster": "stale", "similarity": 0.9}),
            &[("cluster", json!(2))],
        )
        .unwrap();
        let out = sink.finish();
        assert!(
            out.starts_with(
                "{\"analyzer\":\"similarity\",\"kind\":\"pair\",\"file\":null,\"line\":null,\
                 \"name\":null,\"cluster\":2,"
            ),
            "got: {out}",
        );
        assert_eq!(out.matches("cluster").count(), 1, "got: {out}");
    }

    #[test]
    fn push_all_pushes_each_array_element_and_ignores_the_rest() {
        let mut sink = JsonlSink::new("a");
        sink.push_all("k", None, &json!([{"name": "x"}, {"name": "y"}]), &[])
            .unwrap();
        sink.push_all("k", None, &Value::Null, &[]).unwrap();
        let names: Vec<Value> = lines(sink).iter().map(|r| r["name"].clone()).collect();
        assert_eq!(names, vec![json!("x"), json!("y")]);
    }

    #[test]
    fn an_empty_run_renders_nothing() {
        assert_eq!(JsonlSink::new("a").finish(), "");
    }

    #[test]
    fn lifted_copies_the_anchor_location_and_keeps_the_rest() {
        let pair = json!({
            "a": {"file": "x.rs", "name": "f", "start_line": 4, "end_line": 9},
            "b": {"file": "y.rs", "name": "g", "start_line": 1},
            "similarity": 0.9,
        });
        let mut sink = JsonlSink::new("similarity");
        sink.push("pair", None, &lifted(&pair, "a"), &[]).unwrap();
        let record = &lines(sink)[0];
        assert_eq!(record["file"], "x.rs");
        assert_eq!(record["line"], 4);
        assert_eq!(record["name"], "f");
        assert_eq!(record["b"]["name"], "g");
        assert_eq!(record["a"]["end_line"], 9);
        assert_eq!(record["similarity"], 0.9);
    }

    #[test]
    fn lifted_without_the_anchor_is_the_item() {
        let item = json!({"size": 2});
        assert_eq!(lifted(&item, "sink"), item);
    }

    #[test]
    fn an_item_field_named_like_a_fixed_key_is_renamed() {
        let mut sink = JsonlSink::new("narrowable");
        sink.push(
            "single-impl",
            None,
            &json!({"name": "T", "kind": "trait"}),
            &[],
        )
        .unwrap();
        let out = sink.finish();
        assert_eq!(out.matches("\"kind\"").count(), 1, "got: {out}");
        let record: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(record["kind"], "single-impl");
        assert_eq!(record["item_kind"], "trait");
    }

    proptest::proptest! {
        /// Whatever fields a finding carries, its record is one JSON
        /// object line, opens with the five fixed keys, repeats no key,
        /// and drops none of the finding's values.
        #[test]
        fn every_record_is_one_line_with_the_fixed_keys_first_and_no_duplicates(
            fields in proptest::collection::btree_map(
                proptest::sample::select(vec![
                    "analyzer", "kind", "file", "line", "start_line", "name",
                    "qualified_name", "label", "end_line", "score", "item_kind",
                ]),
                proptest::prelude::any::<i64>(),
                0..8,
            ),
        ) {
            let item = Value::Object(
                fields.iter().map(|(k, v)| ((*k).to_owned(), Value::from(*v))).collect(),
            );
            let mut sink = JsonlSink::new("a");
            sink.push("k", None, &item, &[]).unwrap();
            let out = sink.finish();
            proptest::prop_assert_eq!(out.matches('\n').count(), 1);
            let mut seen = std::collections::HashSet::new();
            let mut de = serde_json::Deserializer::from_str(out.trim_end());
            let keys: Vec<String> = {
                use serde::de::Deserializer as _;
                struct Keys;
                impl<'de> serde::de::Visitor<'de> for Keys {
                    type Value = Vec<String>;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("an object")
                    }
                    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                        let mut keys = Vec::new();
                        while let Some((key, _)) = map.next_entry::<String, serde::de::IgnoredAny>()? {
                            keys.push(key);
                        }
                        Ok(keys)
                    }
                }
                (&mut de).deserialize_map(Keys).unwrap()
            };
            proptest::prop_assert_eq!(&keys[..5], FIXED_KEYS);
            for key in &keys {
                proptest::prop_assert!(seen.insert(key.clone()), "duplicate key {} in {}", key, out);
            }
            // Every input value survives somewhere in the record: the
            // fixed `analyzer`/`kind` slots hold the sink's own values,
            // so a colliding input shows up renamed.
            let record: Value = serde_json::from_str(&out).unwrap();
            let values: Vec<&Value> = record.as_object().unwrap().values().collect();
            for (key, value) in &fields {
                proptest::prop_assert!(
                    values.contains(&&Value::from(*value)),
                    "{} lost from {}", key, out,
                );
            }
        }
    }

    /// The renamed key steps around an extra of the same name, not only
    /// around the item's own fields.
    #[test]
    fn a_renamed_field_does_not_collide_with_an_extra() {
        let mut sink = JsonlSink::new("a");
        sink.push(
            "k",
            None,
            &json!({"kind": "struct"}),
            &[("item_kind", json!(1))],
        )
        .unwrap();
        let record = &lines(sink)[0];
        assert_eq!(record["item_kind"], 1);
        assert_eq!(record["item_item_kind"], "struct");
    }
}
