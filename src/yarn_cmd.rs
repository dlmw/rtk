use crate::parser::truncate_output;
use crate::parser::types::{TestFailure, TestResult};
use crate::parser::TokenFormatter;
use crate::tracking;
use crate::utils::{strip_ansi, truncate};
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::ffi::OsString;
use std::process::Command;

lazy_static! {
    // --- Jest regexes ---

    /// Matches Jest "Test Suites:" summary line.
    /// e.g. "Test Suites:  1 failed, 2 passed, 3 total"
    static ref TEST_SUITES_RE: Regex =
        Regex::new(r"Test Suites:\s+(?:(\d+)\s+failed,\s*)?(?:(\d+)\s+passed,\s*)?(\d+)\s+total")
            .expect("invalid test suites regex");
    /// Matches Jest "Tests:" summary line.
    /// e.g. "Tests:  1 failed, 2 skipped, 5 passed, 8 total"
    static ref TESTS_RE: Regex =
        Regex::new(r"Tests:\s+(?:(\d+)\s+failed,\s*)?(?:(\d+)\s+(?:skipped|pending|todo),\s*)?(?:(\d+)\s+passed,\s*)?(\d+)\s+total")
            .expect("invalid tests regex");
    /// Matches Jest "Time:" line.
    /// e.g. "Time:  3.456 s"
    static ref TIME_RE: Regex =
        Regex::new(r"Time:\s+(.+)")
            .expect("invalid time regex");
    /// Matches suite result lines (PASS/FAIL prefix).
    /// e.g. " PASS  src/utils.test.js"
    static ref SUITE_RESULT_RE: Regex =
        Regex::new(r"^\s*(PASS|FAIL)\s+(.+)$")
            .expect("invalid suite result regex");

    // --- Vitest regexes ---

    /// Matches Vitest "Tests" summary line.
    /// e.g. "Tests  3 failed | 106 passed (109)" or "Tests  109 passed (109)"
    static ref VITEST_TESTS_RE: Regex =
        Regex::new(r"Tests\s+(?:(\d+)\s+failed\s*\|\s*)?(?:(\d+)\s+skipped\s*\|\s*)?(\d+)\s+passed\s*\((\d+)\)")
            .expect("invalid vitest tests regex");
    /// Matches Vitest "Duration" line.
    /// e.g. "Duration  8.45s (transform 1.23s, ...)" or "Duration  450ms"
    static ref VITEST_DURATION_RE: Regex =
        Regex::new(r"Duration\s+([\d.]+)(ms|s)")
            .expect("invalid vitest duration regex");
}

/// Detected test runner from output.
#[derive(Debug, PartialEq)]
enum TestRunner {
    Jest,
    Vitest,
    Unknown,
}

/// Detect which test runner produced the output.
fn detect_runner(output: &str) -> TestRunner {
    // Jest: "Test Suites:" summary or PASS/FAIL suite prefix on any line
    if output.contains("Test Suites:") || output.lines().any(|l| SUITE_RESULT_RE.is_match(l.trim()))
    {
        return TestRunner::Jest;
    }
    // Vitest: "Test Files" + "Duration" in summary block
    if output.contains("Test Files") && output.contains("Duration") {
        return TestRunner::Vitest;
    }
    TestRunner::Unknown
}

#[derive(Debug, PartialEq)]
enum ParseState {
    Preamble,
    TestSection,
    Failures,
    Summary,
}

#[derive(Debug, Default)]
struct JestCounts {
    suites_failed: usize,
    suites_passed: usize,
    suites_total: usize,
    tests_failed: usize,
    tests_skipped: usize,
    tests_passed: usize,
    tests_total: usize,
    time: String,
}

impl JestCounts {
    fn has_failures(&self) -> bool {
        self.tests_failed > 0 || self.suites_failed > 0
    }
}

