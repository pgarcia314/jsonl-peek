//! CLI for jsonl-peek. A thin shell over the library: parses arguments, opens
//! the input (a file, or stdin when none is given or it is `-`), runs one of
//! the library passes and prints the result as plain text or, with `--json`,
//! as a single line of compact JSON.

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonl_peek::hist::Histogram;
use jsonl_peek::json::{self, Value};
use jsonl_peek::lines::LineReader;
use jsonl_peek::path::FieldPath;
use jsonl_peek::rng::Reservoir;
use jsonl_peek::schema::{self, PathEntry, Schema, SchemaOptions};
use jsonl_peek::stats::{self, FieldStats, Issue, KeyEntry, Stats, StatsOptions, TypeCounts};

const USAGE: &str = "\
usage: jsonl-peek head   [-n N] [FILE]
       jsonl-peek sample [-n N] [--seed S] [FILE]
       jsonl-peek stats  [--field PATH]... [--top N] [--max-errors N] [--json] [FILE]
       jsonl-peek schema [--depth N] [--min-rate R] [--json] [FILE]";

enum CliError {
    Usage(String),
    Runtime(String),
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> Self {
        CliError::Runtime(e.to_string())
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            eprintln!("jsonl-peek: {msg}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(CliError::Runtime(msg)) => {
            eprintln!("jsonl-peek: {msg}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<(), CliError> {
    let (cmd, rest) = args
        .split_first()
        .ok_or_else(|| CliError::Usage("expected a subcommand".to_string()))?;
    match cmd.as_str() {
        "head" => cmd_head(rest),
        "sample" => cmd_sample(rest),
        "stats" => cmd_stats(rest),
        "schema" => cmd_schema(rest),
        other => Err(CliError::Usage(format!("unknown subcommand '{other}'"))),
    }
}

fn open_input(path: Option<&str>) -> Result<(Box<dyn BufRead>, String), CliError> {
    match path {
        Some(p) if p != "-" => {
            let file = File::open(p).map_err(|e| CliError::Runtime(format!("{p}: {e}")))?;
            Ok((Box::new(BufReader::new(file)), p.to_string()))
        }
        _ => Ok((Box::new(BufReader::new(io::stdin())), "-".to_string())),
    }
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, CliError> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| CliError::Usage(format!("'{flag}' needs a value")))
}

fn take_positional(file: &mut Option<String>, arg: &str) -> Result<(), CliError> {
    if file.is_some() {
        return Err(CliError::Usage(format!("unexpected argument '{arg}'")));
    }
    *file = Some(arg.to_string());
    Ok(())
}

fn parse_usize(flag: &str, raw: &str) -> Result<usize, CliError> {
    raw.parse()
        .map_err(|_| CliError::Usage(format!("'{flag}' expects a non-negative integer, got '{raw}'")))
}

fn parse_u64(flag: &str, raw: &str) -> Result<u64, CliError> {
    raw.parse()
        .map_err(|_| CliError::Usage(format!("'{flag}' expects a non-negative integer, got '{raw}'")))
}

fn parse_f64(flag: &str, raw: &str) -> Result<f64, CliError> {
    raw.parse()
        .map_err(|_| CliError::Usage(format!("'{flag}' expects a number, got '{raw}'")))
}

fn default_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn cmd_head(args: &[String]) -> Result<(), CliError> {
    let mut n: usize = 10;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => n = parse_usize("-n", &take_value(args, &mut i, "-n")?)?,
            other => take_positional(&mut file, other)?,
        }
        i += 1;
    }

    let (input, _) = open_input(file.as_deref())?;
    let mut lines = LineReader::new(input);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut printed = 0;
    while printed < n {
        let Some(line) = lines.next_line()? else {
            break;
        };
        out.write_all(line.bytes)?;
        out.write_all(b"\n")?;
        printed += 1;
    }
    Ok(())
}

fn cmd_sample(args: &[String]) -> Result<(), CliError> {
    let mut n: usize = 10;
    let mut seed: Option<u64> = None;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => n = parse_usize("-n", &take_value(args, &mut i, "-n")?)?,
            "--seed" => seed = Some(parse_u64("--seed", &take_value(args, &mut i, "--seed")?)?),
            other => take_positional(&mut file, other)?,
        }
        i += 1;
    }

    let (input, _) = open_input(file.as_deref())?;
    let mut lines = LineReader::new(input);
    let mut reservoir = Reservoir::new(n, seed.unwrap_or_else(default_seed));
    while let Some(line) = lines.next_line()? {
        if line.is_blank() {
            continue;
        }
        reservoir.offer(line.number, || line.bytes.to_vec());
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (_, bytes) in reservoir.into_sorted() {
        out.write_all(&bytes)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn cmd_stats(args: &[String]) -> Result<(), CliError> {
    let mut fields: Vec<FieldPath> = Vec::new();
    let mut top: usize = 10;
    let mut max_errors: usize = stats::DEFAULT_MAX_ERRORS;
    let mut json_out = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--field" => {
                let raw = take_value(args, &mut i, "--field")?;
                let path = FieldPath::parse(&raw)
                    .map_err(|e| CliError::Usage(format!("--field {raw}: {e}")))?;
                fields.push(path);
            }
            "--top" => top = parse_usize("--top", &take_value(args, &mut i, "--top")?)?,
            "--max-errors" => {
                max_errors = parse_usize("--max-errors", &take_value(args, &mut i, "--max-errors")?)?
            }
            "--json" => json_out = true,
            other => take_positional(&mut file, other)?,
        }
        i += 1;
    }

    let (input, label) = open_input(file.as_deref())?;
    let stats = Stats::from_reader(input, StatsOptions { fields, max_errors })?;

    if json_out {
        println!("{}", stats_to_json(&stats, top).to_json());
    } else {
        print_stats(&label, &stats, top);
    }
    Ok(())
}

