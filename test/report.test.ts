// The summary view's pure helpers (src/report.ts).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  diffClass,
  formatDiff,
  formatSecs,
  hourTicks,
  isUnlocked,
  layoutTimeline,
  markerLabel,
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

// ---- layoutTimeline ----

const T = (hhmm: string) => `2026-11-21T${hhmm}:00Z`;
const ms = (hhmm: string) => Date.parse(T(hhmm));
const MIN = 60_000;

function slot(itemId: string, from: string | null, to: string | null) {
  return {
    itemId,
    plannedSecs: 1500,
    scheduledStart: from === null ? null : T(from),
    scheduledEnd: to === null ? null : T(to),
  };
}

function seg(itemId: string, from: string, to: string, open = false) {
  return { itemId, start: T(from), end: T(to), open };
}

type Marker = DayReport["markers"][number];
function marker(kind: Marker["kind"], at: string, extra: Partial<Marker> = {}): Marker {
  return { at: T(at), kind, itemId: "a", toItemId: null, secs: null, ...extra };
}

const rowsAB = [row(), row({ itemId: "b", title: "B" })];

test("layoutTimeline: locked day lays both lanes on one span", () => {
  const t = layoutTimeline(
    report({
      rows: rowsAB,
      baseline: [slot("a", "09:00", "09:25"), slot("b", "09:25", "10:00")],
      segments: [seg("a", "09:00", "09:32"), seg("b", "09:32", "10:10")],
    }),
  );
  assert.ok(t);
  assert.equal(t.startMs, ms("09:00"));
  assert.equal(t.endMs, ms("10:10"));
  assert.equal(t.unlocked, false);
  assert.equal(t.baseline.rows, 1);
  assert.equal(t.actual.rows, 1);
  assert.deepEqual(
    t.baseline.bars.map((b) => [b.title, b.left, b.width, b.row]),
    [
      ["A", 0, 25 / 70, 0],
      ["B", 25 / 70, 35 / 70, 0],
    ],
  );
  assert.deepEqual(
    t.actual.bars.map((b) => [b.title, b.left, b.width, b.open]),
    [
      ["A", 0, 32 / 70, false],
      ["B", 32 / 70, 38 / 70, false],
    ],
  );
  assert.equal(t.actual.bars[1].endMs, ms("10:10"));
});

test("layoutTimeline: overlapping segments go on separate sub-rows", () => {
  const t = layoutTimeline(
    report({ rows: rowsAB, segments: [seg("a", "09:00", "09:30"), seg("b", "09:20", "09:40")] }),
  );
  assert.ok(t);
  assert.equal(t.actual.rows, 2);
  assert.deepEqual(
    t.actual.bars.map((b) => b.row),
    [0, 1],
  );
});

test("layoutTimeline: a paused-and-resumed item yields one bar per segment", () => {
  const t = layoutTimeline(
    report({
      rows: rowsAB,
      segments: [
        seg("a", "09:00", "09:10"),
        seg("b", "09:10", "09:20"),
        seg("a", "09:20", "09:30"),
      ],
    }),
  );
  assert.ok(t);
  assert.equal(t.actual.rows, 1);
  assert.deepEqual(
    t.actual.bars.map((b) => b.title),
    ["A", "B", "A"],
  );
});

test("layoutTimeline: markers outside the baseline widen the span", () => {
  const t = layoutTimeline(
    report({
      rows: rowsAB,
      baseline: [slot("a", "09:00", "10:00")],
      markers: [
        marker("extended", "08:30", { secs: 300 }),
        marker("inserted", "10:30", { itemId: "b" }),
      ],
    }),
  );
  assert.ok(t);
  assert.equal(t.startMs, ms("08:30"));
  assert.equal(t.endMs, ms("10:30"));
  assert.deepEqual(
    t.markers.map((m) => [m.kind, m.label, m.title, m.left, m.atMs]),
    [
      ["extended", "Extended +05:00", "A", 0, ms("08:30")],
      ["inserted", "Inserted", "B", 1, ms("10:30")],
    ],
  );
  assert.equal(t.baseline.bars[0].left, 0.25);
  assert.equal(t.baseline.bars[0].width, 0.5);
});