/// Check if a line is Yarn/Jest noise that should be stripped.
fn is_noise_line(line: &str) -> bool {
    let trimmed = line.trim();

    // Empty lines
    if trimmed.is_empty() {
        return true;
    }

    // Yarn v1 preamble
    if trimmed.starts_with("yarn run v")
        || trimmed.starts_with("$ jest")
        || trimmed.starts_with("$ react-scripts test")
        || trimmed.starts_with("$ node ")
        || trimmed.starts_with("$ cross-env ")
        || trimmed.starts_with("$ ng test")
        || trimmed.starts_with("$ vitest")
    {
        return true;
    }

    // Angular/Vitest build noise
    if trimmed == "Building..." || trimmed.starts_with("Start at") {
        return true;
    }

    // Yarn Berry preamble/output
    if trimmed.starts_with("\u{27a4} YN") {
        return true;
    }

    // Yarn footer
    if trimmed.starts_with("Done in ")
        || trimmed.starts_with("error Command failed")
        || trimmed.starts_with("info Visit https://yarnpkg.com")
    {
        return true;
    }

    // Jest boilerplate
    if trimmed == "Ran all test suites."
        || trimmed.starts_with("Snapshots:")
        || (trimmed.starts_with("Snapshot") && trimmed.contains("total"))
    {
        return true;
    }

    false
}

/// Parse test output from yarn test, dispatching to the appropriate runner parser.
fn filter_yarn_test(output: &str) -> String {
    let clean = strip_ansi(output);

    match detect_runner(&clean) {
        TestRunner::Jest => filter_jest_output(&clean),
        TestRunner::Vitest => filter_vitest_text_output(&clean),
        TestRunner::Unknown => {
            // "No tests found" check
            if clean.to_lowercase().contains("no tests found") {
                return "Yarn test: No tests found".to_string();
            }
            // Non-recognized runner — truncated passthrough
            let stripped = clean
                .lines()
                .filter(|l| !is_noise_line(l))
                .collect::<Vec<_>>()
                .join("\n");
            truncate_output(&stripped, 2000)
        }
    }
}

/// Parse Jest test output, producing a compact summary.
fn filter_jest_output(clean: &str) -> String {
    let mut state = ParseState::Preamble;
    let mut counts = JestCounts::default();
    let mut failure_lines: Vec<String> = Vec::new();
    let mut current_failure: Vec<String> = Vec::new();
    let mut suite_results: Vec<String> = Vec::new();
    let mut found_any_suite = false;

    for line in clean.lines() {
        let trimmed = line.trim();

        // Skip noise in all states
        if is_noise_line(line) {
            continue;
        }

        // Detect Test Suites summary (triggers Summary state)
        if let Some(caps) = TEST_SUITES_RE.captures(trimmed) {
            // Save any pending failure
            if !current_failure.is_empty() {
                failure_lines.push(current_failure.join("\n"));
                current_failure.clear();
            }
            counts.suites_failed = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            counts.suites_passed = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            counts.suites_total = caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            state = ParseState::Summary;
            continue;
        }

        // Detect Tests summary line
        if let Some(caps) = TESTS_RE.captures(trimmed) {
            counts.tests_failed = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            counts.tests_skipped = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            counts.tests_passed = caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            counts.tests_total = caps
                .get(4)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            continue;
        }

        // Detect Time line
        if let Some(caps) = TIME_RE.captures(trimmed) {
            if let Some(time_match) = caps.get(1) {
                counts.time = time_match.as_str().trim().to_string();
            }
            continue;
        }

        // State transitions
        match state {
            ParseState::Preamble => {
                // Transition to TestSection on PASS/FAIL line
                if let Some(caps) = SUITE_RESULT_RE.captures(trimmed) {
                    found_any_suite = true;
                    let result = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let path = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    suite_results.push(format!("{} {}", result, path.trim()));
                    state = ParseState::TestSection;
                }
            }
            ParseState::TestSection => {
                // Track PASS/FAIL suite lines
                if let Some(caps) = SUITE_RESULT_RE.captures(trimmed) {
                    let result = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let path = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    suite_results.push(format!("{} {}", result, path.trim()));
                    continue;
                }

                // Detect failure marker (Jest uses lines starting with ● for failures)
                if trimmed.starts_with('\u{25cf}') || trimmed.starts_with("●") {
                    state = ParseState::Failures;
                    current_failure.push(truncate(trimmed, 120));
                    continue;
                }

                // Detect "FAIL" at start of failure block (without SUITE_RESULT_RE matching)
                if trimmed.starts_with("FAIL ") && !SUITE_RESULT_RE.is_match(trimmed) {
                    state = ParseState::Failures;
                    current_failure.push(truncate(trimmed, 120));
                    continue;
                }
            }
            ParseState::Failures => {
                // New suite result means new section
                if let Some(caps) = SUITE_RESULT_RE.captures(trimmed) {
                    if !current_failure.is_empty() {
                        failure_lines.push(current_failure.join("\n"));
                        current_failure.clear();
                    }
                    let result = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let path = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    suite_results.push(format!("{} {}", result, path.trim()));
                    state = ParseState::TestSection;
                    continue;
                }

                // New failure block
                if trimmed.starts_with('\u{25cf}') || trimmed.starts_with("●") {
                    if !current_failure.is_empty() {
                        failure_lines.push(current_failure.join("\n"));
                        current_failure.clear();
                    }
                    current_failure.push(truncate(trimmed, 120));
                    continue;
                }

                // Accumulate failure content (non-empty, non-noise)
                if !trimmed.is_empty() {
                    current_failure.push(truncate(trimmed, 120));
                }
            }
            ParseState::Summary => {
                // Already parsed summary lines above; skip remaining
            }
        }
    }

    // Save any pending failure
    if !current_failure.is_empty() {
        failure_lines.push(current_failure.join("\n"));
    }

    // No Jest patterns found (defensive — shouldn't happen since detect_runner routes here)
    if counts.tests_total == 0 && !found_any_suite {
        let stripped = clean
            .lines()
            .filter(|l| !is_noise_line(l))
            .collect::<Vec<_>>()
            .join("\n");
        return truncate_output(&stripped, 2000);
    }

    // Build output
    build_test_output(&counts, &failure_lines)
}