fn cmd_schema(args: &[String]) -> Result<(), CliError> {
    let mut depth = schema::DEFAULT_DEPTH;
    let mut min_rate: f64 = 0.0;
    let mut json_out = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--depth" => depth = parse_usize("--depth", &take_value(args, &mut i, "--depth")?)?,
            "--min-rate" => {
                min_rate = parse_f64("--min-rate", &take_value(args, &mut i, "--min-rate")?)?
            }
            "--json" => json_out = true,
            other => take_positional(&mut file, other)?,
        }
        i += 1;
    }

    let (input, _) = open_input(file.as_deref())?;
    let schema = Schema::from_reader(input, SchemaOptions { depth, min_rate })?;

    if json_out {
        println!("{}", schema_to_json(&schema).to_json());
    } else {
        print_schema(&schema);
    }
    Ok(())
}

fn grouped(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn percent(part: u64, total: u64) -> String {
    if total == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / total as f64)
    }
}

fn format_types(types: &TypeCounts) -> String {
    types
        .iter()
        .map(|(name, count)| format!("{name}:{}", grouped(count)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_stats(label: &str, stats: &Stats, top: usize) {
    println!("file    {label}");
    println!(
        "lines   {}   blank {}   invalid {}   valid {}",
        grouped(stats.lines),
        grouped(stats.blank),
        grouped(stats.invalid),
        grouped(stats.valid),
    );
    println!("bytes   {}", grouped(stats.bytes));
    println!("top level  {}", format_types(&stats.top_level_types));
    println!();

    println!("line length in bytes");
    if stats.line_length.count() > 0 {
        println!(
            "  min {}   p50 {}   p90 {}   p99 {}   max {}   mean {:.1}",
            stats.line_length.min().unwrap(),
            stats.line_length.quantile(0.5).unwrap(),
            stats.line_length.quantile(0.9).unwrap(),
            stats.line_length.quantile(0.99).unwrap(),
            stats.line_length.max().unwrap(),
            stats.line_length.mean().unwrap(),
        );
    } else {
        println!("  no non-blank lines");
    }

    if !stats.keys.entries().is_empty() {
        println!();
        println!("top level keys over {} objects", grouped(stats.valid));
        println!("  {:<28} {:>8}  {:>6}  types", "key", "count", "rate");
        for entry in stats.keys.entries() {
            print_key_entry(entry, stats.valid);
        }
        if stats.keys.truncated() {
            println!("  ... key table truncated at {} distinct keys", stats::MAX_KEYS);
        }
    }

    for field in &stats.fields {
        println!();
        println!("field {}", field.path);
        println!(
            "  present in {} of {} records ({}), {} values, types {}",
            grouped(field.records_present),
            grouped(stats.valid),
            percent(field.records_present, stats.valid),
            grouped(field.values),
            format_types(&field.types),
        );
        println!("  {} distinct values", grouped(field.distinct() as u64));
        for (value, count) in field.top(top) {
            println!("  {:>8}  {:>6}  {value}", grouped(count), percent(count, field.values));
        }
        if field.truncated() {
            println!("  ... value table truncated at {} distinct values", stats::MAX_FIELD_VALUES);
        }
    }

    if !stats.issues.is_empty() {
        println!();
        println!(
            "invalid lines ({} total, showing {})",
            grouped(stats.invalid),
            stats.issues.len()
        );
        for issue in &stats.issues {
            print_issue(issue);
        }
        if stats.issues_truncated {
            println!("  ... {} more not shown", grouped(stats.invalid - stats.issues.len() as u64));
        }
    }
}

fn print_key_entry(entry: &KeyEntry, valid: u64) {
    println!(
        "  {:<28} {:>8}  {:>6}  {}",
        entry.key,
        grouped(entry.count),
        percent(entry.count, valid),
        format_types(&entry.types),
    );
}

fn print_issue(issue: &Issue) {
    println!("  line {} col {}: {}", issue.line, issue.column, issue.reason);
}

fn print_schema(schema: &Schema) {
    println!("{} records, depth {}", grouped(schema.records), schema.depth);
    println!();
    println!("  {:<40} {:>7}  types", "path", "rate");
    for entry in &schema.paths {
        println!(
            "  {:<40} {:>7}  {}",
            entry.path,
            percent(entry.records_present, schema.records),
            format_types(&entry.types),
        );
    }
    if schema.truncated {
        println!("  ... path table truncated at {} distinct paths", schema::MAX_PATHS);
    }

    if schema.skipped > 0 {
        println!();
        println!("{} unparseable lines skipped", grouped(schema.skipped));
    }
}

fn stats_to_json(stats: &Stats, top: usize) -> Value {
    Value::Object(vec![
        ("lines".to_string(), Value::Int(stats.lines as i64)),
        ("blank".to_string(), Value::Int(stats.blank as i64)),
        ("valid".to_string(), Value::Int(stats.valid as i64)),
        ("invalid".to_string(), Value::Int(stats.invalid as i64)),
        ("bytes".to_string(), Value::Int(stats.bytes as i64)),
        ("top_level_types".to_string(), type_counts_to_json(&stats.top_level_types)),
        ("line_length".to_string(), line_length_to_json(&stats.line_length)),
        (
            "keys".to_string(),
            Value::Array(stats.keys.entries().iter().map(key_entry_to_json).collect()),
        ),
        (
            "fields".to_string(),
            Value::Array(stats.fields.iter().map(|f| field_stats_to_json(f, top)).collect()),
        ),
        (
            "issues".to_string(),
            Value::Array(stats.issues.iter().map(issue_to_json).collect()),
        ),
        ("issues_truncated".to_string(), Value::Bool(stats.issues_truncated)),
    ])
}

fn line_length_to_json(h: &Histogram) -> Value {
    if h.count() == 0 {
        return Value::Null;
    }
    Value::Object(vec![
        ("min".to_string(), Value::Int(h.min().unwrap() as i64)),
        ("p50".to_string(), Value::Int(h.quantile(0.5).unwrap() as i64)),
        ("p90".to_string(), Value::Int(h.quantile(0.9).unwrap() as i64)),
        ("p99".to_string(), Value::Int(h.quantile(0.99).unwrap() as i64)),
        ("max".to_string(), Value::Int(h.max().unwrap() as i64)),
        ("mean".to_string(), Value::Float(h.mean().unwrap())),
    ])
}

fn type_counts_to_json(types: &TypeCounts) -> Value {
    Value::Object(
        types
            .iter()
            .map(|(name, count)| (name.to_string(), Value::Int(count as i64)))
            .collect(),
    )
}

fn key_entry_to_json(entry: &KeyEntry) -> Value {
    Value::Object(vec![
        ("key".to_string(), Value::Str(entry.key.clone())),
        ("count".to_string(), Value::Int(entry.count as i64)),
        ("types".to_string(), type_counts_to_json(&entry.types)),
    ])
}

fn field_stats_to_json(field: &FieldStats, top: usize) -> Value {
    Value::Object(vec![
        ("path".to_string(), Value::Str(field.path.as_str().to_string())),
        ("records_present".to_string(), Value::Int(field.records_present as i64)),
        ("values".to_string(), Value::Int(field.values as i64)),
        ("types".to_string(), type_counts_to_json(&field.types)),
        ("distinct".to_string(), Value::Int(field.distinct() as i64)),
        ("truncated".to_string(), Value::Bool(field.truncated())),
        (
            "top".to_string(),
            Value::Array(
                field
                    .top(top)
                    .into_iter()
                    .map(|(raw, count)| {
                        let value = json::parse(raw).unwrap_or_else(|_| Value::Str(raw.to_string()));
                        Value::Object(vec![
                            ("value".to_string(), value),
                            ("count".to_string(), Value::Int(count as i64)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn issue_to_json(issue: &Issue) -> Value {
    Value::Object(vec![
        ("line".to_string(), Value::Int(issue.line as i64)),
        ("column".to_string(), Value::Int(issue.column as i64)),
        ("reason".to_string(), Value::Str(issue.reason.clone())),
    ])
}

fn schema_to_json(schema: &Schema) -> Value {
    Value::Object(vec![
        ("records".to_string(), Value::Int(schema.records as i64)),
        ("depth".to_string(), Value::Int(schema.depth as i64)),
        ("truncated".to_string(), Value::Bool(schema.truncated)),
        ("skipped".to_string(), Value::Int(schema.skipped as i64)),
        (
            "paths".to_string(),
            Value::Array(
                schema
                    .paths
                    .iter()
                    .map(|p| path_entry_to_json(p, schema.records))
                    .collect(),
            ),
        ),
    ])
}

fn path_entry_to_json(entry: &PathEntry, records: u64) -> Value {
    Value::Object(vec![
        ("path".to_string(), Value::Str(entry.path.clone())),
        ("records_present".to_string(), Value::Int(entry.records_present as i64)),
        ("values".to_string(), Value::Int(entry.values as i64)),
        ("rate".to_string(), Value::Float(entry.rate(records))),
        ("types".to_string(), type_counts_to_json(&entry.types)),
    ])
}
