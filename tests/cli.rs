//! End-to-end tests: run the built binary as a subprocess against a fixture
//! file on disk, the way a user actually invokes it. Unit tests inside the
//! library modules cover the logic; these cover argument parsing, exit codes
//! and the text report format that the unit tests never touch.

use std::path::PathBuf;
use std::process::{Command, Output};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.jsonl")
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jsonl-peek"))
        .args(args)
        .output()
        .expect("failed to run jsonl-peek")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is not valid UTF-8")
}

#[test]
fn head_prints_the_first_n_lines_unchanged() {
    let path = fixture_path();
    let output = run(&["head", "-n", "2", path.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "{\"id\":1,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"meta\":{\"source\":\"web\"}}\n\
         {\"id\":2,\"messages\":[{\"role\":\"user\",\"content\":\"hey\"},{\"role\":\"assistant\",\"content\":\"hello\"}],\"meta\":{\"source\":\"code\"}}\n"
    );
}

#[test]
fn head_defaults_to_ten_lines_and_stops_at_eof() {
    let path = fixture_path();
    let output = run(&["head", path.to_str().unwrap()]);
    assert!(output.status.success());
    // The fixture only has 4 non-blank lines plus one blank one; head counts
    // every line LineReader hands back, blank included, so all 5 come out.
    assert_eq!(stdout(&output).lines().count(), 5);
}

#[test]
fn sample_is_deterministic_for_a_given_seed() {
    let path = fixture_path();
    let args = ["sample", "-n", "2", "--seed", "7", path.to_str().unwrap()];
    let first = stdout(&run(&args));
    let second = stdout(&run(&args));
    assert_eq!(first, second);
    assert_eq!(first.lines().count(), 2);

    let non_blank = [
        "{\"id\":1,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"meta\":{\"source\":\"web\"}}",
        "{\"id\":2,\"messages\":[{\"role\":\"user\",\"content\":\"hey\"},{\"role\":\"assistant\",\"content\":\"hello\"}],\"meta\":{\"source\":\"code\"}}",
        "{\"id\":3,\"messages\":[{\"role\":\"system\",\"content\":\"sys\"}],\"meta\":{\"source\":\"web\"}}",
        "{\"id\":4,}",
    ];
    for line in first.lines() {
        assert!(non_blank.contains(&line), "unexpected sampled line: {line}");
    }
}

#[test]
fn stats_reports_counts_and_field_distributions() {
    let path = fixture_path();
    let output = run(&[
        "stats",
        "--field",
        "meta.source",
        "--field",
        "messages[].role",
        path.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let text = stdout(&output);

    assert!(text.contains("lines   5   blank 1   invalid 1   valid 3"));
    assert!(text.contains("top level  object:3"));

    assert!(text.contains("field meta.source"));
    assert!(text.contains("present in 3 of 3 records (100.0%), 3 values, types string:3"));
    assert!(text.contains("\"web\""));
    assert!(text.contains("\"code\""));

    assert!(text.contains("field messages[].role"));
    assert!(text.contains("present in 3 of 3 records (100.0%), 4 values, types string:4"));

    assert!(text.contains("invalid lines (1 total, showing 1)"));
    assert!(text.contains("line 5 col 9: expected a quoted object key"));
}

#[test]
fn schema_discovers_paths_and_skips_broken_lines() {
    let path = fixture_path();
    let output = run(&["schema", path.to_str().unwrap()]);
    assert!(output.status.success());
    let text = stdout(&output);

    assert!(text.starts_with("3 records, depth 3"));
    assert!(text.contains("2 unparseable lines skipped"));

    // Every fixture record shares the same shape, so every discovered path
    // sits at 100%; look up each row by its leading token rather than
    // hard-coding column widths, which are an implementation detail.
    let find_entry = |name: &str| -> String {
        text.lines()
            .find(|line| line.trim_start().split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("path '{name}' not found in schema output:\n{text}"))
            .to_string()
    };

    let id_line = find_entry("id");
    assert!(id_line.contains("100.0%"));
    assert!(id_line.contains("int:3"));

    let role_line = find_entry("messages[].role");
    assert!(role_line.contains("100.0%"));
    assert!(role_line.contains("string:4"));

    let source_line = find_entry("meta.source");
    assert!(source_line.contains("100.0%"));
    assert!(source_line.contains("string:3"));
}

#[test]
fn reading_from_stdin_matches_reading_from_a_file() {
    let path = fixture_path();
    let from_file = stdout(&run(&["stats", path.to_str().unwrap()]));

    let bytes = std::fs::read(&path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsonl-peek"))
        .args(["stats", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn jsonl-peek");
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let from_stdin = stdout(&output);

    // Both reports name their input on the first line; strip that before
    // comparing so the file path itself doesn't break the match.
    let drop_file_line = |s: &str| -> String {
        s.lines().skip(1).collect::<Vec<_>>().join("\n")
    };
    assert_eq!(drop_file_line(&from_file), drop_file_line(&from_stdin));
}

#[test]
fn missing_file_is_a_runtime_error() {
    let output = run(&["stats", "/no/such/path/jsonl-peek-fixture-missing.jsonl"]);
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let output = run(&["frobnicate"]);
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown subcommand 'frobnicate'"));
}