fn build_test_output(counts: &JestCounts, failure_lines: &[String]) -> String {
    if !counts.has_failures() {
        // All pass
        let mut msg = format!("ok Yarn test: {} passed", counts.tests_passed);
        if counts.tests_skipped > 0 {
            msg.push_str(&format!(", {} skipped", counts.tests_skipped));
        }
        if counts.suites_total > 0 {
            msg.push_str(&format!(
                " ({} {})",
                counts.suites_total,
                if counts.suites_total == 1 {
                    "suite"
                } else {
                    "suites"
                }
            ));
        }
        if !counts.time.is_empty() {
            msg.push_str(&format!(" [{}]", counts.time));
        }
        return msg;
    }

    // Failures present
    let mut result = String::new();
    result.push_str(&format!(
        "Yarn test: {} failed, {} passed",
        counts.tests_failed, counts.tests_passed
    ));
    if counts.tests_skipped > 0 {
        result.push_str(&format!(", {} skipped", counts.tests_skipped));
    }
    result.push_str(&format!(" ({} total)", counts.tests_total));
    if !counts.time.is_empty() {
        result.push_str(&format!(" [{}]", counts.time));
    }
    result.push('\n');

    // Show failure details
    if !failure_lines.is_empty() {
        result.push_str("\nFAILURES:\n");
        for (i, failure) in failure_lines.iter().take(10).enumerate() {
            result.push_str(&format!("{}. {}\n", i + 1, failure));
        }
        if failure_lines.len() > 10 {
            result.push_str(&format!(
                "\n... +{} more failures\n",
                failure_lines.len() - 10
            ));
        }
    }

    result.trim().to_string()
}

