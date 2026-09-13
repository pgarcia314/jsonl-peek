//! Generates random JSON documents, runs schema discovery over them, and
//! checks two things schema.rs promises but nothing previously verified:
//! that every path it prints is one `FieldPath::parse` accepts, and that
//! selecting that path reproduces exactly the counts schema recorded. The
//! two are independent implementations walking the same tree, so it is easy
//! for them to quietly drift apart on some path shape neither hand-written
//! test happens to cover.

use std::collections::HashMap;
use std::io::Cursor;

use jsonl_peek::json::Value;
use jsonl_peek::rng::SplitMix64;
use jsonl_peek::{FieldPath, Schema, SchemaOptions};

const WORDS: [&str; 5] = ["user", "assistant", "system", "web", "code"];
const KEYS: [&str; 6] = ["id", "meta", "role", "content", "tags", "source"];
const MAX_GEN_DEPTH: usize = 4;

fn gen_string(rng: &mut SplitMix64) -> String {
    WORDS[rng.below(WORDS.len() as u64) as usize].to_string()
}

fn gen_scalar(rng: &mut SplitMix64) -> Value {
    match rng.below(5) {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => Value::Int(rng.below(1000) as i64 - 500),
        3 => Value::Float(rng.below(1000) as f64 / 7.0),
        _ => Value::Str(gen_string(rng)),
    }
}

fn gen_array(rng: &mut SplitMix64, depth: usize) -> Value {
    let len = rng.below(4) as usize;
    let items = (0..len).map(|_| gen_value(rng, depth - 1)).collect();
    Value::Array(items)
}

fn gen_object(rng: &mut SplitMix64, depth: usize) -> Value {
    let mut fields = Vec::new();
    for key in KEYS {
        if rng.below(2) == 0 {
            fields.push((key.to_string(), gen_value(rng, depth - 1)));
        }
    }
    Value::Object(fields)
}

fn gen_value(rng: &mut SplitMix64, depth: usize) -> Value {
    if depth == 0 {
        return gen_scalar(rng);
    }
    match rng.below(7) {
        5 => gen_array(rng, depth),
        6 => gen_object(rng, depth),
        _ => gen_scalar(rng),
    }
}

fn gen_document(rng: &mut SplitMix64) -> Value {
    if rng.below(5) == 0 {
        gen_array(rng, MAX_GEN_DEPTH)
    } else {
        gen_object(rng, MAX_GEN_DEPTH)
    }
}

fn check_seed(seed: u64) {
    let mut rng = SplitMix64::new(seed);
    let documents: Vec<Value> = (0..40).map(|_| gen_document(&mut rng)).collect();

    let mut input = String::new();
    for doc in &documents {
        input.push_str(&doc.to_json());
        input.push('\n');
    }

    let schema = Schema::from_reader(Cursor::new(input.as_bytes()), SchemaOptions::default())
        .expect("reading from an in-memory buffer cannot fail");
    assert_eq!(
        schema.records,
        documents.len() as u64,
        "seed {seed}: every generated document is valid JSON, none should be skipped"
    );

    for entry in &schema.paths {
        let path = FieldPath::parse(&entry.path).unwrap_or_else(|e| {
            panic!("seed {seed}: schema printed '{}', which FieldPath::parse rejects: {e}", entry.path)
        });

        let mut records_present = 0u64;
        let mut values = 0u64;
        let mut types: HashMap<&'static str, u64> = HashMap::new();
        for doc in &documents {
            let selected = path.select(doc);
            if !selected.is_empty() {
                records_present += 1;
            }
            for value in selected {
                values += 1;
                *types.entry(value.type_name()).or_insert(0) += 1;
            }
        }

        assert_eq!(
            records_present, entry.records_present,
            "seed {seed}: records_present mismatch for '{}'",
            entry.path
        );
        assert_eq!(values, entry.values, "seed {seed}: value count mismatch for '{}'", entry.path);
        let schema_types: HashMap<&'static str, u64> = entry.types.iter().collect();
        assert_eq!(types, schema_types, "seed {seed}: type counts mismatch for '{}'", entry.path);
    }
}

#[test]
fn field_path_reads_back_every_path_schema_discovers() {
    for seed in [1u64, 2, 3, 42, 12345] {
        check_seed(seed);
    }
}
