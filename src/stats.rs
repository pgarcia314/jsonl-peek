//! Single-pass file statistics: line/byte counts, the top-level key table and
//! per-field value distributions.
//!
//! Everything here is gathered in one pass over the input with bounded
//! memory: the key table and each field's distinct value table stop growing
//! past a fixed size and say so rather than keeping every value ever seen.
//!
//! ```
//! use jsonl_peek::{FieldPath, Stats, StatsOptions};
//! use std::io::Cursor;
//!
//! let data = b"{\"role\":\"user\"}\n{\"role\":\"assistant\"}\nnot json\n";
//! let options = StatsOptions {
//!     fields: vec![FieldPath::parse("role").unwrap()],
//!     ..StatsOptions::default()
//! };
//! let stats = Stats::from_reader(Cursor::new(&data[..]), options).unwrap();
//! assert_eq!(stats.lines, 3);
//! assert_eq!(stats.valid, 2);
//! assert_eq!(stats.invalid, 1);
//! assert_eq!(stats.fields[0].top(5), vec![("\"assistant\"", 1), ("\"user\"", 1)]);
//! ```

use std::collections::HashMap;
use std::io::{self, BufRead};

use crate::hist::Histogram;
use crate::json::{self, Value};
use crate::lines::LineReader;
use crate::path::FieldPath;

/// Top-level key table entries stop growing past this many distinct keys.
pub const MAX_KEYS: usize = 512;

/// Each profiled field's distinct value table stops growing past this size.
pub const MAX_FIELD_VALUES: usize = 10_000;

/// Default number of broken lines kept in [`Stats::issues`].
pub const DEFAULT_MAX_ERRORS: usize = 10;

/// Options controlling a [`Stats::from_reader`] pass.
#[derive(Debug, Clone)]
pub struct StatsOptions {
    /// Field paths to profile the value distribution of.
    pub fields: Vec<FieldPath>,
    /// Maximum number of broken lines recorded in [`Stats::issues`].
    pub max_errors: usize,
}

impl Default for StatsOptions {
    fn default() -> Self {
        StatsOptions {
            fields: Vec::new(),
            max_errors: DEFAULT_MAX_ERRORS,
        }
    }
}

/// How often each JSON type was seen, in first-seen order.
#[derive(Debug, Clone, Default)]
pub struct TypeCounts {
    counts: Vec<(&'static str, u64)>,
}

impl TypeCounts {
    pub(crate) fn record(&mut self, type_name: &'static str) {
        match self.counts.iter_mut().find(|(t, _)| *t == type_name) {
            Some(entry) => entry.1 += 1,
            None => self.counts.push((type_name, 1)),
        }
    }

    /// Type name and occurrence count, in the order each type was first seen.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.counts.iter().copied()
    }

    /// Total number of recorded occurrences across all types.
    pub fn total(&self) -> u64 {
        self.counts.iter().map(|(_, n)| n).sum()
    }
}

/// One entry of the top-level key table: how often a key appeared and with
/// which value types.
#[derive(Debug, Clone)]
pub struct KeyEntry {
    /// The key name.
    pub key: String,
    /// Number of records in which the key was present.
    pub count: u64,
    /// Types the key's value took across those records.
    pub types: TypeCounts,
}

/// Occurrence counts of the top-level object keys, in first-seen order.
///
/// Only records whose top-level value is an object contribute to this table.
#[derive(Debug, Clone, Default)]
pub struct KeyTable {
    entries: Vec<KeyEntry>,
    truncated: bool,
}

