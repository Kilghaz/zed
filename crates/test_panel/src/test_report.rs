//! Parsing of test runner output into per-test results.
//!
//! Test runners print their results in runner specific, human readable formats. To show
//! the status and the output of individual tests, the panel re-parses the full output of
//! a run whenever it changes, producing a [`TestReport`]: a flat list of the tests and
//! suites the runner mentioned, each with the output lines the runner attributed to it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// Prefix of the JSON lines emitted by the reporters shipped with Zed.
pub const STRUCTURED_PREFIX: &str = "##zed-test## ";

/// The reporter scripts shipped with Zed, extracted to disk before a run.
pub const REPORTER_FILES: &[(&str, &str)] = &[
    (
        "zed_vitest_reporter.mjs",
        include_str!("../reporters/zed_vitest_reporter.mjs"),
    ),
    (
        "zed_jest_reporter.js",
        include_str!("../reporters/zed_jest_reporter.js"),
    ),
    (
        "zed_pytest_reporter.py",
        include_str!("../reporters/zed_pytest_reporter.py"),
    ),
];

/// The test runner a task uses, detected from its command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runner {
    /// `cargo test`, which prints libtest's `test name ... ok` lines.
    Libtest,
    /// `go test`, in either its `-v` text format or its `-json` format.
    GoTest,
    Vitest,
    Jest,
    Pytest,
    /// A runner whose output is not understood: all output is attributed to the run.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestStatus {
    Running,
    Passed,
    Failed,
    Skipped,
    /// Enumerated by a collection run, but not executed.
    NotRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Test,
    Suite,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReportEntry {
    pub kind: EntryKind,
    /// The file the runner reported the test in, in the runner's own notation.
    pub file: Option<String>,
    /// The suites enclosing the test, followed by the test's own name.
    pub path: Vec<String>,
    pub status: TestStatus,
    pub duration: Option<Duration>,
    /// The output lines the runner attributed to this test or suite.
    pub output: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TestReport {
    pub entries: Vec<ReportEntry>,
    /// Output that the runner did not attribute to a particular test, such as runner
    /// start-up, compilation and the final summary.
    pub run_output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportFormat {
    /// The runner's own human readable output.
    Text,
    /// JSON lines emitted by one of the reporters in [`REPORTER_FILES`].
    Structured,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReporterSetup {
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub format: ReportFormat,
}

/// A test enumerated by a collection run, without having been executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectedTest {
    pub file: Option<String>,
    pub path: Vec<String>,
}

/// Quotes a path for the shell the task runs in, since Zed's data directory contains a
/// space on macOS.
fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

/// Writes the reporter scripts to `dir`, returning the directory once every file is in
/// place. Existing files are rewritten so that upgrades take effect.
pub fn write_reporter_files(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for (name, contents) in REPORTER_FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*contents) {
            std::fs::write(&path, contents)?;
        }
    }
    Ok(dir.to_path_buf())
}

impl TestReport {
    fn entry_index(&self, file: Option<&str>, path: &[String]) -> Option<usize> {
        self.entries.iter().position(|entry| {
            entry.path == path
                && (file.is_none() || entry.file.is_none() || entry.file.as_deref() == file)
        })
    }

    fn upsert(
        &mut self,
        kind: EntryKind,
        file: Option<&str>,
        path: Vec<String>,
        status: TestStatus,
        duration: Option<Duration>,
    ) -> usize {
        match self.entry_index(file, &path) {
            Some(index) => {
                let entry = &mut self.entries[index];
                if entry.kind == EntryKind::Suite && kind == EntryKind::Test {
                    entry.kind = kind;
                }
                if entry.status == TestStatus::Running || status != TestStatus::Running {
                    entry.status = status;
                }
                if duration.is_some() {
                    entry.duration = duration;
                }
                if entry.file.is_none() {
                    entry.file = file.map(str::to_owned);
                }
                index
            }
            None => {
                self.entries.push(ReportEntry {
                    kind,
                    file: file.map(str::to_owned),
                    path,
                    status,
                    duration,
                    output: String::new(),
                });
                self.entries.len() - 1
            }
        }
    }

    fn append_output(&mut self, index: Option<usize>, line: &str) {
        let target = match index.and_then(|index| self.entries.get_mut(index)) {
            Some(entry) => &mut entry.output,
            None => &mut self.run_output,
        };
        target.push_str(line);
        target.push('\n');
    }

    fn append_output_by_path(&mut self, file: Option<&str>, path: &[String], line: &str) {
        let index = match self.entry_index(file, path) {
            Some(index) => index,
            None if path.is_empty() => {
                self.append_output(None, line);
                return;
            }
            None => self.upsert(
                EntryKind::Suite,
                file,
                path.to_vec(),
                TestStatus::Running,
                None,
            ),
        };
        self.append_output(Some(index), line);
    }
}

impl Runner {
    pub fn detect(command_label: &str) -> Self {
        let command = command_label.to_lowercase();
        let has_word = |word: &str| {
            command
                .split(|c: char| {
                    c.is_whitespace() || c == '/' || c == '\\' || c == '"' || c == '\''
                })
                .any(|token| token == word)
        };
        if command.contains("cargo test") || command.contains("cargo nextest") {
            Self::Libtest
        } else if command.contains("go test") {
            Self::GoTest
        } else if has_word("vitest") {
            Self::Vitest
        } else if has_word("jest") {
            Self::Jest
        } else if has_word("pytest") || has_word("py.test") {
            Self::Pytest
        } else {
            Self::Unknown
        }
    }