/// Parse Vitest text output (e.g. Angular 21 via `yarn test`), producing a compact summary.
fn filter_vitest_text_output(clean: &str) -> String {
    let mut failed: usize = 0;
    let mut skipped: usize = 0;
    let mut passed: usize = 0;
    let mut total: usize = 0;
    let mut duration_ms: Option<u64> = None;
    let mut failures: Vec<TestFailure> = Vec::new();

    // Collect failure blocks from ✗/× lines
    let lines: Vec<&str> = clean.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();

        // Parse Vitest summary lines
        if let Some(caps) = VITEST_TESTS_RE.captures(line) {
            failed = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            skipped = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            passed = caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            total = caps
                .get(4)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            i += 1;
            continue;
        }

        if let Some(caps) = VITEST_DURATION_RE.captures(line) {
            if let Ok(value) = caps[1].parse::<f64>() {
                let unit = &caps[2];
                duration_ms = Some(if unit == "ms" {
                    value as u64
                } else {
                    (value * 1000.0) as u64
                });
            }
            i += 1;
            continue;
        }

        // Detect failure lines: × test name
        if line.starts_with('\u{d7}') || line.starts_with("×") {
            let test_name = line
                .trim_start_matches('\u{d7}')
                .trim_start_matches('×')
                .trim()
                .to_string();
            let mut error_lines: Vec<String> = Vec::new();
            i += 1;

            // Collect indented error message lines (start with →)
            while i < lines.len() {
                let next = lines[i].trim();
                if next.starts_with('\u{2192}') || next.starts_with("→") {
                    error_lines.push(
                        next.trim_start_matches('\u{2192}')
                            .trim_start_matches('→')
                            .trim()
                            .to_string(),
                    );
                    i += 1;
                } else {
                    break;
                }
            }

            failures.push(TestFailure {
                test_name,
                file_path: String::new(),
                error_message: error_lines.join("\n"),
                stack_trace: None,
            });
            continue;
        }

        i += 1;
    }

    // If we found no summary data, fallback to passthrough
    if total == 0 {
        let stripped = clean
            .lines()
            .filter(|l| !is_noise_line(l))
            .collect::<Vec<_>>()
            .join("\n");
        return truncate_output(&stripped, 2000);
    }

    let result = TestResult {
        total,
        passed,
        failed,
        skipped,
        duration_ms,
        failures,
    };

    format!("Yarn test (vitest): {}", result.format_compact())
}

