// The view rows of the day-plan matrix, as pure functions (src/plan.ts).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  addCommand,
  editCommand,
  flexibleNeighbours,
  mergeSnapshot,
  minutesToSecs,
  pinCommand,
  pinInstant,
  rowActions,
  rowTime,
  type Item,
  type Snapshot,
} from "../src/plan.ts";
import { parseInstantNs } from "../src/elapsed.ts";

function item(id: string, extra: Partial<Item> = {}): Item {
  return {
    id,
    title: id,
    plannedSecs: 1500,
    fixed: false,
    pinnedStart: null,
    status: "pending",
    actualSecs: 0,
    ...extra,
  };
}

const F = item("F", { fixed: true, pinnedStart: "2026-11-11T07:00:00Z" });
const A = item("A");
const B = item("B");

function snap(rev: number, items: Item[] = [F, A, B], running: Snapshot["running"] = null): Snapshot {
  return { rev, day: { date: "2026-11-11", tz: "Europe/Berlin" }, items, running, status: [] };
}

test("merge keeps the newer rev, whatever the arrival order", () => {
  const s1 = snap(1);
  const s3 = snap(3);
  assert.equal(mergeSnapshot(null, s1), s1);
  assert.equal(mergeSnapshot(s1, s3), s3);
  assert.equal(mergeSnapshot(s3, s1), s3, "an older event arriving later is ignored");
  assert.equal(mergeSnapshot(s3, snap(3)), s3);
});

test("add sends title and minutes as seconds; blank or 0 is sent as is", () => {
  assert.deepEqual(addCommand("Write", "25"), { name: "createTask", args: ["Write", 1500] });
  assert.deepEqual(addCommand("", ""), { name: "createTask", args: ["", 0] });
  assert.deepEqual(addCommand("x", "0"), { name: "createTask", args: ["x", 0] });
  assert.equal(minutesToSecs("1.5"), 90);
  assert.equal(minutesToSecs("abc"), 0);
});

test("edit sends null for unchanged fields", () => {
  assert.deepEqual(editCommand(A, "New", "25"), { name: "editTask", args: ["A", "New", null] });
  assert.deepEqual(editCommand(A, "A", "30"), { name: "editTask", args: ["A", null, 1800] });
  assert.equal(editCommand(A, "A", "25"), null);
});

test("up and down target the flexible neighbours with the rendered rev", () => {
  const s = snap(7);
  assert.deepEqual(rowActions(s, B).up, {
    name: "reorderTask",
    args: ["B", { place: "before", itemId: "A" }, 7],
  });
  assert.deepEqual(rowActions(s, A).down, {
    name: "reorderTask",
    args: ["A", { place: "after", itemId: "B" }, 7],
  });
  assert.equal(rowActions(s, A).up, null, "no flexible item above A");
  assert.equal(rowActions(s, B).down, null, "no flexible item below B");
  assert.equal(rowActions(s, F).up, null, "no Up on fixed items");
  assert.equal(rowActions(s, F).down, null, "no Down on fixed items");
  assert.deepEqual(flexibleNeighbours([F, A, B], "A"), { prev: null, next: B });
});

test("pin builds the local instant on the day's date; disabled without a time", () => {
  const s = snap(4);
  const expected = new Date(2026, 10, 11, 9, 30).toISOString();
  assert.equal(pinInstant("2026-11-11", "09:30"), expected);
  assert.deepEqual(pinCommand(s, A, "09:30"), { name: "toggleFixed", args: ["A", expected, 4] });
  assert.equal(pinCommand(s, A, ""), null);
  assert.equal(pinCommand(s, F, "09:30"), null, "fixed items unpin instead");
  assert.equal(rowActions(s, A).canPin, true);
  assert.equal(rowActions(s, F).canPin, false);
});

test("unpin sends null with the rendered rev", () => {
  assert.deepEqual(rowActions(snap(5), F).unpin, { name: "toggleFixed", args: ["F", null, 5] });
  assert.equal(rowActions(snap(5), A).unpin, null);
});

test("start for pending or paused, pause for running", () => {
  const paused = item("A", { status: "paused" });
  const running = item("A", { status: "running" });
  assert.deepEqual(rowActions(snap(2), A).start, { name: "startTask", args: ["A", 2] });
  assert.deepEqual(rowActions(snap(2, [paused]), paused).start, { name: "startTask", args: ["A", 2] });
  assert.equal(rowActions(snap(2), A).pause, null);
  assert.deepEqual(rowActions(snap(2, [running]), running).pause, { name: "pauseTask", args: ["A"] });
  assert.equal(rowActions(snap(2, [running]), running).start, null);
  const done = item("A", { status: "done" });
  assert.equal(rowActions(snap(2, [done]), done).start, null);
});

test("switch on the running row only, with the rendered rev", () => {
  const running = item("A", { status: "running" });
  assert.deepEqual(rowActions(snap(6, [running, B]), running).switch, {
    name: "switchTask",
    args: ["A", 6],
  });
  for (const status of ["pending", "paused", "done"] as const) {
    const other = item("A", { status });
    assert.equal(rowActions(snap(6, [other]), other).switch, null, status);
  }
});

test("remove on non-running items only", () => {
  const running = item("A", { status: "running" });
  assert.deepEqual(rowActions(snap(1), A).remove, { name: "removeTask", args: ["A"] });
  assert.equal(rowActions(snap(1, [running]), running).remove, null);
});

test("row time ticks for the running item and shows banked time otherwise", () => {
  const a = item("A", { status: "running", actualSecs: 60 });
  const b = item("B", { actualSecs: 3600 });
  const s = snap(1, [a, b], {
    itemId: "A",
    title: "A",
    segmentStartedAt: "2026-11-11T09:00:00Z",
    accumulatedSecs: 60,
  });
  assert.equal(rowTime(s, a, parseInstantNs("2026-11-11T09:00:05.9Z")), "01:05");
  assert.equal(rowTime(s, a, parseInstantNs("2026-11-11T09:00:06Z")), "01:06");
  assert.equal(rowTime(s, b, parseInstantNs("2026-11-11T09:00:06Z")), "1:00:00");
});