    /// How a run's output should be parsed and what to add to the command line so that
    /// every test is reported individually. Prefers the structured reporters shipped with
    /// Zed when `reporter_dir` holds them, and falls back to the runner's verbose output.
    pub fn reporter_setup(&self, args: &[String], reporter_dir: Option<&Path>) -> ReporterSetup {
        let has_prefix = |prefix: &str| args.iter().any(|arg| arg.starts_with(prefix));
        let has = |flag: &str| args.iter().any(|arg| arg == flag);
        match (self, reporter_dir) {
            (Self::Vitest, Some(dir)) if !has_prefix("--reporter") => ReporterSetup {
                args: vec![
                    "--reporter".to_owned(),
                    quoted(&dir.join("zed_vitest_reporter.mjs")),
                ],
                env: HashMap::default(),
                format: ReportFormat::Structured,
            },
            (Self::Jest, Some(dir)) if !has_prefix("--reporters") => ReporterSetup {
                args: vec![
                    "--reporters".to_owned(),
                    quoted(&dir.join("zed_jest_reporter.js")),
                ],
                env: HashMap::default(),
                format: ReportFormat::Structured,
            },
            (Self::Pytest, Some(dir)) if !has("-p") => {
                let mut env = HashMap::default();
                env.insert("PYTHONPATH".to_owned(), dir.to_string_lossy().into_owned());
                ReporterSetup {
                    args: vec!["-p".to_owned(), "zed_pytest_reporter".to_owned()],
                    env,
                    format: ReportFormat::Structured,
                }
            }
            (Self::GoTest, _) if !has("-json") => ReporterSetup {
                args: vec!["-json".to_owned()],
                env: HashMap::default(),
                format: ReportFormat::Text,
            },
            _ => ReporterSetup {
                args: self.verbose_args(args),
                env: HashMap::default(),
                format: ReportFormat::Text,
            },
        }
    }

    /// Arguments that make the runner list its tests instead of running them, or `None`
    /// when the runner cannot enumerate tests.
    pub fn collection_args(&self, args: &[String]) -> Option<Vec<String>> {
        match self {
            Self::Libtest => {
                let mut extra = Vec::new();
                if !args.iter().any(|arg| arg == "--") {
                    extra.push("--".to_owned());
                }
                extra.push("--list".to_owned());
                Some(extra)
            }
            Self::Pytest => Some(vec!["--collect-only".to_owned(), "-q".to_owned()]),
            Self::GoTest => Some(vec!["-list".to_owned(), ".".to_owned()]),
            Self::Vitest | Self::Jest | Self::Unknown => None,
        }
    }

    /// Parses the output of a collection run into the tests the runner knows about.
    pub fn parse_collection(&self, output: &str) -> Vec<CollectedTest> {
        let output = strip_ansi(output);
        let mut tests = Vec::new();
        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match self {
                Self::Libtest => {
                    if let Some(name) = line
                        .strip_suffix(": test")
                        .or_else(|| line.strip_suffix(": benchmark"))
                    {
                        tests.push(CollectedTest {
                            file: None,
                            path: libtest_path(name),
                        });
                    }
                }
                Self::Pytest => {
                    if line.contains("::") && !line.contains(' ') {
                        let mut segments = line.split("::");
                        let file = segments.next().map(str::to_owned);
                        let path: Vec<String> = segments.map(str::to_owned).collect();
                        if !path.is_empty() {
                            tests.push(CollectedTest { file, path });
                        }
                    }
                }
                Self::GoTest => {
                    if !line.contains(char::is_whitespace)
                        && (line.starts_with("Test")
                            || line.starts_with("Example")
                            || line.starts_with("Fuzz"))
                    {
                        tests.push(CollectedTest {
                            file: None,
                            path: go_test_path(line),
                        });
                    }
                }
                Self::Vitest | Self::Jest | Self::Unknown => {}
            }
        }
        tests
    }

    /// Arguments appended to the runner's command line so that it reports every test on
    /// its own line, which is what the parsers rely on.
    pub fn verbose_args(&self, args: &[String]) -> Vec<String> {
        let has = |flag: &str| args.iter().any(|arg| arg == flag);
        let has_prefix = |prefix: &str| args.iter().any(|arg| arg.starts_with(prefix));
        match self {
            Self::GoTest => {
                if has("-v") {
                    Vec::new()
                } else {
                    vec!["-v".to_owned()]
                }
            }
            Self::Vitest => {
                if has_prefix("--reporter") {
                    Vec::new()
                } else {
                    vec!["--reporter=verbose".to_owned()]
                }
            }
            Self::Jest => {
                if has("--verbose") {
                    Vec::new()
                } else {
                    vec!["--verbose".to_owned()]
                }
            }
            Self::Pytest => {
                let mut extra = Vec::new();
                if !has("-v") && !has("--verbose") && !has("-vv") {
                    extra.push("-v".to_owned());
                }
                if !has_prefix("-r") {
                    extra.push("-rA".to_owned());
                }
                extra
            }
            Self::Libtest | Self::Unknown => Vec::new(),
        }
    }

    pub fn parse(&self, output: &str, format: ReportFormat) -> TestReport {
        let output = strip_ansi(output);
        if format == ReportFormat::Structured {
            return parse_structured(&output);
        }
        match self {
            Self::Libtest => parse_libtest(&output),
            Self::GoTest => parse_go_test(&output),
            Self::Vitest => parse_vitest(&output),
            Self::Jest => parse_jest(&output),
            Self::Pytest => parse_pytest(&output),
            Self::Unknown => TestReport {
                entries: Vec::new(),
                run_output: output,
            },
        }
    }
}

