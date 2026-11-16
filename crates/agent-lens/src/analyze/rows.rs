//! Flat row views of an analyzer's JSON report: `--format jsonl` and
//! `--format tsv`.
//!
//! Every analyzer nests its findings differently — `files[].functions[]`,
//! `clusters[].pairs[]`, `visibility.modules[].findings[]` — and puts the
//! file a finding belongs to wherever its own shape suggests, often only
//! on the parent. A consumer therefore needs one path per analyzer, and a
//! recursive descent loses whatever lived on the parent. The row view
//! removes both: one self-contained record per finding, the same way for
//! every analyzer, built from the JSON report so it can never disagree
//! with it.
//!
//! The rules, applied to the report as a tree:
//!
//! * Every object that is an element of an array is one **row**. Its
//!   `kind` is the dotted path of keys leading to that array
//!   (`files.functions`).
//! * A row carries the object's own fields. A nested plain object is
//!   folded into dotted names (`a.file`, `a.start_line`); a field that
//!   is itself an array of objects is left out, because its elements are
//!   rows of their own.
//! * A row inherits the string fields of every enclosing row, so the
//!   `file` on a `files[]` entry reaches each of its `functions[]`. Where
//!   the row has a field of the same name, the inherited one is kept
//!   under `<last key of the kind it came from>.<name>` (`files.name`).
//! * Fields of the report that sit outside any array (`summary`, a
//!   top-level `note`) are not rows and are not emitted; the JSON report
//!   is where they live.
//!
//! [`Detail::Concise`] keeps only a row's scalar fields, dropping arrays
//! and nulls — the member and caller lists that dominate a row's size.

use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::{Detail, ReportFormat};

/// One finding, self-contained.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Dotted path of the keys leading to the array this row came from.
    pub kind: String,
    /// The row's own fields followed by the inherited ones, in report
    /// order.
    pub fields: Map<String, Value>,
}

/// Flatten a JSON `report` into its rows, in report order.
pub fn flatten(report: &Value, detail: Detail) -> Vec<Row> {
    let mut rows = Vec::new();
    walk_container(report, "", &Inherited::new(), detail, &mut rows);
    rows
}

/// Render `report` — one analyzer's JSON output — in a row `format`.
/// `analyzer` is the `analyze` subcommand name, stamped on every record.
///
/// For [`ReportFormat::Json`] and [`ReportFormat::Md`], which are not
/// row formats, the report's compact JSON is returned unchanged.
pub fn render(analyzer: &str, report: &Value, format: ReportFormat, detail: Detail) -> String {
    let rows = flatten(report, detail);
    match format {
        ReportFormat::Jsonl => render_jsonl(analyzer, &rows),
        ReportFormat::Tsv => render_tsv(analyzer, &rows),
        ReportFormat::Json | ReportFormat::Md => report.to_string(),
    }
}

/// Descend through a non-row node — the report root, or a plain object
/// nested in it — looking for arrays of objects.
fn walk_container(
    node: &Value,
    path: &str,
    inherited: &Inherited,
    detail: Detail,
    rows: &mut Vec<Row>,
) {
    let Value::Object(object) = node else {
        return;
    };
    for (key, value) in object {
        let child_path = join(path, key);
        match value {
            Value::Array(items) if is_row_array(items) => {
                for item in items {
                    walk_row(item, &child_path, inherited, detail, rows);
                }
            }
            Value::Object(_) => walk_container(value, &child_path, inherited, detail, rows),
            _ => {}
        }
    }
}

/// A string field a row hands down to the rows nested in it: its name,
/// the last key of the `kind` it came from, and its value.
type Inherited = Vec<(String, String, Value)>;

/// Emit `node`, an element of the array at `kind`, then its child rows.
fn walk_row(node: &Value, kind: &str, inherited: &Inherited, detail: Detail, rows: &mut Vec<Row>) {
    let Value::Object(object) = node else {
        return;
    };
    let mut own = OwnFields::default();
    own.collect(object, "");
    let mut fields = Map::new();
    for (name, value) in &own.values {
        if detail.keeps(value) {
            fields.insert(name.clone(), (*value).clone());
        }
    }
    for (name, origin, value) in inherited {
        // A clash is decided on the row's own names, kept or not, so a
        // field has the same name at every detail level.
        let clashes = own.values.iter().any(|(own_name, _)| own_name == name);
        let name = if clashes {
            format!("{origin}.{name}")
        } else {
            name.clone()
        };
        fields.entry(name).or_insert_with(|| value.clone());
    }
    rows.push(Row {
        kind: kind.to_owned(),
        fields,
    });
    if own.child_rows.is_empty() {
        return;
    }
    // Nearest first: this row's strings, then what it inherited.
    let origin = kind.rsplit('.').next().unwrap_or(kind);
    let mut handed_down: Inherited = own
        .values
        .iter()
        .filter(|(_, value)| value.is_string())
        .map(|(name, value)| (name.clone(), origin.to_owned(), (*value).clone()))
        .collect();
    handed_down.extend(inherited.iter().cloned());
    for (key, items) in &own.child_rows {
        let child_kind = join(kind, key);
        for item in *items {
            walk_row(item, &child_kind, &handed_down, detail, rows);
        }
    }
}

