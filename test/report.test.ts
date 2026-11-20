// The summary view's pure helpers (src/report.ts).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  diffClass,
  formatDiff,
  formatSecs,
  isUnlocked,
  rowMarks,
  type DayReport,
  type ReportRow,
} from "../src/report.ts";

function row(extra: Partial<ReportRow> = {}): ReportRow {
  return {
    itemId: "a",
    title: "A",
    status: "done",
    carriedFrom: null,
    tags: [],
    plannedSecs: 1500,
    plannedSource: "baseline",
    actualSecs: 1900,
    differenceSecs: 400,
    ...extra,
  };
}

function report(extra: Partial<DayReport> = {}): DayReport {
  return {
    date: "2026-11-21",
    lockDecision: "locked",
    lockedAt: "2026-11-21T08:00:00Z",
    rows: [],
    baseline: [],
    segments: [],
    markers: [],
    ...extra,
  };
}

test("formatSecs formats like the tray, and none as a dash", () => {
  assert.equal(formatSecs(0), "00:00");
  assert.equal(formatSecs(1500), "25:00");
  assert.equal(formatSecs(3725), "1:02:05");
  assert.equal(formatSecs(null), "—");
});

test("formatDiff carries a sign, and none as a dash", () => {
  assert.equal(formatDiff(400), "+06:40");
  assert.equal(formatDiff(-600), "−10:00");
  assert.equal(formatDiff(-3725), "−1:02:05");
  assert.equal(formatDiff(0), "00:00");
  assert.equal(formatDiff(null), "—");
  assert.equal(diffClass(400), "over");
  assert.equal(diffClass(-1), "under");
  assert.equal(diffClass(0), "");
  assert.equal(diffClass(null), "");
});

test("rowMarks marks unplanned, removed, carried-over and carried-in rows", () => {
  assert.deepEqual(rowMarks(row()), []);
  assert.deepEqual(
    rowMarks(row({ plannedSource: "unplanned", plannedSecs: null, differenceSecs: null })),
    ["unplanned"],
  );
  assert.deepEqual(rowMarks(row({ status: "removed" })), ["removed"]);
  assert.deepEqual(rowMarks(row({ status: "carriedOver" })), ["carried over"]);
  assert.deepEqual(rowMarks(row({ carriedFrom: "z", plannedSource: "unlocked" })), ["carried in"]);
  assert.deepEqual(rowMarks(row({ status: "removed", plannedSource: "unplanned" })), [
    "unplanned",
    "removed",
  ]);
});

test("isUnlocked is true for every decision but locked", () => {
  assert.equal(isUnlocked(report()), false);
  assert.equal(isUnlocked(report({ lockDecision: "noLock", lockedAt: null })), true);
  assert.equal(isUnlocked(report({ lockDecision: "none", lockedAt: null })), true);
});
