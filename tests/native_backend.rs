use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Examples that should compile and run successfully with the native Cranelift backend.
/// We exclude examples that require features not yet supported natively:
///   - processes (spawn, send, receive)
///   - maps
///   - binary string literals in certain positions
///   - closures/lambdas as values
const NATIVE_EXAMPLES: &[(&str, &[&str])] = &[
    // Basic arithmetic and recursion
    ("fib.lux", &["55"]),
    ("hello.lux", &["120"]),
    ("math_lib.lux", &["25", "27", "120"]),
    // Pattern matching and guards
    ("guards.lux", &[":negative", ":zero", ":positive", ":large"]),
    ("destructure.lux", &["20", "10", "6", "6"]),
    ("enums.lux", &["42", "99", ":true", ":false"]),
    ("listpat.lux", &["15", "{10, 20}", "{42, 0}", "{0, 0}"]),
    ("option.lux", &["52"]),
    // Bitwise operations
    ("bitwise.lux", &["1"]),
    // Closures and list comprehensions
    ("listcomp.lux", &["[2, 4, 6, 8, 10]"]),
    ("range.lux", &["[1, 2, 3, 4]"]),
    // Pipe operator and list/string stdlib
    ("pipe.lux", &["10"]),
    ("stdlib.lux", &["6"]),
    // Structs / maps
    ("structs.lux", &["3"]),
    ("use_example.lux", &["744"]),
    // Strings
    (
        "cond.lux",
        &[
            "\"negative\"",
            "\"zero\"",
            "\"small\"",
            "\"medium\"",
            "\"large\"",
        ],
    ),
    ("concat.lux", &["\"Hello World\""]),
    ("unless.lux", &["\"x is not greater than 10\""]),
    // Closures and misc
    ("counter.lux", &["42"]),
    // Type introspection
    ("debug.lux", &["\"Assert passed!\""]),
    // Try/catch
    ("trycatch.lux", &["5", "\"Division error!\"", "0"]),
    // Types
    ("types.lux", &[":true"]),
    // Maps
    ("mapfuncs.lux", &[]), // just verify it doesn't crash
    ("maps.lux", &[]),
    // Print
    ("print_test.lux", &["42"]),
    // Char pattern matching
    (
        "charpattern.lux",
        &[
            "\"letter a\"",
            "\"letter b\"",
            "\"uppercase A\"",
            "\"digit zero\"",
            "\"newline\"",
            "\"other\"",
            ":true",
            ":false",
        ],
    ),
    // String stdlib
    (
        "strings.lux",
        &[
            "17",
            "\"Hello, World!\"",
            "\"HELLO\"",
            "\"hello\"",
            "\"hello lux\"",
            ":true",
            ":false",
            "[\"a\", \"b\", \"c\"]",
            "\"x-y-z\"",
            "[1, 2, 3]",
            "[3, 4, 5]",
            "3",
            "[{1, \"a\"}, {2, \"b\"}, {3, \"c\"}]",
            "[{1, \"x\"}, {2, \"y\"}, {3, \"z\"}]",
            ":true",
            ":false",
            "[1, 2, 3]",
        ],
    ),
    // Binary construction and operations
    ("binary.lux", &["\"A\"", "1"]),
    // String interpolation
    ("interp.lux", &["\"Name: World, Age: 42\""]),
    // Native feature tests: closures, deep equality, floats
    (
        "native_features.lux",
        &[
            "21",
            "25",
            "[2, 4, 6, 8, 10]",
            ":true",
            ":false",
            ":true",
            ":false",
            ":true",
            ":true",
            ":false",
        ],
    ),
];

fn compile_native(example: &str, artifacts_dir: &Path) -> std::path::PathBuf {
    let example_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(example);
    let stem = Path::new(example).file_stem().unwrap().to_str().unwrap();
    let output_path = artifacts_dir.join(stem);

    let result = Command::new(env!("CARGO_BIN_EXE_lux"))
        .env("LUX_HOME", artifacts_dir.parent().unwrap())
        .arg("--native")
        .arg(&example_path)
        .output()
        .expect("lux binary should start");

    assert!(
        result.status.success(),
        "native compilation of {} failed:\nstdout: {}\nstderr: {}",
        example,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );

    assert!(
        output_path.exists(),
        "native executable should exist at {}",
        output_path.display()
    );

    output_path
}

fn run_native(executable: &Path) -> String {
    let result = Command::new(executable)
        .output()
        .expect("native executable should start");

    assert!(
        result.status.success(),
        "native executable {} failed with:\nstdout: {}\nstderr: {}",
        executable.display(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );

    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

#[test]
fn native_backend_compile_and_run() {
    let temp_dir = TempDir::new().expect("temp dir");
    let artifacts_dir = temp_dir.path().join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).expect("create artifacts dir");

    for &(example, expected_lines) in NATIVE_EXAMPLES {
        let executable = compile_native(example, &artifacts_dir);
        let output = run_native(&executable);

        let output_lines: Vec<&str> = output.lines().collect();

        // Check that each expected line appears in order at the start of the output
        for (i, &expected) in expected_lines.iter().enumerate() {
            assert!(
                i < output_lines.len(),
                "{}: expected at least {} output lines, got {}\nfull output:\n{}",
                example,
                i + 1,
                output_lines.len(),
                output,
            );
            assert_eq!(
                output_lines[i], expected,
                "{}: line {} mismatch\nexpected: {}\ngot:      {}\nfull output:\n{}",
                example, i, expected, output_lines[i], output,
            );
        }
    }
}

#[test]
fn native_fib_correct_result() {
    let temp_dir = TempDir::new().expect("temp dir");
    let artifacts_dir = temp_dir.path().join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).expect("create artifacts dir");

    let executable = compile_native("fib.lux", &artifacts_dir);
    let output = run_native(&executable);
    assert_eq!(output.trim(), "55", "fib(10) should be 55");
}