/// Removes ANSI escape sequences (colors, cursor movement) from terminal output.
pub fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            result.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut previous = None;
                for c in chars.by_ref() {
                    if c == '\u{7}' || (previous == Some('\u{1b}') && c == '\\') {
                        break;
                    }
                    previous = Some(c);
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    result
}

fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim().trim_start_matches('(').trim_end_matches(')');
    if let Some(millis) = text.strip_suffix("ms") {
        return millis
            .trim()
            .parse::<f64>()
            .ok()
            .map(Duration::from_secs_f64)
            .map(|d| d / 1000);
    }
    if let Some(seconds) = text.strip_suffix('s') {
        return seconds
            .trim()
            .parse::<f64>()
            .ok()
            .map(Duration::from_secs_f64);
    }
    None
}

fn parse_structured(output: &str) -> TestReport {
    let mut report = TestReport::default();
    for line in output.lines() {
        let trimmed = line.trim_end();
        let Some(index) = trimmed.find(STRUCTURED_PREFIX) else {
            report.append_output(None, trimmed);
            continue;
        };
        let json = &trimmed[index + STRUCTURED_PREFIX.len()..];
        let Ok(event) = serde_json::from_str::<serde_json::Value>(json) else {
            report.append_output(None, trimmed);
            continue;
        };
        let file = event["file"].as_str().filter(|file| !file.is_empty());
        let path: Vec<String> = event["path"]
            .as_array()
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| segment.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        match event["event"].as_str().unwrap_or_default() {
            "start" if !path.is_empty() => {
                report.upsert(EntryKind::Test, file, path, TestStatus::Running, None);
            }
            "finish" if !path.is_empty() => {
                let status = match event["status"].as_str().unwrap_or_default() {
                    "passed" => TestStatus::Passed,
                    "failed" => TestStatus::Failed,
                    "skipped" => TestStatus::Skipped,
                    _ => TestStatus::Running,
                };
                let duration = event["duration_ms"]
                    .as_f64()
                    .map(|millis| Duration::from_secs_f64(millis / 1000.0));
                let index = report.upsert(EntryKind::Test, file, path, status, duration);
                if let Some(output) = event["output"].as_str()
                    && !output.is_empty()
                {
                    for output_line in output.lines() {
                        report.append_output(Some(index), output_line);
                    }
                }
            }
            "output" => {
                if let Some(text) = event["output"].as_str().or_else(|| event["text"].as_str()) {
                    for output_line in text.lines() {
                        if path.is_empty() {
                            report.append_output(None, output_line);
                        } else {
                            report.append_output_by_path(file, &path, output_line);
                        }
                    }
                }
            }
            _ => report.append_output(None, trimmed),
        }
    }
    report
}

fn parse_libtest(output: &str) -> TestReport {
    let mut report = TestReport::default();
    // Output of failed tests is printed in `---- name stdout ----` sections at the end.
    let mut current_section: Option<usize> = None;
    // With `--nocapture` and a single test, the test's output precedes its result line, so
    // it is buffered until the test's name is known.
    let mut single_test_output: Option<Vec<String>> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("---- ")
            && let Some(name) = rest.strip_suffix(" stdout ----")
        {
            let path = libtest_path(name);
            current_section =
                Some(report.upsert(EntryKind::Test, None, path, TestStatus::Failed, None));
            continue;
        }
        if trimmed == "failures:" || trimmed.starts_with("test result:") {
            current_section = None;
            single_test_output = None;
        }
        if let Some(count) = trimmed
            .strip_prefix("running ")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|count| count.parse::<usize>().ok())
        {
            single_test_output = (count == 1).then(Vec::new);
            report.append_output(None, trimmed);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("test ")
            && let Some((name, outcome)) = rest.rsplit_once(" ... ")
            && !name.contains(" has been running")
        {
            let (outcome, duration) = match outcome.rsplit_once(" <") {
                Some((outcome, duration)) => {
                    (outcome, parse_duration(duration.trim_end_matches('>')))
                }
                None => (outcome, None),
            };
            let status = if outcome.starts_with("ok") {
                TestStatus::Passed
            } else if outcome.starts_with("FAILED") {
                TestStatus::Failed
            } else if outcome.starts_with("ignored") {
                TestStatus::Skipped
            } else {
                TestStatus::Running
            };
            let path = libtest_path(name);
            let index = report.upsert(EntryKind::Test, None, path, status, duration);
            if let Some(buffered) = single_test_output.take() {
                for buffered_line in buffered {
                    report.append_output(Some(index), &buffered_line);
                }
            }
            continue;
        }
        match &mut single_test_output {
            Some(buffered) if current_section.is_none() => buffered.push(trimmed.to_owned()),
            _ => report.append_output(current_section, trimmed),
        }
    }
    if let Some(buffered) = single_test_output {
        for buffered_line in buffered {
            report.append_output(None, &buffered_line);
        }
    }
    report
}

fn libtest_path(name: &str) -> Vec<String> {
    // Doc tests are reported as `src/lib.rs - module::item (line 12)`.
    if name.contains(" - ") && name.ends_with(')') {
        return vec![name.to_owned()];
    }
    name.split("::").map(str::to_owned).collect()
}

