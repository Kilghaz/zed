"""pytest plugin used by Zed's test panel.

Emits one JSON object per line, prefixed with ``##zed-test## ``, so that the panel can
attribute results and captured output to individual tests without parsing the human
readable pytest output. Loaded with ``-p zed_pytest_reporter``.
"""

import json
import sys

PREFIX = "##zed-test## "


def _emit(event):
    sys.stdout.write(PREFIX + json.dumps(event) + "\n")
    sys.stdout.flush()


def _split(nodeid):
    parts = nodeid.split("::")
    return parts[0], parts[1:]


def pytest_runtest_logstart(nodeid, location):
    file, path = _split(nodeid)
    _emit({"event": "start", "file": file, "path": path})


def pytest_runtest_logreport(report):
    # The call phase carries the outcome; setup and teardown only matter when they fail
    # or skip the test.
    if report.when != "call" and report.outcome == "passed":
        return
    file, path = _split(report.nodeid)
    status = {"passed": "passed", "failed": "failed", "skipped": "skipped"}.get(
        report.outcome, "failed"
    )
    output = ""
    for section_name, content in getattr(report, "sections", []):
        if content:
            output += "----- %s -----\n%s\n" % (section_name, content)
    if report.failed:
        output += getattr(report, "longreprtext", "") or ""
    elif report.skipped and getattr(report, "longrepr", None):
        output += str(report.longrepr) + "\n"
    _emit(
        {
            "event": "finish",
            "file": file,
            "path": path,
            "status": status,
            "duration_ms": round(getattr(report, "duration", 0.0) * 1000, 3),
            "output": output,
        }
    )