test("layoutTimeline: an open segment is flagged open", () => {
  const t = layoutTimeline(report({ rows: rowsAB, segments: [seg("a", "09:00", "09:40", true)] }));
  assert.ok(t);
  assert.equal(t.actual.bars[0].open, true);
});

test("layoutTimeline: an unlocked day has an empty baseline lane, actual and markers drawn", () => {
  for (const lockDecision of ["noLock", "none"] as const) {
    const t = layoutTimeline(
      report({
        lockDecision,
        lockedAt: null,
        rows: rowsAB,
        segments: [seg("a", "09:00", "09:30")],
        markers: [marker("switched", "09:30", { toItemId: "b" })],
      }),
    );
    assert.ok(t);
    assert.equal(t.unlocked, true);
    assert.deepEqual(t.baseline, { bars: [], rows: 0 });
    assert.equal(t.actual.bars.length, 1);
    assert.equal(t.markers.length, 1);
  }
});

test("layoutTimeline: a done-at-lock slot without times is skipped", () => {
  const t = layoutTimeline(
    report({ rows: rowsAB, baseline: [slot("a", null, null), slot("b", "09:00", "09:30")] }),
  );
  assert.ok(t);
  assert.deepEqual(
    t.baseline.bars.map((b) => b.itemId),
    ["b"],
  );
});

test("layoutTimeline: a span under an hour is widened to one hour from its start", () => {
  const t = layoutTimeline(report({ markers: [marker("recovered", "09:15")] }));
  assert.ok(t);
  assert.equal(t.startMs, ms("09:15"));
  assert.equal(t.endMs, ms("09:15") + 60 * MIN);
  assert.equal(t.markers[0].left, 0);
});

test("layoutTimeline: nothing to draw gives null", () => {
  assert.equal(layoutTimeline(report()), null);
  assert.equal(layoutTimeline(report({ rows: rowsAB, baseline: [slot("a", null, null)] })), null);
});

test("layoutTimeline: an unknown item id is labelled ?", () => {
  const t = layoutTimeline(
    report({
      rows: rowsAB,
      segments: [seg("zz", "09:00", "09:30")],
      markers: [marker("removed", "09:10", { itemId: "zz" })],
    }),
  );
  assert.ok(t);
  assert.equal(t.actual.bars[0].title, "?");
  assert.equal(t.markers[0].title, "?");
});

test("markerLabel labels every kind", () => {
  const m = (kind: Marker["kind"], secs: number | null = null) => markerLabel({ kind, secs });
  assert.equal(m("extended", 300), "Extended +05:00");
  assert.equal(m("extended", 3725), "Extended +1:02:05");
  assert.equal(m("inserted"), "Inserted");
  assert.equal(m("switched"), "Switched");
  assert.equal(m("removed"), "Removed");
  assert.equal(m("carriedOver"), "Carried over");
  assert.equal(m("recovered"), "Recovered");
  assert.equal(m("calendarChanged"), "Calendar changed");
});

test("hourTicks returns each local whole hour in the span", () => {
  assert.deepEqual(hourTicks(ms("08:30"), ms("10:30"), 0), [ms("09:00"), ms("10:00")]);
  assert.deepEqual(hourTicks(ms("09:00"), ms("10:00"), 0), [ms("09:00"), ms("10:00")]);
  // +05:30: local whole hours fall on UTC half hours.
  assert.deepEqual(hourTicks(ms("08:15"), ms("10:45"), 330), [
    ms("08:30"),
    ms("09:30"),
    ms("10:30"),
  ]);
  assert.deepEqual(hourTicks(ms("09:00"), ms("09:20"), 0), [ms("09:00")]);
  assert.deepEqual(hourTicks(ms("09:31"), ms("10:29"), 330), []);
});
