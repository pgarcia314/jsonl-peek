//! Schema discovery: which field paths a JSONL file actually contains, up to
//! a depth limit, and how often each one shows up.
//!
//! Unlike [`crate::stats`], which profiles paths the caller already knows to
//! ask for, `schema` walks every record itself and discovers the paths. Object
//! keys become dotted segments and arrays become a `[]` wildcard segment, the
//! same syntax [`crate::path::FieldPath`] parses, so a path printed by `schema`
//! can be pasted straight into `stats --field`.
//!
//! ```
//! use jsonl_peek::{Schema, SchemaOptions};
//! use std::io::Cursor;
//!
//! let data = b"{\"meta\":{\"source\":\"web\"}}\n{\"meta\":{\"source\":\"web\"}}\n";
//! let schema = Schema::from_reader(Cursor::new(&data[..]), SchemaOptions::default()).unwrap();
//! assert_eq!(schema.records, 2);
//! let path = schema.paths.iter().find(|p| p.path == "meta.source").unwrap();
//! assert_eq!(path.records_present, 2);
//! assert_eq!(path.rate(schema.records), 1.0);
//! ```

use std::collections::HashMap;
use std::io::{self, BufRead};

use crate::json::{self, Value};
use crate::lines::LineReader;
use crate::stats::TypeCounts;

/// Default number of segments [`Schema::from_reader`] descends into a record.
pub const DEFAULT_DEPTH: usize = 3;

/// The path table stops growing past this many distinct paths.
pub const MAX_PATHS: usize = 2_000;

/// Options controlling a [`Schema::from_reader`] pass.
#[derive(Debug, Clone)]
pub struct SchemaOptions {
    /// Maximum number of path segments to descend into each record.
    pub depth: usize,
    /// Paths present in fewer than this share of records (`0.0..=1.0`) are
    /// left out of the result.
    pub min_rate: f64,
}

impl Default for SchemaOptions {
    fn default() -> Self {
        SchemaOptions {
            depth: DEFAULT_DEPTH,
            min_rate: 0.0,
        }
    }
}

/// One discovered path: how often it appeared and with which value types.
#[derive(Debug, Clone)]
pub struct PathEntry {
    /// The path, in the same dotted/bracketed syntax [`crate::path::FieldPath`]
    /// parses (`meta.source`, `messages[].role`, `[].id`, ...).
    pub path: String,
    /// Number of records in which the path selected at least one value.
    pub records_present: u64,
    /// Total number of values matched (more than one per record for a path
    /// that passes through a `[]` wildcard).
    pub values: u64,
    /// Types the matched values took.
    pub types: TypeCounts,
}

impl PathEntry {
    /// Share of `records` in which this path was present, in `0.0..=1.0`.
    pub fn rate(&self, records: u64) -> f64 {
        if records == 0 {
            0.0
        } else {
            self.records_present as f64 / records as f64
        }
    }
}

/// The result of a [`Schema::from_reader`] pass over a JSONL file.
#[derive(Debug, Clone)]
pub struct Schema {
    /// Number of lines that parsed as valid JSON.
    pub records: u64,
    /// The depth the pass was run with.
    pub depth: usize,
    /// Discovered paths, sorted alphabetically.
    pub paths: Vec<PathEntry>,
    /// True once the number of distinct paths reached [`MAX_PATHS`].
    pub truncated: bool,
    /// Lines that were blank, not valid UTF-8 or not valid JSON.
    pub skipped: u64,
}