impl KeyTable {
    fn record(&mut self, fields: &[(String, Value)]) {
        // A record can repeat a key (the parser keeps duplicates), so only
        // count the first occurrence per record towards `count`.
        let mut seen_in_record: Vec<&str> = Vec::new();
        for (key, value) in fields {
            if seen_in_record.contains(&key.as_str()) {
                continue;
            }
            seen_in_record.push(key.as_str());

            match self.entries.iter_mut().find(|e| e.key == *key) {
                Some(entry) => {
                    entry.count += 1;
                    entry.types.record(value.type_name());
                }
                None => {
                    if self.entries.len() >= MAX_KEYS {
                        self.truncated = true;
                        continue;
                    }
                    let mut types = TypeCounts::default();
                    types.record(value.type_name());
                    self.entries.push(KeyEntry {
                        key: key.clone(),
                        count: 1,
                        types,
                    });
                }
            }
        }
    }

    /// The recorded keys, in the order each was first seen.
    pub fn entries(&self) -> &[KeyEntry] {
        &self.entries
    }

    /// True once the number of distinct keys reached [`MAX_KEYS`].
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Value distribution of one profiled [`FieldPath`].
#[derive(Debug, Clone)]
pub struct FieldStats {
    /// The path this profile was built for.
    pub path: FieldPath,
    /// Number of records in which the path selected at least one value.
    pub records_present: u64,
    /// Total number of values matched (more than one per record when the
    /// path contains a wildcard).
    pub values: u64,
    /// Types the matched values took.
    pub types: TypeCounts,
    distinct: HashMap<String, u64>,
    truncated: bool,
}

impl FieldStats {
    fn new(path: FieldPath) -> Self {
        FieldStats {
            path,
            records_present: 0,
            values: 0,
            types: TypeCounts::default(),
            distinct: HashMap::new(),
            truncated: false,
        }
    }

    fn record(&mut self, selected: &[&Value]) {
        if selected.is_empty() {
            return;
        }
        self.records_present += 1;
        for value in selected {
            self.values += 1;
            self.types.record(value.type_name());
            let key = value.to_json();
            if let Some(count) = self.distinct.get_mut(&key) {
                *count += 1;
            } else if self.distinct.len() < MAX_FIELD_VALUES {
                self.distinct.insert(key, 1);
            } else {
                self.truncated = true;
            }
        }
    }

    /// Number of distinct values recorded (capped at [`MAX_FIELD_VALUES`]).
    pub fn distinct(&self) -> usize {
        self.distinct.len()
    }