fn parse_go_test(output: &str) -> TestReport {
    let mut report = TestReport::default();
    let mut current: Option<Vec<String>> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with('{') && trimmed.contains("\"Action\"") {
            parse_go_test_json_line(trimmed, &mut report);
            continue;
        }
        if let Some(rest) = trimmed.trim_start().strip_prefix("=== ") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let action = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default().trim();
            match action {
                "RUN" => {
                    let path = go_test_path(name);
                    report.upsert(
                        EntryKind::Test,
                        None,
                        path.clone(),
                        TestStatus::Running,
                        None,
                    );
                    current = Some(path);
                }
                "CONT" => current = Some(go_test_path(name)),
                "PAUSE" => current = None,
                _ => {}
            }
            continue;
        }
        if let Some(rest) = trimmed.trim_start().strip_prefix("--- ")
            && let Some((outcome, rest)) = rest.split_once(": ")
        {
            let (name, duration) = match rest.rsplit_once(" (") {
                Some((name, duration)) => (name, parse_duration(duration.trim_end_matches(')'))),
                None => (rest, None),
            };
            let status = match outcome {
                "PASS" => TestStatus::Passed,
                "FAIL" => TestStatus::Failed,
                "SKIP" => TestStatus::Skipped,
                _ => TestStatus::Running,
            };
            report.upsert(EntryKind::Test, None, go_test_path(name), status, duration);
            current = None;
            continue;
        }
        if trimmed == "PASS"
            || trimmed == "FAIL"
            || trimmed.starts_with("ok ")
            || trimmed.starts_with("FAIL\t")
        {
            current = None;
        }
        match &current {
            Some(path) => report.append_output_by_path(None, path, trimmed),
            None => report.append_output(None, trimmed),
        }
    }
    report
}

fn go_test_path(name: &str) -> Vec<String> {
    name.split('/').map(str::to_owned).collect()
}

fn parse_go_test_json_line(line: &str, report: &mut TestReport) {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        report.append_output(None, line);
        return;
    };
    let action = event["Action"].as_str().unwrap_or_default();
    let test = event["Test"].as_str();
    let output = event["Output"].as_str();
    let duration = event["Elapsed"].as_f64().map(Duration::from_secs_f64);
    match (action, test) {
        ("run", Some(name)) => {
            report.upsert(
                EntryKind::Test,
                None,
                go_test_path(name),
                TestStatus::Running,
                None,
            );
        }
        ("pass", Some(name)) => {
            report.upsert(
                EntryKind::Test,
                None,
                go_test_path(name),
                TestStatus::Passed,
                duration,
            );
        }
        ("fail", Some(name)) => {
            report.upsert(
                EntryKind::Test,
                None,
                go_test_path(name),
                TestStatus::Failed,
                duration,
            );
        }
        ("skip", Some(name)) => {
            report.upsert(
                EntryKind::Test,
                None,
                go_test_path(name),
                TestStatus::Skipped,
                duration,
            );
        }
        ("output", test) => {
            if let Some(output) = output {
                let line = output.trim_end_matches('\n');
                let is_marker =
                    line.trim_start().starts_with("=== ") || line.trim_start().starts_with("--- ");
                if !is_marker {
                    match test {
                        Some(name) => report.append_output_by_path(None, &go_test_path(name), line),
                        None => report.append_output(None, line),
                    }
                }
            }
        }
        _ => {}
    }
}

fn split_js_path(text: &str) -> Vec<String> {
    text.split(" > ")
        .map(|part| part.trim().to_owned())
        .collect()
}

fn looks_like_file(segment: &str) -> bool {
    segment.contains('/')
        || segment.contains('\\')
        || segment.ends_with(".ts")
        || segment.ends_with(".js")
        || segment.ends_with(".tsx")
        || segment.ends_with(".jsx")
        || segment.ends_with(".mjs")
        || segment.ends_with(".cjs")
}

fn parse_vitest(output: &str) -> TestReport {
    let mut report = TestReport::default();
    let mut current_file: Option<String> = None;
    // `stdout | file > suite > test` blocks and `FAIL file > suite > test` sections
    // attribute the following lines to a test.
    let mut attribution: Option<(Option<String>, Vec<String>)> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();
        let content = trimmed.trim_start();

        if content.starts_with("⎯⎯")
            || content.starts_with("Test Files ")
            || content.starts_with("Tests ") && content.contains("passed")
        {
            attribution = None;
            report.append_output(None, trimmed);
            continue;
        }
        if let Some(rest) = content
            .strip_prefix("stdout | ")
            .or_else(|| content.strip_prefix("stderr | "))
        {
            let (file, path) = split_vitest_location(rest);
            attribution = Some((file, path));
            continue;
        }
        if let Some(rest) = content.strip_prefix("FAIL ") {
            let rest = rest.trim_start();
            if rest.contains(" > ") {
                let (file, path) = split_vitest_location(rest);
                report.upsert(
                    EntryKind::Test,
                    file.as_deref(),
                    path.clone(),
                    TestStatus::Failed,
                    None,
                );
                attribution = Some((file, path));
                continue;
            }
        }
        if let Some((symbol, rest)) = split_status_symbol(content) {
            let status = match symbol {
                '✓' | '✔' | '√' => TestStatus::Passed,
                '×' | '✗' | '✘' | '❯' => TestStatus::Failed,
                '↓' | '⏭' | '○' => TestStatus::Skipped,
                _ => TestStatus::Running,
            };
            let (rest, duration) = split_trailing_duration(rest);
            let rest = rest
                .trim_end_matches(" [skipped]")
                .trim_end_matches(" [todo]");
            let first_segment = rest.split(" > ").next().unwrap_or_default().trim();
            if looks_like_file(first_segment) && !rest.contains(" > ") {
                // A file summary line such as `✓ src/math.test.ts (3 tests) 5ms`.
                let file = first_segment
                    .split(" (")
                    .next()
                    .unwrap_or(first_segment)
                    .to_owned();
                current_file = Some(file);
                attribution = None;
                report.append_output(None, trimmed);
                continue;
            }
            let (file, path) = if looks_like_file(first_segment) {
                let (file, path) = split_vitest_location(rest);
                (file, path)
            } else {
                (current_file.clone(), split_js_path(rest))
            };
            if !path.is_empty() {
                report.upsert(EntryKind::Test, file.as_deref(), path, status, duration);
                attribution = None;
                continue;
            }
        }
        match &attribution {
            Some((file, path)) => {
                if content.is_empty() {
                    attribution = None;
                } else {
                    report.append_output_by_path(file.as_deref(), path, trimmed);
                }
            }
            None => report.append_output(None, trimmed),
        }
    }
    report
}