pub fn run_test(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("yarn");
    cmd.arg("test");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: yarn test {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run yarn test. Is Yarn installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_yarn_test(&raw);

    if let Some(hint) = crate::tee::tee_and_hint(&raw, "yarn_test", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("yarn test {}", args.join(" ")),
        &format!("rtk yarn test {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

pub fn run_other(args: &[OsString], verbose: u8) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("yarn: no subcommand specified");
    }

    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].to_string_lossy();
    let mut cmd = Command::new("yarn");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: yarn {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run yarn {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("yarn {}", subcommand),
        &format!("rtk yarn {}", subcommand),
        &raw,
        &raw, // No filtering for unsupported subcommands
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    #[test]
    fn test_filter_yarn_test_all_pass() {
        let input = r#"yarn run v1.22.22
$ jest
 PASS  src/utils.test.js
 PASS  src/app.test.js

Test Suites:  2 passed, 2 total
Tests:        8 passed, 8 total
Snapshots:   0 total
Time:        3.456 s
Ran all test suites.
Done in 4.12s."#;

        let result = filter_yarn_test(input);
        assert!(
            result.contains("ok Yarn test"),
            "Expected success marker, got: {}",
            result
        );
        assert!(
            result.contains("8 passed"),
            "Expected '8 passed', got: {}",
            result
        );
        assert!(
            result.contains("2 suites"),
            "Expected '2 suites', got: {}",
            result
        );
        assert!(result.contains("3.456 s"), "Expected time, got: {}", result);
        // Should NOT contain noise
        assert!(!result.contains("yarn run"), "Should strip yarn preamble");
        assert!(!result.contains("$ jest"), "Should strip jest invocation");
        assert!(!result.contains("Done in"), "Should strip yarn footer");
        assert!(
            !result.contains("Ran all test suites"),
            "Should strip jest boilerplate"
        );
        assert!(!result.contains("Snapshots"), "Should strip snapshots line");
    }

    #[test]
    fn test_filter_yarn_test_with_failures() {
        let input = r#"yarn run v1.22.22
$ jest
 PASS  src/utils.test.js
 FAIL  src/app.test.js
  ● App > should render correctly

    expect(received).toBe(expected)

    Expected: "Hello"
    Received: "World"

      12 |   const result = render(<App />);
      13 |   expect(result.text()).toBe("Hello");
         |                         ^
      14 | });

Test Suites:  1 failed, 1 passed, 2 total
Tests:        1 failed, 5 passed, 6 total
Snapshots:   0 total
Time:        2.345 s
Ran all test suites.
error Command failed with exit code 1."#;

        let result = filter_yarn_test(input);
        assert!(
            result.contains("1 failed"),
            "Expected failure count, got: {}",
            result
        );
        assert!(
            result.contains("FAILURES"),
            "Expected FAILURES section, got: {}",
            result
        );
        assert!(
            result.contains("should render correctly"),
            "Expected test name, got: {}",
            result
        );
        assert!(
            result.contains("Expected:"),
            "Expected assertion message, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_yarn_test_strips_yarn_noise() {
        let input = r#"yarn run v1.22.22
$ jest
 PASS  src/index.test.js

Test Suites:  1 passed, 1 total
Tests:        3 passed, 3 total
Snapshots:   0 total
Time:        1.2 s
Ran all test suites.
Done in 2.00s."#;

        let result = filter_yarn_test(input);
        assert!(!result.contains("yarn run"), "Should strip yarn run");
        assert!(!result.contains("$ jest"), "Should strip $ jest");
        assert!(!result.contains("Done in"), "Should strip Done in");
        assert!(
            !result.contains("Ran all test suites"),
            "Should strip boilerplate"
        );
        assert!(!result.contains("Snapshots"), "Should strip snapshots");
    }

    #[test]
    fn test_filter_yarn_test_no_tests() {
        let input = r#"yarn run v1.22.22
$ jest
No tests found, exiting with code 0
Done in 1.00s."#;

        let result = filter_yarn_test(input);
        assert!(
            result.contains("No tests found"),
            "Expected no tests message, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_yarn_test_with_skipped() {
        let input = r#"yarn run v1.22.22
$ jest
 PASS  src/utils.test.js
 PASS  src/app.test.js

Test Suites:  2 passed, 2 total
Tests:        2 skipped, 6 passed, 8 total
Snapshots:   0 total
Time:        2.5 s
Ran all test suites.
Done in 3.00s."#;

        let result = filter_yarn_test(input);
        assert!(
            result.contains("ok Yarn test"),
            "Expected success marker, got: {}",
            result
        );
        assert!(
            result.contains("6 passed"),
            "Expected passed count, got: {}",
            result
        );
        assert!(
            result.contains("2 skipped"),
            "Expected skipped count, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_yarn_test_berry_format() {
        let input = r#"➤ YN0000: · Yarn 4.0.2
➤ YN0000: ┌ Resolution step
➤ YN0000: └ Completed
➤ YN0000: ┌ Post-resolution validation
➤ YN0000: └ Completed
 PASS  src/utils.test.js

Test Suites:  1 passed, 1 total
Tests:        4 passed, 4 total
Snapshots:   0 total
Time:        1.8 s
Ran all test suites."#;

        let result = filter_yarn_test(input);
        assert!(
            result.contains("ok Yarn test"),
            "Expected success, got: {}",
            result
        );
        assert!(!result.contains("YN0000"), "Should strip Berry preamble");
        assert!(
            result.contains("4 passed"),
            "Expected 4 passed, got: {}",
            result
        );
    }

    #[test]
    fn test_yarn_test_token_savings() {
        let input = r#"yarn run v1.22.22
$ jest --verbose
 PASS  src/components/Button.test.tsx
  Button Component
    ✓ renders correctly (15 ms)
    ✓ handles click events (8 ms)
    ✓ applies disabled state (5 ms)
    ✓ renders with custom className (3 ms)
 PASS  src/components/Input.test.tsx
  Input Component
    ✓ renders correctly (12 ms)
    ✓ handles change events (6 ms)
    ✓ shows error state (4 ms)
    ✓ handles focus/blur (7 ms)
 PASS  src/hooks/useAuth.test.ts
  useAuth Hook
    ✓ returns authenticated state (20 ms)
    ✓ handles login (15 ms)
    ✓ handles logout (10 ms)
    ✓ refreshes token (8 ms)
 PASS  src/utils/format.test.ts
  Format Utilities
    ✓ formats currency (2 ms)
    ✓ formats date (3 ms)
    ✓ truncates text (1 ms)
    ✓ capitalizes string (1 ms)
 PASS  src/api/client.test.ts
  API Client
    ✓ makes GET request (25 ms)
    ✓ makes POST request (18 ms)
    ✓ handles errors (12 ms)
    ✓ includes auth headers (5 ms)
    ✓ retries on failure (30 ms)
 PASS  src/store/reducer.test.ts
  Store Reducer
    ✓ handles SET_USER action (3 ms)
    ✓ handles CLEAR_USER action (2 ms)
    ✓ handles SET_LOADING action (1 ms)
    ✓ handles SET_ERROR action (2 ms)
    ✓ handles RESET action (1 ms)

Test Suites:  6 passed, 6 total
Tests:        26 passed, 26 total
Snapshots:   0 total
Time:        5.234 s
Ran all test suites.
Done in 6.12s."#;

        let result = filter_yarn_test(input);
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&result);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 75.0,
            "Yarn test filter: expected >=75% savings, got {:.1}% (input: {} tokens, output: {} tokens)\nOutput: {}",
            savings, input_tokens, output_tokens, result
        );
    }

    #[test]
    fn test_parse_test_suites_line() {
        let line = "Test Suites:  1 failed, 2 passed, 3 total";
        let caps = TEST_SUITES_RE.captures(line).expect("Should match");
        assert_eq!(caps.get(1).unwrap().as_str(), "1");
        assert_eq!(caps.get(2).unwrap().as_str(), "2");
        assert_eq!(caps.get(3).unwrap().as_str(), "3");
    }

    #[test]
    fn test_parse_tests_line() {
        let line = "Tests:  1 failed, 2 skipped, 5 passed, 8 total";
        let caps = TESTS_RE.captures(line).expect("Should match");
        assert_eq!(caps.get(1).unwrap().as_str(), "1");
        assert_eq!(caps.get(2).unwrap().as_str(), "2");
        assert_eq!(caps.get(3).unwrap().as_str(), "5");
        assert_eq!(caps.get(4).unwrap().as_str(), "8");
    }

    #[test]
    fn test_is_noise_line_yarn_preamble() {
        assert!(is_noise_line("yarn run v1.22.22"));
        assert!(is_noise_line("$ jest"));
        assert!(is_noise_line("$ react-scripts test"));
        assert!(is_noise_line("$ node scripts/test.js"));
    }

    #[test]
    fn test_is_noise_line_yarn_footer() {
        assert!(is_noise_line("Done in 4.12s."));
        assert!(is_noise_line("error Command failed with exit code 1."));
        assert!(is_noise_line(
            "info Visit https://yarnpkg.com/en/docs/cli/run for documentation."
        ));
    }

    #[test]
    fn test_is_noise_line_not_noise() {
        assert!(!is_noise_line(" PASS  src/utils.test.js"));
        assert!(!is_noise_line(" FAIL  src/app.test.js"));
        assert!(!is_noise_line("Test Suites:  2 passed, 2 total"));
        assert!(!is_noise_line("  ● App > should render correctly"));
    }

    #[test]
    fn test_filter_yarn_test_ansi_stripped() {
        let input = "\x1b[1m\x1b[32m PASS \x1b[39m\x1b[22m src/utils.test.js\n\
                      \n\
                      Test Suites:  \x1b[1m\x1b[32m1 passed\x1b[39m\x1b[22m, 1 total\n\
                      Tests:        \x1b[1m\x1b[32m3 passed\x1b[39m\x1b[22m, 3 total\n\
                      Snapshots:   0 total\n\
                      Time:        1.5 s\n\
                      Ran all test suites.";

        let result = filter_yarn_test(input);
        assert!(
            result.contains("ok Yarn test"),
            "Expected success with ANSI stripped, got: {}",
            result
        );
        assert!(!result.contains("\x1b["), "Should not contain ANSI codes");
    }

    #[test]
    fn test_filter_yarn_test_non_jest_runner_passthrough() {
        // Angular/Karma style output — no Jest PASS/FAIL or Test Suites lines
        let input = r#"yarn run v1.22.22
$ ng test --watch=false
10% building 3/3 modules 0 activeBuilding...
30% building 15/15 modules 0 active
chunk {main} main.js (main) 12.3 kB [entry]
chunk {vendor} vendor.js (vendor) 450 kB [initial]
Chrome Headless 120.0: Executed 42 of 42 SUCCESS (1.234 secs / 0.987 secs)
Done in 15.00s."#;

        let result = filter_yarn_test(input);
        assert!(
            !result.contains("No tests found"),
            "Non-Jest runner should NOT show 'No tests found', got: {}",
            result
        );
        // Should preserve meaningful lines
        assert!(
            result.contains("Executed 42 of 42 SUCCESS"),
            "Should preserve Karma results, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_yarn_test_jest_no_tests_preserved() {
        // Jest explicitly says "No tests found" — should still return that message
        let input = r#"yarn run v1.22.22
$ jest
No tests found, exiting with code 0
Done in 0.50s."#;

        let result = filter_yarn_test(input);
        assert_eq!(
            result, "Yarn test: No tests found",
            "Jest 'No tests found' should be preserved, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_yarn_test_non_jest_truncation() {
        // Large non-Jest output should be truncated
        let mut lines = vec!["yarn run v1.22.22".to_string(), "$ ng test".to_string()];
        for i in 0..200 {
            lines.push(format!("  TestResult {}: some long test output line that keeps going and going to fill up space number {}", i, i));
        }
        lines.push("Done in 30.00s.".to_string());
        let input = lines.join("\n");

        let result = filter_yarn_test(&input);
        assert!(
            !result.contains("No tests found"),
            "Large non-Jest output should NOT show 'No tests found'"
        );
        assert!(
            result.contains("[RTK:PASSTHROUGH]"),
            "Large non-Jest output should be truncated with passthrough marker, got len: {}",
            result.len()
        );
    }

    #[test]
    fn test_filter_yarn_test_multiple_suites() {
        let input = r#" PASS  src/a.test.js
 PASS  src/b.test.js
 PASS  src/c.test.js
 FAIL  src/d.test.js
  ● D > fails

    Expected: true
    Received: false

Test Suites:  1 failed, 3 passed, 4 total
Tests:        1 failed, 11 passed, 12 total
Snapshots:   0 total
Time:        4.0 s"#;

        let result = filter_yarn_test(input);
        assert!(
            result.contains("1 failed"),
            "Expected 1 failed, got: {}",
            result
        );
        assert!(
            result.contains("11 passed"),
            "Expected 11 passed, got: {}",
            result
        );
        assert!(
            result.contains("12 total"),
            "Expected 12 total, got: {}",
            result
        );
        assert!(
            result.contains("FAILURES"),
            "Expected FAILURES section, got: {}",
            result
        );
        assert!(
            result.contains("D > fails"),
            "Expected failure name, got: {}",
            result
        );
    }

    // --- TestRunner detection tests ---

    #[test]
    fn test_detect_runner_jest() {
        let jest_output = " PASS  src/utils.test.js\nTest Suites:  1 passed, 1 total";
        assert_eq!(detect_runner(jest_output), TestRunner::Jest);
    }

    #[test]
    fn test_detect_runner_jest_pass_fail_only() {
        // Even without "Test Suites:", PASS/FAIL prefix should detect Jest
        let jest_output = " PASS  src/utils.test.js\n FAIL  src/app.test.js";
        assert_eq!(detect_runner(jest_output), TestRunner::Jest);
    }

    #[test]
    fn test_detect_runner_vitest() {
        let vitest_output =
            " Test Files  12 passed (12)\n      Tests  109 passed (109)\n   Duration  8.45s";
        assert_eq!(detect_runner(vitest_output), TestRunner::Vitest);
    }

    #[test]
    fn test_detect_runner_unknown() {
        let unknown_output = "Chrome Headless: Executed 42 of 42 SUCCESS";
        assert_eq!(detect_runner(unknown_output), TestRunner::Unknown);
    }

    // --- Vitest output tests ---

    #[test]
    fn test_filter_vitest_all_pass() {
        let input = include_str!("../tests/fixtures/yarn_vitest_all_pass.txt");
        let result = filter_yarn_test(input);

        assert!(
            result.contains("Yarn test (vitest):"),
            "Expected vitest prefix, got: {}",
            result
        );
        assert!(
            result.contains("PASS (109)"),
            "Expected 109 passed, got: {}",
            result
        );
        assert!(
            result.contains("FAIL (0)"),
            "Expected 0 failed, got: {}",
            result
        );
        assert!(
            result.contains("8450ms"),
            "Expected duration, got: {}",
            result
        );
        // Should NOT contain noise
        assert!(!result.contains("yarn run"), "Should strip yarn preamble");
        assert!(!result.contains("Building"), "Should strip build noise");
        assert!(!result.contains("Start at"), "Should strip Start at line");
        assert!(!result.contains("Done in"), "Should strip yarn footer");
    }

    #[test]
    fn test_filter_vitest_with_failures() {
        let input = include_str!("../tests/fixtures/yarn_vitest_with_failures.txt");
        let result = filter_yarn_test(input);

        assert!(
            result.contains("Yarn test (vitest):"),
            "Expected vitest prefix, got: {}",
            result
        );
        assert!(
            result.contains("PASS (106)"),
            "Expected 106 passed, got: {}",
            result
        );
        assert!(
            result.contains("FAIL (3)"),
            "Expected 3 failed, got: {}",
            result
        );
        // Failure details
        assert!(
            result.contains("should display user name"),
            "Expected failure test name, got: {}",
            result
        );
        assert!(
            result.contains("should toggle menu"),
            "Expected second failure, got: {}",
            result
        );
        assert!(
            result.contains("should validate email"),
            "Expected third failure, got: {}",
            result
        );
    }

    #[test]
    fn test_vitest_token_savings() {
        let input = include_str!("../tests/fixtures/yarn_vitest_all_pass.txt");
        let result = filter_yarn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&result);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "Vitest filter: expected >=60% savings, got {:.1}% (input: {} tokens, output: {} tokens)\nOutput: {}",
            savings, input_tokens, output_tokens, result
        );
    }

    #[test]
    fn test_vitest_with_failures_token_savings() {
        let input = include_str!("../tests/fixtures/yarn_vitest_with_failures.txt");
        let result = filter_yarn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&result);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "Vitest failure filter: expected >=60% savings, got {:.1}% (input: {} tokens, output: {} tokens)\nOutput: {}",
            savings, input_tokens, output_tokens, result
        );
    }

    #[test]
    fn test_is_noise_line_vitest_preamble() {
        assert!(is_noise_line("$ ng test --watch=false"));
        assert!(is_noise_line("$ vitest run"));
        assert!(is_noise_line("Building..."));
        assert!(is_noise_line("Start at  14:23:45"));
    }

    #[test]
    fn test_vitest_regex_tests_all_pass() {
        let line = "Tests  109 passed (109)";
        let caps = VITEST_TESTS_RE.captures(line).expect("Should match");
        assert!(caps.get(1).is_none()); // no failed
        assert!(caps.get(2).is_none()); // no skipped
        assert_eq!(caps.get(3).unwrap().as_str(), "109");
        assert_eq!(caps.get(4).unwrap().as_str(), "109");
    }

    #[test]
    fn test_vitest_regex_tests_with_failures() {
        let line = "Tests  3 failed | 106 passed (109)";
        let caps = VITEST_TESTS_RE.captures(line).expect("Should match");
        assert_eq!(caps.get(1).unwrap().as_str(), "3");
        assert!(caps.get(2).is_none()); // no skipped
        assert_eq!(caps.get(3).unwrap().as_str(), "106");
        assert_eq!(caps.get(4).unwrap().as_str(), "109");
    }

    #[test]
    fn test_vitest_regex_tests_with_skipped() {
        let line = "Tests  1 failed | 5 skipped | 100 passed (106)";
        let caps = VITEST_TESTS_RE.captures(line).expect("Should match");
        assert_eq!(caps.get(1).unwrap().as_str(), "1");
        assert_eq!(caps.get(2).unwrap().as_str(), "5");
        assert_eq!(caps.get(3).unwrap().as_str(), "100");
        assert_eq!(caps.get(4).unwrap().as_str(), "106");
    }

    #[test]
    fn test_vitest_regex_duration_seconds() {
        let line = "Duration  8.45s (transform 1.23s, setup 0.89s)";
        let caps = VITEST_DURATION_RE.captures(line).expect("Should match");
        assert_eq!(&caps[1], "8.45");
        assert_eq!(&caps[2], "s");
    }

    #[test]
    fn test_vitest_regex_duration_ms() {
        let line = "Duration  450ms";
        let caps = VITEST_DURATION_RE.captures(line).expect("Should match");
        assert_eq!(&caps[1], "450");
        assert_eq!(&caps[2], "ms");
    }
}