    /// True once the number of distinct values reached [`MAX_FIELD_VALUES`].
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The `n` most frequent values, each as its compact JSON encoding, most
    /// frequent first. Ties break by the encoding itself, for a stable order.
    pub fn top(&self, n: usize) -> Vec<(&str, u64)> {
        let mut values: Vec<(&str, u64)> =
            self.distinct.iter().map(|(v, &c)| (v.as_str(), c)).collect();
        values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        values.truncate(n);
        values
    }
}

/// One broken line: a byte offset and a human-readable reason.
#[derive(Debug, Clone)]
pub struct Issue {
    /// 1-based line number.
    pub line: u64,
    /// 1-based byte column within the line.
    pub column: usize,
    /// Why the line was rejected.
    pub reason: String,
}

/// The result of a [`Stats::from_reader`] pass over a JSONL file.
#[derive(Debug, Clone)]
pub struct Stats {
    /// Total number of lines read, blank lines included.
    pub lines: u64,
    /// Lines containing only whitespace.
    pub blank: u64,
    /// Non-blank lines that parsed as valid JSON.
    pub valid: u64,
    /// Non-blank lines that were not valid UTF-8 or not valid JSON.
    pub invalid: u64,
    /// Total bytes consumed from the input.
    pub bytes: u64,
    /// Byte length of each non-blank line.
    pub line_length: Histogram,
    /// Top-level JSON type of each valid record.
    pub top_level_types: TypeCounts,
    /// Top-level object keys, for records whose top-level value is an object.
    pub keys: KeyTable,
    /// One entry per field path in [`StatsOptions::fields`], same order.
    pub fields: Vec<FieldStats>,
    /// The first broken lines encountered, up to [`StatsOptions::max_errors`].
    pub issues: Vec<Issue>,
    /// True when more lines were broken than fit in `issues`.
    pub issues_truncated: bool,
}

impl Stats {
    /// Reads and profiles a whole JSONL stream in a single pass.
    pub fn from_reader<R: BufRead>(reader: R, options: StatsOptions) -> io::Result<Stats> {
        let max_errors = options.max_errors;
        let mut stats = Stats {
            lines: 0,
            blank: 0,
            valid: 0,
            invalid: 0,
            bytes: 0,
            line_length: Histogram::new(),
            top_level_types: TypeCounts::default(),
            keys: KeyTable::default(),
            fields: options.fields.into_iter().map(FieldStats::new).collect(),
            issues: Vec::new(),
            issues_truncated: false,
        };

        let mut lines = LineReader::new(reader);
        while let Some(line) = lines.next_line()? {
            stats.lines += 1;
            if line.is_blank() {
                stats.blank += 1;
                continue;
            }
            stats.line_length.record(line.bytes.len() as u64);

            let text = match std::str::from_utf8(line.bytes) {
                Ok(text) => text,
                Err(e) => {
                    stats.invalid += 1;
                    let column = e.valid_up_to() + 1;
                    push_issue(&mut stats, max_errors, line.number, column, "invalid UTF-8".to_string());
                    continue;
                }
            };

            match json::parse(text) {
                Ok(value) => {
                    stats.valid += 1;
                    stats.top_level_types.record(value.type_name());
                    if let Value::Object(fields) = &value {
                        stats.keys.record(fields);
                    }
                    for field in &mut stats.fields {
                        let selected = field.path.select(&value);
                        field.record(&selected);
                    }
                }
                Err(e) => {
                    stats.invalid += 1;
                    push_issue(&mut stats, max_errors, line.number, e.offset + 1, e.kind.to_string());
                }
            }
        }
        stats.bytes = lines.bytes_read();
        Ok(stats)
    }
}

fn push_issue(stats: &mut Stats, max_errors: usize, line: u64, column: usize, reason: String) {
    if stats.issues.len() < max_errors {
        stats.issues.push(Issue { line, column, reason });
    } else {
        stats.issues_truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn stats(input: &str, options: StatsOptions) -> Stats {
        Stats::from_reader(Cursor::new(input.as_bytes()), options).unwrap()
    }

    fn default(input: &str) -> Stats {
        stats(input, StatsOptions::default())
    }

    #[test]
    fn counts_lines_blank_valid_and_invalid() {
        let s = default("{}\n\nnot json\n{\"a\":1}\n");
        assert_eq!(s.lines, 4);
        assert_eq!(s.blank, 1);
        assert_eq!(s.valid, 2);
        assert_eq!(s.invalid, 1);
        assert_eq!(s.bytes, "{}\n\nnot json\n{\"a\":1}\n".len() as u64);
    }

    #[test]
    fn records_top_level_types() {
        let s = default("{}\n[1,2]\n1\n");
        let types: Vec<_> = s.top_level_types.iter().collect();
        assert_eq!(types, vec![("object", 1), ("array", 1), ("int", 1)]);
        assert_eq!(s.top_level_types.total(), 3);
    }

    #[test]
    fn builds_the_top_level_key_table() {
        let s = default("{\"id\":1,\"tags\":[1]}\n{\"id\":2}\n");
        let entries = s.keys.entries();
        assert_eq!(entries[0].key, "id");
        assert_eq!(entries[0].count, 2);
        assert_eq!(entries[0].types.iter().collect::<Vec<_>>(), vec![("int", 2)]);
        assert_eq!(entries[1].key, "tags");
        assert_eq!(entries[1].count, 1);
        assert!(!s.keys.truncated());
    }

    #[test]
    fn key_table_caps_distinct_keys() {
        let mut line = String::from("{");
        for i in 0..(MAX_KEYS + 5) {
            if i > 0 {
                line.push(',');
            }
            line.push_str(&format!("\"k{i}\":1"));
        }
        line.push('}');
        let s = default(&format!("{line}\n"));
        assert_eq!(s.keys.entries().len(), MAX_KEYS);
        assert!(s.keys.truncated());
    }

    #[test]
    fn ignores_duplicate_keys_within_one_record() {
        let s = default("{\"a\":1,\"a\":2}\n");
        assert_eq!(s.keys.entries().len(), 1);
        assert_eq!(s.keys.entries()[0].count, 1);
    }

    #[test]
    fn profiles_a_simple_field() {
        let options = StatsOptions {
            fields: vec![FieldPath::parse("meta.source").unwrap()],
            ..StatsOptions::default()
        };
        let s = stats(
            "{\"meta\":{\"source\":\"web\"}}\n{\"meta\":{\"source\":\"web\"}}\n{\"meta\":{}}\n",
            options,
        );
        let field = &s.fields[0];
        assert_eq!(field.records_present, 2);
        assert_eq!(field.values, 2);
        assert_eq!(field.distinct(), 1);
        assert_eq!(field.top(5), vec![("\"web\"", 2)]);
    }

    #[test]
    fn profiles_a_wildcard_field_across_array_elements() {
        let options = StatsOptions {
            fields: vec![FieldPath::parse("messages[].role").unwrap()],
            ..StatsOptions::default()
        };
        let s = stats(
            r#"{"messages":[{"role":"user"},{"role":"assistant"}]}
{"messages":[{"role":"user"}]}
"#,
            options,
        );
        let field = &s.fields[0];
        assert_eq!(field.records_present, 2);
        assert_eq!(field.values, 3);
        assert_eq!(
            field.top(5),
            vec![("\"user\"", 2), ("\"assistant\"", 1)]
        );
    }

    #[test]
    fn field_value_table_caps_distinct_values() {
        let options = StatsOptions {
            fields: vec![FieldPath::parse("v").unwrap()],
            ..StatsOptions::default()
        };
        let mut input = String::new();
        for i in 0..(MAX_FIELD_VALUES + 5) {
            input.push_str(&format!("{{\"v\":{i}}}\n"));
        }
        let s = stats(&input, options);
        let field = &s.fields[0];
        assert_eq!(field.distinct(), MAX_FIELD_VALUES);
        assert!(field.truncated());
        assert_eq!(field.values, (MAX_FIELD_VALUES + 5) as u64);
    }

    #[test]
    fn records_issues_with_line_and_column() {
        let s = default("{}\n{,}\n[1 2]\n");
        assert_eq!(s.issues.len(), 2);
        assert_eq!(s.issues[0].line, 2);
        assert_eq!(s.issues[0].column, 2);
        assert_eq!(s.issues[1].line, 3);
        assert_eq!(s.issues[1].column, 4);
    }

    #[test]
    fn caps_the_number_of_recorded_issues() {
        let options = StatsOptions {
            max_errors: 1,
            ..StatsOptions::default()
        };
        let s = stats("bad one\nbad two\nbad three\n", options);
        assert_eq!(s.invalid, 3);
        assert_eq!(s.issues.len(), 1);
        assert!(s.issues_truncated);
    }

    #[test]
    fn reports_invalid_utf8_with_its_byte_offset() {
        let mut input = b"{}\n".to_vec();
        input.extend_from_slice(b"ok\xff\n");
        let s = Stats::from_reader(Cursor::new(input), StatsOptions::default()).unwrap();
        assert_eq!(s.invalid, 1);
        assert_eq!(s.issues[0].line, 2);
        assert_eq!(s.issues[0].column, 3);
        assert_eq!(s.issues[0].reason, "invalid UTF-8");
    }

    #[test]
    fn empty_input_yields_zeroed_stats() {
        let s = default("");
        assert_eq!(s.lines, 0);
        assert_eq!(s.valid, 0);
        assert_eq!(s.bytes, 0);
        assert!(s.fields.is_empty());
    }
}
