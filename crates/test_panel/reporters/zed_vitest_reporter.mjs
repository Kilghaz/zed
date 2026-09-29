// Vitest reporter used by Zed's test panel.
//
// Emits one JSON object per line, prefixed with `##zed-test## `, so that the panel can
// attribute results and console output to individual tests without parsing the human
// readable reporter output. Works with the `onTaskUpdate` reporter API that Vitest 1, 2
// and 3 provide.

import { relative } from "node:path";

const PREFIX = "##zed-test## ";

function emit(event) {
  process.stdout.write(PREFIX + JSON.stringify(event) + "\n");
}

function isFile(task) {
  return task && typeof task.filepath === "string";
}

function taskPath(task) {
  const path = [];
  let current = task;
  while (current && !isFile(current)) {
    if (current.name) {
      path.unshift(current.name);
    }
    current = current.suite;
  }
  return path;
}

function taskFile(root, task) {
  let current = task;
  while (current && !isFile(current)) {
    current = current.suite || current.file;
  }
  if (!current) {
    return null;
  }
  return relative(root, current.filepath);
}

function statusOf(result, task) {
  const state = result ? result.state : task.mode;
  switch (state) {
    case "pass":
      return "passed";
    case "fail":
      return "failed";
    case "skip":
    case "todo":
      return "skipped";
    default:
      return null;
  }
}

function errorText(result) {
  if (!result || !Array.isArray(result.errors)) {
    return "";
  }
  return result.errors
    .map((error) => error.stack || error.message || String(error))
    .join("\n");
}

function walk(task, visit) {
  visit(task);
  if (Array.isArray(task.tasks)) {
    for (const child of task.tasks) {
      walk(child, visit);
    }
  }
}

export default class ZedReporter {
  constructor() {
    this.reported = new Set();
  }

  onInit(ctx) {
    this.ctx = ctx;
    this.root = ctx.config.root;
  }

  report(task, result) {
    if (!task || (task.type !== "test" && task.type !== "custom")) {
      return;
    }
    const file = taskFile(this.root, task);
    const path = taskPath(task);
    if (result && result.state === "run") {
      emit({ event: "start", file, path });
      return;
    }
    const status = statusOf(result, task);
    if (!status || this.reported.has(task.id)) {
      return;
    }
    this.reported.add(task.id);
    emit({
      event: "finish",
      file,
      path,
      status,
      duration_ms: result ? result.duration : undefined,
      output: errorText(result),
    });
  }

  onTaskUpdate(packs) {
    if (!this.ctx) {
      return;
    }
    for (const [id, result] of packs) {
      this.report(this.ctx.state.idMap.get(id), result);
    }
  }

  onUserConsoleLog(log) {
    const task =
      log.taskId && this.ctx ? this.ctx.state.idMap.get(log.taskId) : null;
    emit({
      event: "output",
      file: task ? taskFile(this.root, task) : null,
      path: task ? taskPath(task) : [],
      text: log.content,
    });
  }

  onFinished(files = [], errors = []) {
    for (const file of files) {
      walk(file, (task) => this.report(task, task.result));
    }
    for (const error of errors) {
      emit({
        event: "output",
        file: null,
        path: [],
        text: error.stack || error.message || String(error),
      });
    }
  }
}