fn split_vitest_location(text: &str) -> (Option<String>, Vec<String>) {
    let mut segments = split_js_path(text);
    if segments
        .first()
        .is_some_and(|segment| looks_like_file(segment))
    {
        let file = segments.remove(0);
        let file = file.split(" (").next().unwrap_or(&file).to_owned();
        (Some(file), segments)
    } else {
        (None, segments)
    }
}

fn split_status_symbol(content: &str) -> Option<(char, &str)> {
    let mut chars = content.chars();
    let symbol = chars.next()?;
    if !matches!(
        symbol,
        '✓' | '✔' | '√' | '×' | '✗' | '✘' | '✕' | '❯' | '↓' | '⏭' | '○' | '✎'
    ) {
        return None;
    }
    let rest = chars.as_str();
    if !rest.starts_with(' ') {
        return None;
    }
    Some((symbol, rest.trim_start()))
}

fn split_trailing_duration(text: &str) -> (&str, Option<Duration>) {
    let text = text.trim_end();
    // Jest prints durations in parentheses with a space, e.g. `name (2 ms)`.
    if text.ends_with(')')
        && let Some((rest, duration)) = text.rsplit_once(" (")
        && let Some(duration) = parse_duration(duration.trim_end_matches(')'))
    {
        return (rest.trim_end(), Some(duration));
    }
    if let Some((rest, duration)) = text.rsplit_once(' ')
        && (duration.ends_with("ms") || duration.ends_with('s'))
        && let Some(duration) = parse_duration(duration)
    {
        return (rest.trim_end(), Some(duration));
    }
    (text, None)
}

fn parse_jest(output: &str) -> TestReport {
    let mut report = TestReport::default();
    let mut current_file: Option<String> = None;
    // Describe blocks are printed as indented lines without a status symbol; the tests
    // under them are indented one level deeper.
    let mut suites: Vec<(usize, String)> = Vec::new();
    let mut attribution: Option<(Option<String>, Vec<String>)> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();
        let content = trimmed.trim_start();
        let indent = trimmed.len() - content.len();

        if let Some(rest) = content
            .strip_prefix("PASS ")
            .or_else(|| content.strip_prefix("FAIL "))
        {
            let file = rest.split(" (").next().unwrap_or(rest).trim().to_owned();
            current_file = Some(file);
            suites.clear();
            attribution = None;
            report.append_output(None, trimmed);
            continue;
        }
        if content.starts_with("Tests:")
            || content.starts_with("Test Suites:")
            || content.starts_with("Snapshots:")
        {
            attribution = None;
            suites.clear();
            report.append_output(None, trimmed);
            continue;
        }
        if let Some(rest) = content.strip_prefix("● ") {
            let path: Vec<String> = rest
                .split(" › ")
                .map(|part| part.trim().to_owned())
                .collect();
            if path.len() > 1 || current_file.is_some() {
                report.upsert(
                    EntryKind::Test,
                    current_file.as_deref(),
                    path.clone(),
                    TestStatus::Failed,
                    None,
                );
                attribution = Some((current_file.clone(), path));
                continue;
            }
        }
        if let Some((symbol, rest)) = split_status_symbol(content) {
            let status = match symbol {
                '✓' | '✔' | '√' => TestStatus::Passed,
                '✕' | '×' | '✗' | '✘' => TestStatus::Failed,
                '○' | '✎' => TestStatus::Skipped,
                _ => TestStatus::Running,
            };
            let rest = rest
                .strip_prefix("skipped ")
                .or_else(|| rest.strip_prefix("todo "))
                .unwrap_or(rest);
            let (name, duration) = split_trailing_duration(rest);
            while suites
                .last()
                .is_some_and(|(suite_indent, _)| *suite_indent >= indent)
            {
                suites.pop();
            }
            let mut path: Vec<String> = suites.iter().map(|(_, name)| name.clone()).collect();
            path.push(name.to_owned());
            report.upsert(
                EntryKind::Test,
                current_file.as_deref(),
                path,
                status,
                duration,
            );
            attribution = None;
            continue;
        }
        if current_file.is_some()
            && attribution.is_none()
            && !content.is_empty()
            && indent >= 2
            && !content.starts_with("console.")
            && !content.starts_with("at ")
        {
            while suites
                .last()
                .is_some_and(|(suite_indent, _)| *suite_indent >= indent)
            {
                suites.pop();
            }
            suites.push((indent, content.to_owned()));
            let path: Vec<String> = suites.iter().map(|(_, name)| name.clone()).collect();
            report.upsert(
                EntryKind::Suite,
                current_file.as_deref(),
                path,
                TestStatus::Running,
                None,
            );
            continue;
        }
        match &attribution {
            Some((file, path)) => report.append_output_by_path(file.as_deref(), path, trimmed),
            None => report.append_output(None, trimmed),
        }
    }
    report
}

