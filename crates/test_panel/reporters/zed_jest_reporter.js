// Jest reporter used by Zed's test panel.
//
// Emits one JSON object per line, prefixed with `##zed-test## `, so that the panel can
// attribute results and console output to individual tests without parsing the human
// readable reporter output.

const path = require("node:path");

const PREFIX = "##zed-test## ";

function emit(event) {
  process.stdout.write(PREFIX + JSON.stringify(event) + "\n");
}

function statusOf(result) {
  switch (result.status) {
    case "passed":
      return "passed";
    case "failed":
      return "failed";
    case "pending":
    case "skipped":
    case "todo":
    case "disabled":
      return "skipped";
    default:
      return null;
  }
}

class ZedReporter {
  constructor(globalConfig) {
    this.root = globalConfig.rootDir;
    this.reported = new Set();
  }

  file(testPath) {
    return path.relative(this.root, testPath);
  }

  report(testPath, result) {
    const key = testPath + "\0" + result.fullName;
    if (this.reported.has(key)) {
      return;
    }
    const status = statusOf(result);
    if (!status) {
      return;
    }
    this.reported.add(key);
    emit({
      event: "finish",
      file: this.file(testPath),
      path: [...result.ancestorTitles, result.title],
      status,
      duration_ms: result.duration,
      output: (result.failureMessages || []).join("\n"),
    });
  }

  onTestCaseResult(test, result) {
    this.report(test.path, result);
  }

  onTestResult(test, testResult) {
    for (const result of testResult.testResults) {
      this.report(test.path, result);
    }
    for (const entry of testResult.console || []) {
      emit({
        event: "output",
        file: this.file(test.path),
        path: [],
        text: entry.message,
      });
    }
    if (testResult.testExecError || testResult.failureMessage) {
      emit({
        event: "output",
        file: this.file(test.path),
        path: [],
        text: testResult.failureMessage || String(testResult.testExecError),
      });
    }
  }

  onRunComplete() {}

  getLastError() {}
}

module.exports = ZedReporter;