/// What one row object holds itself, with nested plain objects folded
/// into dotted names (`a.file`), and the arrays of objects that are rows
/// of their own set aside.
#[derive(Default)]
struct OwnFields<'a> {
    values: Vec<(String, &'a Value)>,
    child_rows: Vec<(String, &'a [Value])>,
}

impl<'a> OwnFields<'a> {
    fn collect(&mut self, object: &'a Map<String, Value>, prefix: &str) {
        for (key, value) in object {
            let name = format!("{prefix}{key}");
            match value {
                Value::Array(items) if is_row_array(items) => self.child_rows.push((name, items)),
                Value::Object(nested) => self.collect(nested, &format!("{name}.")),
                other => self.values.push((name, other)),
            }
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// A non-empty array whose every element is an object: a list of rows.
fn is_row_array(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(Value::is_object)
}

impl Detail {
    /// Whether a row keeps a field holding `value`.
    fn keeps(self, value: &Value) -> bool {
        match self {
            Self::Detailed => true,
            Self::Concise => !matches!(value, Value::Array(_) | Value::Null),
        }
    }
}

/// One JSON object per line: `analyzer`, `kind`, then the row's fields.
fn render_jsonl(analyzer: &str, rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        let mut record = Map::new();
        record.insert("analyzer".to_owned(), Value::String(analyzer.to_owned()));
        record.insert("kind".to_owned(), Value::String(row.kind.clone()));
        for (key, value) in &row.fields {
            // A report field called `analyzer` or `kind` keeps its value
            // under a name that cannot be mistaken for the stamp.
            let name = match key.as_str() {
                "analyzer" | "kind" => format!("row.{key}"),
                _ => key.clone(),
            };
            record.insert(name, value.clone());
        }
        out.push_str(&Value::Object(record).to_string());
        out.push('\n');
    }
    out
}

/// One tab-separated table per `kind`, in order of first appearance.
/// Each table opens with a `# analyzer=… kind=… rows=…` line — the only
/// lines that start with `#` — followed by a header row and `rows` data
/// lines; its columns are the union of the fields its rows carry, in
/// order of first appearance. A kind whose rows carry no field at all is
/// that one line.
fn render_tsv(analyzer: &str, rows: &[Row]) -> String {
    let mut kinds: Vec<&str> = Vec::new();
    for row in rows {
        if !kinds.contains(&row.kind.as_str()) {
            kinds.push(&row.kind);
        }
    }
    let mut out = String::new();
    for kind in kinds {
        let table: Vec<&Row> = rows.iter().filter(|row| row.kind == kind).collect();
        let mut columns: Vec<&str> = Vec::new();
        for row in &table {
            for key in row.fields.keys() {
                if !columns.contains(&key.as_str()) {
                    columns.push(key);
                }
            }
        }
        let _ = writeln!(
            out,
            "# analyzer={analyzer} kind={kind} rows={}",
            table.len()
        );
        // Rows without a single field have no grid to write; the count
        // above is all there is to say.
        if columns.is_empty() {
            continue;
        }
        push_tsv_line(&mut out, columns.iter().map(|column| (*column).to_owned()));
        for row in table {
            push_tsv_line(
                &mut out,
                columns
                    .iter()
                    .map(|column| row.fields.get(*column).map(tsv_cell).unwrap_or_default()),
            );
        }
    }
    out
}

/// Write one line of a table. Only a `#` line opens a table, so a data
/// line that would start with one is written with it escaped.
fn push_tsv_line(out: &mut String, cells: impl Iterator<Item = String>) {
    let line = cells.collect::<Vec<_>>().join("\t");
    if line.starts_with('#') {
        out.push('\\');
    }
    out.push_str(&line);
    out.push('\n');
}

/// A value as one TSV cell: strings bare, `null` empty, nested values as
/// compact JSON. Tabs, newlines and backslashes are escaped so a cell
/// never breaks the grid.
fn tsv_cell(value: &Value) -> String {
    let text = match value {
        Value::Null => return String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let mut cell = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => cell.push_str("\\\\"),
            '\t' => cell.push_str("\\t"),
            '\n' => cell.push_str("\\n"),
            '\r' => cell.push_str("\\r"),
            other => cell.push(other),
        }
    }
    cell
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn kinds(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.kind.as_str()).collect()
    }

    /// The complexity shape that motivated the format (#585): the file
    /// lives only on the parent `files[]` entry.
    fn complexity_report() -> Value {
        json!({
            "summary": {"function_count": 2, "file_rollups": [{"path": "a.rs", "cognitive_sum": 3}]},
            "files": [{
                "file": "a.rs",
                "function_count": 2,
                "functions": [
                    {"name": "f", "start_line": 1, "cognitive": 3},
                    {"name": "g", "start_line": 9, "cognitive": 0},
                ],
            }],
        })
    }

    #[test]
    fn a_nested_row_inherits_the_strings_of_its_parent() {
        let rows = flatten(&complexity_report(), Detail::Detailed);
        assert_eq!(
            kinds(&rows),
            [
                "summary.file_rollups",
                "files",
                "files.functions",
                "files.functions"
            ],
        );
        assert_eq!(
            Value::Object(rows[2].fields.clone()),
            json!({"name": "f", "start_line": 1, "cognitive": 3, "file": "a.rs"}),
        );
        // The parent is a row too, without the children it handed off
        // and without numbers leaking down into them.
        assert_eq!(
            Value::Object(rows[1].fields.clone()),
            json!({"file": "a.rs", "function_count": 2}),
        );
        assert!(!rows[2].fields.contains_key("function_count"));
    }

    #[test]
    fn inheritance_reaches_through_every_level_and_plain_objects() {
        let report = json!({"visibility": {"note": "n", "modules": [{
            "module": "m",
            "findings": [{"id": "x", "callers": [{"line": 4}]}],
        }]}});
        let rows = flatten(&report, Detail::Detailed);
        assert_eq!(
            kinds(&rows),
            [
                "visibility.modules",
                "visibility.modules.findings",
                "visibility.modules.findings.callers",
            ],
        );
        assert_eq!(
            Value::Object(rows[2].fields.clone()),
            json!({"line": 4, "id": "x", "module": "m"}),
        );
        // `note` sits outside any array: not a row, not inherited.
        assert!(rows.iter().all(|row| !row.fields.contains_key("note")));
    }

    #[test]
    fn a_clashing_inherited_field_is_kept_under_its_owners_name() {
        let report = json!({"files": [{"name": "a.rs", "functions": [
            {"name": "f", "calls": [{"name": "g"}]},
        ]}]});
        let rows = flatten(&report, Detail::Detailed);
        assert_eq!(
            Value::Object(rows[1].fields.clone()),
            json!({"name": "f", "files.name": "a.rs"}),
        );
        // The grandchild sees both ancestors, each under the array it
        // came from.
        assert_eq!(
            Value::Object(rows[2].fields.clone()),
            json!({"name": "g", "functions.name": "f", "files.name": "a.rs"}),
        );
    }

    #[rstest]
    #[case::scalar_array(json!({"tags": ["a", "b"]}))]
    #[case::empty_array(json!({"rows": []}))]
    #[case::mixed_array(json!({"rows": [{"a": 1}, 2]}))]
    #[case::scalars_only(json!({"count": 3, "note": "n"}))]
    #[case::not_an_object(json!([{"a": 1}]))]
    fn a_report_without_an_array_of_objects_has_no_rows(#[case] report: Value) {
        assert_eq!(flatten(&report, Detail::Detailed), []);
    }

    #[test]
    fn concise_keeps_scalars_and_drops_arrays_and_nulls() {
        let report = json!({"units": [{
            "label": "impl A",
            "lcom4": 2,
            "inert": false,
            "lcom96": null,
            "components": [["a"], ["b"]],
            "owner": {"file": "a.rs"},
            "methods": [{"name": "a", "fields": ["x"]}],
        }]});
        let rows = flatten(&report, Detail::Concise);
        assert_eq!(
            Value::Object(rows[0].fields.clone()),
            json!({"label": "impl A", "lcom4": 2, "inert": false, "owner.file": "a.rs"}),
        );
        assert_eq!(
            Value::Object(rows[1].fields.clone()),
            json!({"name": "a", "label": "impl A", "owner.file": "a.rs"}),
        );
        let detailed = flatten(&report, Detail::Detailed);
        assert_eq!(detailed[0].fields["components"], json!([["a"], ["b"]]));
        assert_eq!(detailed[0].fields["lcom96"], Value::Null);
        assert_eq!(detailed[1].fields["fields"], json!(["x"]));
    }

    #[test]
    fn jsonl_stamps_every_line_and_each_line_is_one_object() {
        let out = render(
            "complexity",
            &complexity_report(),
            ReportFormat::Jsonl,
            Detail::Detailed,
        );
        let lines: Vec<Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[2],
            json!({
                "analyzer": "complexity", "kind": "files.functions",
                "name": "f", "start_line": 1, "cognitive": 3, "file": "a.rs",
            }),
        );
        // The stamp leads, so `grep '"kind":"files.functions"'` and a
        // prefix match both work.
        assert!(
            out.lines()
                .all(|l| l.starts_with(r#"{"analyzer":"complexity","kind":""#))
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn jsonl_keeps_a_report_field_named_like_the_stamp() {
        let report = json!({"units": [{"kind": "impl", "analyzer": "x", "label": "A"}]});
        let out = render("cohesion", &report, ReportFormat::Jsonl, Detail::Detailed);
        assert_eq!(
            serde_json::from_str::<Value>(out.trim()).unwrap(),
            json!({
                "analyzer": "cohesion", "kind": "units",
                "row.kind": "impl", "row.analyzer": "x", "label": "A",
            }),
        );
    }

    #[test]
    fn tsv_writes_one_table_per_kind_with_a_union_header() {
        let report = json!({
            "files": [{"path": "a.rs", "score": 3}, {"path": "b.rs", "commits": 2}],
            "pairs": [{"a": "x", "b": "y"}],
        });
        let out = render("hotspot", &report, ReportFormat::Tsv, Detail::Detailed);
        assert_eq!(
            out,
            "# analyzer=hotspot kind=files rows=2\n\
             path\tscore\tcommits\n\
             a.rs\t3\t\n\
             b.rs\t\t2\n\
             # analyzer=hotspot kind=pairs rows=1\n\
             a\tb\n\
             x\ty\n",
        );
    }

    #[rstest]
    #[case::null(json!(null), "")]
    #[case::string(json!("a b"), "a b")]
    #[case::number(json!(1.5), "1.5")]
    #[case::bool(json!(true), "true")]
    #[case::array(json!(["a", 1]), r#"["a",1]"#)]
    #[case::object(json!({"k": "v"}), r#"{"k":"v"}"#)]
    #[case::tab_and_newline(json!("a\tb\nc\rd"), r"a\tb\nc\rd")]
    #[case::backslash(json!(r"a\tb"), r"a\\tb")]
    fn tsv_cells_never_break_the_grid(#[case] value: Value, #[case] cell: &str) {
        assert_eq!(tsv_cell(&value), cell);
    }

    #[test]
    fn a_nested_plain_object_folds_into_dotted_names_and_keeps_its_rows() {
        let report = json!({"pairs": [{
            "a": {"file": "x.rs", "span": {"start": 1}},
            "similarity": 0.9,
            "evidence": {"sites": [{"line": 7}]},
        }]});
        let rows = flatten(&report, Detail::Detailed);
        assert_eq!(kinds(&rows), ["pairs", "pairs.evidence.sites"]);
        assert_eq!(
            Value::Object(rows[0].fields.clone()),
            json!({"a.file": "x.rs", "a.span.start": 1, "similarity": 0.9}),
        );
        assert_eq!(
            Value::Object(rows[1].fields.clone()),
            json!({"line": 7, "a.file": "x.rs"}),
        );
    }

    #[test]
    fn tsv_writes_only_the_count_for_rows_without_fields() {
        let report = json!({"empty": [{}, {}], "files": [{"path": "a.rs"}]});
        assert_eq!(
            render("tool", &report, ReportFormat::Tsv, Detail::Detailed),
            "# analyzer=tool kind=empty rows=2\n# analyzer=tool kind=files rows=1\npath\na.rs\n",
        );
    }

    #[test]
    fn tsv_escapes_a_data_line_that_would_read_as_a_table_opening() {
        let report = json!({"notes": [{"text": "# heading"}, {"text": ""}]});
        assert_eq!(
            render("tool", &report, ReportFormat::Tsv, Detail::Detailed),
            "# analyzer=tool kind=notes rows=2\ntext\n\\# heading\n\n",
        );
    }

    #[test]
    fn a_report_without_rows_renders_as_nothing() {
        let report = json!({"summary": {"count": 0}, "files": []});
        for format in [ReportFormat::Jsonl, ReportFormat::Tsv] {
            assert_eq!(render("complexity", &report, format, Detail::Detailed), "");
        }
    }

    #[test]
    fn the_document_formats_pass_the_report_through() {
        let report = json!({"files": [{"path": "a.rs"}]});
        for format in [ReportFormat::Json, ReportFormat::Md] {
            assert_eq!(
                render("hotspot", &report, format, Detail::Detailed),
                report.to_string(),
            );
        }
    }

    /// Arbitrary report-shaped JSON: objects of scalars, scalar arrays,
    /// nested objects and arrays of objects.
    fn arb_report() -> impl Strategy<Value = Value> {
        let key = prop::sample::select(vec!["file", "name", "n", "rows", "items", "meta"]);
        let scalar = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i16>().prop_map(Value::from),
            "[a-z\t\n\\\\]{0,6}".prop_map(Value::from),
        ];
        let leaf = prop_oneof![
            scalar.clone(),
            prop::collection::vec(scalar, 0..3).prop_map(Value::Array),
        ];
        leaf.prop_recursive(4, 48, 4, move |inner| {
            let object = prop::collection::vec((key.clone(), inner), 0..4).prop_map(|entries| {
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect(),
                )
            });
            prop_oneof![
                object.clone(),
                prop::collection::vec(object, 0..3).prop_map(Value::Array),
            ]
        })
        .prop_map(|value| json!({"root": value}))
    }

    /// Objects that are elements of an array, anywhere in `value`.
    fn count_array_objects(value: &Value, in_row_array: bool) -> usize {
        match value {
            Value::Object(object) => {
                usize::from(in_row_array)
                    + object
                        .values()
                        .map(|v| count_array_objects(v, false))
                        .sum::<usize>()
            }
            // Only an array a row walk would enter: reached through
            // objects alone, and made of objects alone.
            Value::Array(items) if is_row_array(items) => {
                items.iter().map(|v| count_array_objects(v, true)).sum()
            }
            _ => 0,
        }
    }

    proptest! {
        /// Lossless in count: every object in an array of objects that
        /// the report reaches through objects is exactly one row.
        #[test]
        fn every_array_object_is_exactly_one_row(report in arb_report()) {
            let rows = flatten(&report, Detail::Detailed);
            prop_assert_eq!(rows.len(), count_array_objects(&report, false));
        }

        /// Concise never adds a row or a field, and what it keeps is
        /// scalar and equal to the detailed value.
        #[test]
        fn concise_is_a_scalar_subset_of_detailed(report in arb_report()) {
            let detailed = flatten(&report, Detail::Detailed);
            let concise = flatten(&report, Detail::Concise);
            prop_assert_eq!(concise.len(), detailed.len());
            for (c, d) in concise.iter().zip(&detailed) {
                prop_assert_eq!(&c.kind, &d.kind);
                for (key, value) in &c.fields {
                    prop_assert!(!value.is_array() && !value.is_object() && !value.is_null());
                    prop_assert_eq!(Some(value), d.fields.get(key));
                }
            }
        }

        /// Every JSONL line parses back to an object carrying the stamp,
        /// one line per row.
        #[test]
        fn jsonl_round_trips_line_by_line(report in arb_report()) {
            let rows = flatten(&report, Detail::Detailed);
            let out = render("tool", &report, ReportFormat::Jsonl, Detail::Detailed);
            prop_assert_eq!(out.lines().count(), rows.len());
            for (line, row) in out.lines().zip(&rows) {
                let parsed: Value = serde_json::from_str(line).unwrap();
                prop_assert_eq!(&parsed["analyzer"], "tool");
                prop_assert_eq!(parsed["kind"].as_str(), Some(row.kind.as_str()));
            }
        }

        /// A TSV table is a grid: its `#` line says how many data lines
        /// follow the header, and each has as many cells as the header,
        /// whatever the values contain.
        #[test]
        fn tsv_tables_are_rectangular(report in arb_report()) {
            let out = render("tool", &report, ReportFormat::Tsv, Detail::Detailed);
            let mut lines = out.lines();
            while let Some(comment) = lines.next() {
                prop_assert!(comment.starts_with("# analyzer=tool kind="), "{}", comment);
                let rows: usize = comment.rsplit("rows=").next().unwrap().parse().unwrap();
                // A table of field-less rows is its `#` line alone.
                let mut rest = lines.clone();
                let Some(header) = rest.next().filter(|line| !line.starts_with('#')) else {
                    continue;
                };
                lines.next();
                let width = header.split('\t').count();
                for _ in 0..rows {
                    let line = lines.next().unwrap();
                    prop_assert!(!line.starts_with('#'), "{}", line);
                    prop_assert_eq!(line.split('\t').count(), width, "{}", line);
                }
            }
        }
    }
}