impl Schema {
    /// Discovers paths and their rates in a single pass over a JSONL stream.
    pub fn from_reader<R: BufRead>(reader: R, options: SchemaOptions) -> io::Result<Schema> {
        let depth = options.depth;
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut paths: Vec<PathEntry> = Vec::new();
        let mut truncated = false;
        let mut records = 0u64;
        let mut skipped = 0u64;
        let mut occurrences: Vec<(String, &'static str)> = Vec::new();

        let mut lines = LineReader::new(reader);
        while let Some(line) = lines.next_line()? {
            if line.is_blank() {
                skipped += 1;
                continue;
            }
            let value = match std::str::from_utf8(line.bytes).ok().and_then(|text| json::parse(text).ok()) {
                Some(value) => value,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            records += 1;

            occurrences.clear();
            walk(&value, "", depth, &mut occurrences);

            let mut seen_in_record: Vec<&str> = Vec::new();
            for (path, type_name) in &occurrences {
                let first_in_record = !seen_in_record.contains(&path.as_str());
                if first_in_record {
                    seen_in_record.push(path.as_str());
                }

                let idx = match index.get(path.as_str()) {
                    Some(&idx) => idx,
                    None => {
                        if paths.len() >= MAX_PATHS {
                            truncated = true;
                            continue;
                        }
                        let idx = paths.len();
                        index.insert(path.clone(), idx);
                        paths.push(PathEntry {
                            path: path.clone(),
                            records_present: 0,
                            values: 0,
                            types: TypeCounts::default(),
                        });
                        idx
                    }
                };

                let entry = &mut paths[idx];
                entry.values += 1;
                entry.types.record(*type_name);
                if first_in_record {
                    entry.records_present += 1;
                }
            }
        }

        if options.min_rate > 0.0 {
            paths.retain(|p| p.rate(records) >= options.min_rate);
        }
        paths.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(Schema {
            records,
            depth,
            paths,
            truncated,
            skipped,
        })
    }
}

/// Appends `(path, type)` for every location reachable from `value` within
/// `remaining` segments, using the same object-key/array-wildcard syntax
/// [`crate::path::FieldPath`] parses.
///
/// A record can repeat an object key (the parser keeps duplicates), so only
/// the first occurrence per object contributes a path, matching how
/// [`crate::stats::KeyTable`] treats the top level.
fn walk(value: &Value, prefix: &str, remaining: usize, out: &mut Vec<(String, &'static str)>) {
    if remaining == 0 {
        return;
    }
    match value {
        Value::Object(fields) => {
            let mut seen: Vec<&str> = Vec::new();
            for (key, child) in fields {
                if seen.contains(&key.as_str()) {
                    continue;
                }
                seen.push(key.as_str());
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                out.push((path.clone(), child.type_name()));
                walk(child, &path, remaining - 1, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            for item in items {
                out.push((path.clone(), item.type_name()));
                walk(item, &path, remaining - 1, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn schema(input: &str, options: SchemaOptions) -> Schema {
        Schema::from_reader(Cursor::new(input.as_bytes()), options).unwrap()
    }

    fn default(input: &str) -> Schema {
        schema(input, SchemaOptions::default())
    }

    fn find<'a>(schema: &'a Schema, path: &str) -> &'a PathEntry {
        schema.paths.iter().find(|p| p.path == path).unwrap_or_else(|| panic!("path {path} not found"))
    }

    #[test]
    fn discovers_top_level_keys() {
        let s = schema(
            "{\"id\":1,\"tags\":[1,2]}\n",
            SchemaOptions { depth: 1, ..SchemaOptions::default() },
        );
        assert_eq!(s.records, 1);
        let names: Vec<&str> = s.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names, vec!["id", "tags"]);
    }

    #[test]
    fn descends_into_nested_objects_and_arrays() {
        let s = default(r#"{"messages":[{"role":"user"},{"role":"assistant"}]}"#);
        let names: Vec<&str> = s.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names, vec!["messages", "messages[]", "messages[].role"]);

        let role = find(&s, "messages[].role");
        assert_eq!(role.records_present, 1);
        assert_eq!(role.values, 2);
        assert_eq!(role.types.iter().collect::<Vec<_>>(), vec![("string", 2)]);
    }

    #[test]
    fn respects_the_depth_limit() {
        let input = r#"{"messages":[{"role":"user"}]}"#;

        let s1 = schema(input, SchemaOptions { depth: 1, ..SchemaOptions::default() });
        let names1: Vec<&str> = s1.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names1, vec!["messages"]);

        let s2 = schema(input, SchemaOptions { depth: 2, ..SchemaOptions::default() });
        let names2: Vec<&str> = s2.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names2, vec!["messages", "messages[]"]);

        let s3 = schema(input, SchemaOptions { depth: 3, ..SchemaOptions::default() });
        let names3: Vec<&str> = s3.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names3, vec!["messages", "messages[]", "messages[].role"]);
    }

    #[test]
    fn a_leading_wildcard_covers_a_top_level_array() {
        let s = default("[{\"id\":1},{\"id\":2}]\n");
        let names: Vec<&str> = s.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names, vec!["[]", "[].id"]);

        let id = find(&s, "[].id");
        assert_eq!(id.records_present, 1);
        assert_eq!(id.values, 2);
    }

    #[test]
    fn dedupes_duplicate_keys_within_one_record() {
        let s = default("{\"a\":1,\"a\":2}\n");
        assert_eq!(s.paths.len(), 1);
        assert_eq!(find(&s, "a").values, 1);
    }

    #[test]
    fn filters_by_min_rate() {
        let input = "{\"meta\":{\"source\":\"web\",\"extra\":1}}\n{\"meta\":{\"source\":\"web\"}}\n{\"meta\":{}}\n";
        let s = schema(
            input,
            SchemaOptions { min_rate: 0.5, ..SchemaOptions::default() },
        );
        let names: Vec<&str> = s.paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(names, vec!["meta", "meta.source"]);
    }

    #[test]
    fn caps_the_number_of_distinct_paths() {
        let mut line = String::from("{");
        for i in 0..(MAX_PATHS + 5) {
            if i > 0 {
                line.push(',');
            }
            line.push_str(&format!("\"k{i}\":1"));
        }
        line.push('}');
        let s = default(&format!("{line}\n"));
        assert_eq!(s.paths.len(), MAX_PATHS);
        assert!(s.truncated);
    }

    #[test]
    fn skips_blank_and_unparseable_lines() {
        let s = default("{}\n\nnot json\n{\"a\":1}\n");
        assert_eq!(s.records, 2);
        assert_eq!(s.skipped, 2);
    }

    #[test]
    fn rate_is_relative_to_all_parsed_records() {
        let s = default("{\"a\":1}\n{\"a\":1}\n{\"b\":1}\n");
        assert_eq!(find(&s, "a").rate(s.records), 2.0 / 3.0);
        assert_eq!(find(&s, "b").rate(s.records), 1.0 / 3.0);
    }

    #[test]
    fn empty_input_yields_no_paths() {
        let s = default("");
        assert_eq!(s.records, 0);
        assert!(s.paths.is_empty());
        assert!(!s.truncated);
    }
}