fn parse_pytest(output: &str) -> TestReport {
    let mut report = TestReport::default();
    let mut attribution: Option<Vec<String>> = None;
    let mut in_details = false;

    for line in output.lines() {
        let trimmed = line.trim_end();
        let content = trimmed.trim_start();

        if content.starts_with("====") {
            in_details = content.contains("FAILURES")
                || content.contains("PASSES")
                || content.contains("ERRORS");
            attribution = None;
            report.append_output(None, trimmed);
            continue;
        }
        if in_details && content.starts_with("____") && content.ends_with("____") {
            let name = content.trim_matches('_').trim();
            let name = name.split(" - ").next().unwrap_or(name).trim();
            let path: Vec<String> = name
                .split(['.', ':'])
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect();
            if !path.is_empty() {
                attribution = Some(path);
                continue;
            }
        }
        if !in_details
            && let Some((location, outcome)) = content.split_once(' ')
            && location.contains("::")
        {
            let outcome = outcome.trim_start();
            let status = if outcome.starts_with("PASSED") {
                Some(TestStatus::Passed)
            } else if outcome.starts_with("FAILED") || outcome.starts_with("ERROR") {
                Some(TestStatus::Failed)
            } else if outcome.starts_with("SKIPPED")
                || outcome.starts_with("XFAIL")
                || outcome.starts_with("XPASS")
            {
                Some(TestStatus::Skipped)
            } else {
                None
            };
            if let Some(status) = status {
                let mut segments = location.split("::");
                let file = segments.next().map(str::to_owned);
                let path: Vec<String> = segments.map(str::to_owned).collect();
                if !path.is_empty() {
                    report.upsert(EntryKind::Test, file.as_deref(), path, status, None);
                    continue;
                }
            }
        }
        match &attribution {
            Some(path) => {
                let index = report.entries.iter().position(|entry| {
                    entry.path.ends_with(path) || entry.path.last() == path.last()
                });
                report.append_output(index, trimmed);
            }
            None => report.append_output(None, trimmed),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(report: &'a TestReport, path: &[&str]) -> &'a ReportEntry {
        report
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("no entry for {path:?} in {:#?}", report.entries))
    }

    #[test]
    fn detects_runners() {
        assert_eq!(
            Runner::detect("cargo test -p zed tests::foo"),
            Runner::Libtest
        );
        assert_eq!(
            Runner::detect("go test -run ^TestFoo$ ./..."),
            Runner::GoTest
        );
        assert_eq!(
            Runner::detect("npx vitest run src/foo.test.ts"),
            Runner::Vitest
        );
        assert_eq!(
            Runner::detect("node_modules/.bin/jest src/foo.test.ts"),
            Runner::Jest
        );
        assert_eq!(
            Runner::detect("pytest tests/test_x.py::test_a"),
            Runner::Pytest
        );
        assert_eq!(Runner::detect("make check"), Runner::Unknown);
    }

    #[test]
    fn parses_libtest_output() {
        let output = "\
   Compiling foo v0.1.0
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.02s
     Running unittests src/lib.rs (target/debug/deps/foo-abc)

running 3 tests
test tests::adds ... ok
test tests::ignored_one ... ignored
test tests::fails ... FAILED

failures:

---- tests::fails stdout ----
thread 'tests::fails' panicked at src/lib.rs:12:9:
assertion `left == right` failed
  left: 1
 right: 2


failures:
    tests::fails

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let report = Runner::Libtest.parse(output, ReportFormat::Text);
        assert_eq!(report.entries.len(), 3);
        assert_eq!(
            entry(&report, &["tests", "adds"]).status,
            TestStatus::Passed
        );
        assert_eq!(
            entry(&report, &["tests", "ignored_one"]).status,
            TestStatus::Skipped
        );
        let failed = entry(&report, &["tests", "fails"]);
        assert_eq!(failed.status, TestStatus::Failed);
        assert!(failed.output.contains("assertion `left == right` failed"));
        assert!(report.run_output.contains("Compiling foo"));
        assert!(report.run_output.contains("test result: FAILED"));
        assert!(!report.run_output.contains("left == right"));
    }

    #[test]
    fn attributes_nocapture_output_of_a_single_libtest_test() {
        let output = "\
running 1 test
hello from the test
test tests::adds ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s
";
        let report = Runner::Libtest.parse(output, ReportFormat::Text);
        let adds = entry(&report, &["tests", "adds"]);
        assert_eq!(adds.status, TestStatus::Passed);
        assert!(adds.output.contains("hello from the test"));
        assert!(!report.run_output.contains("hello from the test"));
    }

    #[test]
    fn parses_go_test_output() {
        let output = "\
=== RUN   TestAdd
    math_test.go:10: adding
=== RUN   TestAdd/negative
    math_test.go:14: negative case
--- PASS: TestAdd/negative (0.00s)
--- PASS: TestAdd (0.01s)
=== RUN   TestFail
    math_test.go:20: expected 2, got 3
--- FAIL: TestFail (0.00s)
=== RUN   TestSkip
--- SKIP: TestSkip (0.00s)
FAIL
FAIL\texample.com/math\t0.123s
";
        let report = Runner::GoTest.parse(output, ReportFormat::Text);
        let add = entry(&report, &["TestAdd"]);
        assert_eq!(add.status, TestStatus::Passed);
        assert_eq!(add.duration, Some(Duration::from_millis(10)));
        assert!(add.output.contains("adding"));
        let negative = entry(&report, &["TestAdd", "negative"]);
        assert_eq!(negative.status, TestStatus::Passed);
        assert!(negative.output.contains("negative case"));
        let fail = entry(&report, &["TestFail"]);
        assert_eq!(fail.status, TestStatus::Failed);
        assert!(fail.output.contains("expected 2, got 3"));
        assert_eq!(entry(&report, &["TestSkip"]).status, TestStatus::Skipped);
        assert!(report.run_output.contains("example.com/math"));
    }

    #[test]
    fn parses_go_test_json_output() {
        let output = r#"{"Time":"2024-01-01T00:00:00Z","Action":"run","Package":"example.com/math","Test":"TestAdd"}
{"Time":"2024-01-01T00:00:00Z","Action":"output","Package":"example.com/math","Test":"TestAdd","Output":"=== RUN   TestAdd\n"}
{"Time":"2024-01-01T00:00:00Z","Action":"output","Package":"example.com/math","Test":"TestAdd","Output":"    math_test.go:10: adding\n"}
{"Time":"2024-01-01T00:00:00Z","Action":"pass","Package":"example.com/math","Test":"TestAdd","Elapsed":0.5}
{"Time":"2024-01-01T00:00:00Z","Action":"output","Package":"example.com/math","Output":"ok  \texample.com/math\t0.6s\n"}
"#;
        let report = Runner::GoTest.parse(output, ReportFormat::Text);
        let add = entry(&report, &["TestAdd"]);
        assert_eq!(add.status, TestStatus::Passed);
        assert_eq!(add.duration, Some(Duration::from_millis(500)));
        assert!(add.output.contains("adding"));
        assert!(!add.output.contains("=== RUN"));
        assert!(report.run_output.contains("ok"));
    }

    #[test]
    fn parses_vitest_verbose_output() {
        let output = "
 RUN  v2.1.0 /home/me/project

stdout | src/math.test.ts > add > logs
hello from test

stdout | src/math.test.ts > add
hello from beforeAll

 ✓ src/math.test.ts > add > adds numbers 2ms
 ✓ src/math.test.ts > add > logs 1ms
 × src/math.test.ts > add > fails 3ms
 ↓ src/math.test.ts > add > skipped [skipped]

⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  src/math.test.ts > add > fails
AssertionError: expected 3 to be 4
 ❯ src/math.test.ts:9:19

⎯⎯⎯⎯⎯⎯⎯[1/1]⎯⎯⎯⎯⎯⎯⎯

 Test Files  1 failed (1)
      Tests  1 failed | 2 passed | 1 skipped (4)
";
        let report = Runner::Vitest.parse(output, ReportFormat::Text);
        let adds = entry(&report, &["add", "adds numbers"]);
        assert_eq!(adds.status, TestStatus::Passed);
        assert_eq!(adds.file.as_deref(), Some("src/math.test.ts"));
        assert_eq!(adds.duration, Some(Duration::from_millis(2)));
        let logs = entry(&report, &["add", "logs"]);
        assert!(logs.output.contains("hello from test"));
        let suite = entry(&report, &["add"]);
        assert_eq!(suite.kind, EntryKind::Suite);
        assert!(suite.output.contains("hello from beforeAll"));
        let fails = entry(&report, &["add", "fails"]);
        assert_eq!(fails.status, TestStatus::Failed);
        assert!(fails.output.contains("expected 3 to be 4"));
        assert_eq!(
            entry(&report, &["add", "skipped"]).status,
            TestStatus::Skipped
        );
        assert!(report.run_output.contains("RUN  v2.1.0"));
        assert!(report.run_output.contains("Test Files"));
    }

    #[test]
    fn parses_vitest_file_grouped_output() {
        let output = "\
 ✓ src/math.test.ts (2 tests) 5ms
   ✓ add > adds numbers
   × add > fails
";
        let report = Runner::Vitest.parse(output, ReportFormat::Text);
        let adds = entry(&report, &["add", "adds numbers"]);
        assert_eq!(adds.file.as_deref(), Some("src/math.test.ts"));
        assert_eq!(adds.status, TestStatus::Passed);
        assert_eq!(entry(&report, &["add", "fails"]).status, TestStatus::Failed);
    }

    #[test]
    fn parses_jest_verbose_output() {
        let output = "\
FAIL src/math.test.ts
  add
    ✓ adds numbers (2 ms)
    ✕ fails (3 ms)
    ○ skipped skips
    nested
      ✓ deep (1 ms)

  ● add › fails

    expect(received).toBe(expected)

    Expected: 4
    Received: 3

      at Object.<anonymous> (src/math.test.ts:8:19)

Test Suites: 1 failed, 1 total
Tests:       1 failed, 1 skipped, 2 passed, 4 total
";
        let report = Runner::Jest.parse(output, ReportFormat::Text);
        let adds = entry(&report, &["add", "adds numbers"]);
        assert_eq!(adds.status, TestStatus::Passed);
        assert_eq!(adds.file.as_deref(), Some("src/math.test.ts"));
        assert_eq!(adds.duration, Some(Duration::from_millis(2)));
        let fails = entry(&report, &["add", "fails"]);
        assert_eq!(fails.status, TestStatus::Failed);
        assert!(fails.output.contains("Expected: 4"));
        assert_eq!(
            entry(&report, &["add", "skips"]).status,
            TestStatus::Skipped
        );
        assert_eq!(
            entry(&report, &["add", "nested", "deep"]).status,
            TestStatus::Passed
        );
        assert_eq!(entry(&report, &["add"]).kind, EntryKind::Suite);
        assert!(report.run_output.contains("Tests:       1 failed"));
    }

    #[test]
    fn parses_pytest_verbose_output() {
        let output = "\
============================= test session starts ==============================
collected 3 items

tests/test_math.py::TestAdd::test_adds PASSED                            [ 33%]
tests/test_math.py::test_fails FAILED                                    [ 66%]
tests/test_math.py::test_skipped SKIPPED (not today)                     [100%]

=================================== FAILURES ===================================
__________________________________ test_fails __________________________________

    def test_fails():
>       assert 1 == 2
E       assert 1 == 2

tests/test_math.py:9: AssertionError
----------------------------- Captured stdout call -----------------------------
about to fail
==================================== PASSES ====================================
_____________________________ TestAdd.test_adds ________________________________
----------------------------- Captured stdout call -----------------------------
adding
=========================== short test summary info ============================
FAILED tests/test_math.py::test_fails - assert 1 == 2
==================== 1 failed, 1 passed, 1 skipped in 0.02s ====================
";
        let report = Runner::Pytest.parse(output, ReportFormat::Text);
        let adds = entry(&report, &["TestAdd", "test_adds"]);
        assert_eq!(adds.status, TestStatus::Passed);
        assert_eq!(adds.file.as_deref(), Some("tests/test_math.py"));
        assert!(adds.output.contains("adding"));
        let fails = entry(&report, &["test_fails"]);
        assert_eq!(fails.status, TestStatus::Failed);
        assert!(fails.output.contains("about to fail"));
        assert!(fails.output.contains("assert 1 == 2"));
        assert_eq!(
            entry(&report, &["test_skipped"]).status,
            TestStatus::Skipped
        );
        assert!(report.run_output.contains("test session starts"));
    }

    #[test]
    fn parses_structured_reporter_output() {
        let output = "\
 RUN  v3.0.0
##zed-test## {\"event\":\"start\",\"file\":\"src/math.test.ts\",\"path\":[\"add\",\"adds\"]}
##zed-test## {\"event\":\"output\",\"file\":\"src/math.test.ts\",\"path\":[\"add\",\"adds\"],\"text\":\"hello\\nworld\"}
##zed-test## {\"event\":\"finish\",\"file\":\"src/math.test.ts\",\"path\":[\"add\",\"adds\"],\"status\":\"passed\",\"duration_ms\":2.5}
##zed-test## {\"event\":\"finish\",\"file\":\"src/math.test.ts\",\"path\":[\"add\",\"fails\"],\"status\":\"failed\",\"output\":\"expected 1\"}
##zed-test## {\"event\":\"output\",\"file\":\"src/math.test.ts\",\"path\":[\"add\"],\"text\":\"from beforeAll\"}
done
";
        let report = Runner::Vitest.parse(output, ReportFormat::Structured);
        let adds = entry(&report, &["add", "adds"]);
        assert_eq!(adds.status, TestStatus::Passed);
        assert_eq!(adds.duration, Some(Duration::from_micros(2500)));
        assert_eq!(adds.output, "hello\nworld\n");
        assert_eq!(entry(&report, &["add", "fails"]).output, "expected 1\n");
        let suite = entry(&report, &["add"]);
        assert_eq!(suite.kind, EntryKind::Suite);
        assert!(suite.output.contains("from beforeAll"));
        assert!(report.run_output.contains("RUN  v3.0.0"));
        assert!(report.run_output.contains("done"));
    }

    #[test]
    fn parses_collection_output() {
        let libtest = Runner::Libtest.parse_collection(
            "tests::adds: test\ntests::fails: test\nsrc/lib.rs - foo (line 3): test\n\n3 tests, 0 benchmarks\n",
        );
        assert_eq!(libtest.len(), 3);
        assert_eq!(libtest[0].path, vec!["tests", "adds"]);
        assert_eq!(libtest[2].path, vec!["src/lib.rs - foo (line 3)"]);

        let pytest = Runner::Pytest.parse_collection(
            "tests/test_math.py::TestAdd::test_adds\ntests/test_math.py::test_fails\n\n2 tests collected in 0.01s\n",
        );
        assert_eq!(pytest.len(), 2);
        assert_eq!(pytest[0].file.as_deref(), Some("tests/test_math.py"));
        assert_eq!(pytest[0].path, vec!["TestAdd", "test_adds"]);

        let go = Runner::GoTest
            .parse_collection("TestAdd\nTestFail\nExampleFoo\nok  \texample.com/math\t0.001s\n");
        assert_eq!(go.len(), 3);
        assert_eq!(go[0].path, vec!["TestAdd"]);
    }

    #[test]
    fn prefers_structured_reporters_when_available() {
        let dir = Path::new("/tmp/reporters");
        let setup = Runner::Vitest.reporter_setup(&[], Some(dir));
        assert_eq!(setup.format, ReportFormat::Structured);
        assert_eq!(setup.args[0], "--reporter");
        assert!(setup.args[1].starts_with('"') && setup.args[1].ends_with('"'));
        let setup = Runner::Pytest.reporter_setup(&[], Some(dir));
        assert_eq!(setup.args, vec!["-p", "zed_pytest_reporter"]);
        assert!(setup.env.contains_key("PYTHONPATH"));
        let setup = Runner::Vitest.reporter_setup(&[], None);
        assert_eq!(setup.format, ReportFormat::Text);
        assert_eq!(setup.args, vec!["--reporter=verbose"]);
        let setup = Runner::GoTest.reporter_setup(&[], Some(dir));
        assert_eq!(setup.args, vec!["-json"]);
        assert!(
            Runner::Libtest
                .collection_args(&["test".to_owned()])
                .unwrap()
                .contains(&"--list".to_owned())
        );
    }

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[32m✓\u{1b}[0m ok"), "✓ ok");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
    }

    #[test]
    fn adds_verbose_flags_once() {
        assert_eq!(Runner::Vitest.verbose_args(&[]), vec!["--reporter=verbose"]);
        assert!(
            Runner::Vitest
                .verbose_args(&["--reporter=json".to_owned()])
                .is_empty()
        );
        assert_eq!(Runner::Pytest.verbose_args(&["-v".to_owned()]), vec!["-rA"]);
        assert!(Runner::GoTest.verbose_args(&["-v".to_owned()]).is_empty());
        assert!(Runner::Libtest.verbose_args(&[]).is_empty());
    }
}
