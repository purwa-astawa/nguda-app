// The view rows of the day-plan matrix, as pure functions (src/plan.ts).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  addCommand,
  editCommand,
  extendCommand,
  flexibleNeighbours,
  insertCommand,
  insertPositions,
  mergeSnapshot,
  minutesToSecs,
  overflowItems,
  pinCommand,
  pinInstant,
  rowActions,
  rowTime,
  scheduleLabel,
  workEndCommand,
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
    scheduledStart: null,
    scheduledEnd: null,
    interrupted: false,
    ...extra,
  };
}

const F = item("F", { fixed: true, pinnedStart: "2026-11-11T07:00:00Z" });
const A = item("A");
const B = item("B");

function snap(rev: number, items: Item[] = [F, A, B], running: Snapshot["running"] = null): Snapshot {
  return {
    rev,
    day: { date: "2026-11-11", tz: "Europe/Berlin" },
    items,
    running,
    workEnd: "17:00",
    overflow: [],
    status: [],
  };
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

test("schedule label shows start and end as local HH:MM in the day's zone", () => {
  const a = item("A", {
    scheduledStart: "2026-11-11T08:00:00Z",
    scheduledEnd: "2026-11-11T08:25:30Z",
  });
  assert.equal(scheduleLabel(a, "Europe/Berlin"), "09:00\u201309:25");
  assert.equal(scheduleLabel(a, "UTC"), "08:00\u201308:25");
  assert.equal(scheduleLabel(a, "Asia/Tokyo"), "17:00\u201317:25");
  const late = item("L", {
    scheduledStart: "2026-11-11T23:30:00Z",
    scheduledEnd: "2026-11-12T00:10:00Z",
  });
  assert.equal(scheduleLabel(late, "UTC"), "23:30\u201300:10");
  assert.equal(scheduleLabel(item("C"), "UTC"), null, "no span, no label");
});

test("up and down skip done and running neighbours, which are not in order-key order", () => {
  const C = item("C", { status: "done" });
  const R = item("R", { status: "running" });
  const s = snap(3, [C, A, R, B]);
  assert.equal(rowActions(s, A).up, null, "the done C is not a neighbour");
  assert.deepEqual(rowActions(s, A).down, {
    name: "reorderTask",
    args: ["A", { place: "after", itemId: "B" }, 3],
  });
  assert.deepEqual(rowActions(s, B).up, {
    name: "reorderTask",
    args: ["B", { place: "before", itemId: "A" }, 3],
  });
  assert.deepEqual(flexibleNeighbours([C, A, R, B], "R"), { prev: null, next: null });
  assert.deepEqual(flexibleNeighbours([C, A, R, B], "C"), { prev: null, next: null });
});

test("up and down skip interrupted items, and an interrupted row has neither", () => {
  // The window shows N, Z, B, C: stacked Z flows first whatever its key.
  const N = item("N", { status: "running" });
  const Z = item("Z", { status: "paused", interrupted: true });
  const C = item("C");
  const s = snap(6, [N, Z, B, C]);
  assert.equal(rowActions(s, B).up, null, "the interrupted Z is not a neighbour");
  assert.deepEqual(rowActions(s, B).down, {
    name: "reorderTask",
    args: ["B", { place: "after", itemId: "C" }, 6],
  });
  assert.deepEqual(rowActions(s, C).up, {
    name: "reorderTask",
    args: ["C", { place: "before", itemId: "B" }, 6],
  });
  assert.equal(rowActions(s, Z).up, null, "no Up on an interrupted row");
  assert.equal(rowActions(s, Z).down, null, "no Down on an interrupted row");
  assert.deepEqual(flexibleNeighbours([N, Z, B, C], "Z"), { prev: null, next: null });
});

test("extendCommand extends only the running item, by whole minutes, at the rendered rev", () => {
  const R = item("R", { status: "running" });
  const s = snap(7, [F, R, B]);
  assert.deepEqual(extendCommand(s, R, "5"), { name: "extendTask", args: ["R", 300, 7] });
  assert.deepEqual(extendCommand(s, R, " 15 "), { name: "extendTask", args: ["R", 900, 7] });
  for (const status of ["pending", "paused", "done", "carriedOver"] as const) {
    assert.equal(extendCommand(s, item("X", { status }), "5"), null, status);
  }
  for (const bad of ["", "0", "-5", "abc"]) {
    assert.equal(extendCommand(s, R, bad), null, JSON.stringify(bad));
  }
});

test("insert positions: Now and After current only while running, then each pending or paused flexible item, then At end", () => {
  const R = item("R", { status: "running", title: "Run" });
  const P = item("P", { status: "paused", title: "Paused" });
  const D = item("D", { status: "done" });
  const running = { itemId: "R", title: "Run", segmentStartedAt: "2026-11-11T09:00:00Z", accumulatedSecs: 0 };
  const s = snap(4, [D, R, F, P, B], running);
  assert.deepEqual(
    insertPositions(s).map((p) => [p.value, p.label, p.anchor]),
    [
      ["now", "Now", { place: "now", itemId: null }],
      ["after-current", "After current", { place: "after", itemId: "R" }],
      ["before:P", "Before \u201cPaused\u201d", { place: "before", itemId: "P" }],
      ["after:P", "After \u201cPaused\u201d", { place: "after", itemId: "P" }],
      ["before:B", "Before \u201cB\u201d", { place: "before", itemId: "B" }],
      ["after:B", "After \u201cB\u201d", { place: "after", itemId: "B" }],
      ["end", "At end", { place: "end", itemId: null }],
    ],
  );
  const idle = insertPositions(snap(4, [F, A]));
  assert.deepEqual(
    idle.map((p) => p.value),
    ["before:A", "after:A", "end"],
    "no Now or After current while idle; fixed F is not an anchor",
  );
  assert.deepEqual(insertPositions(snap(1, [])).map((p) => p.label), ["At end"]);
});

test("insert positions skip interrupted items", () => {
  const N = item("N", { status: "running", title: "N" });
  const A2 = item("A", { status: "paused", interrupted: true });
  const running = { itemId: "N", title: "N", segmentStartedAt: "2026-11-11T09:00:00Z", accumulatedSecs: 0 };
  assert.deepEqual(
    insertPositions(snap(3, [N, A2, B], running)).map((p) => p.label),
    ["Now", "After current", "Before \u201cB\u201d", "After \u201cB\u201d", "At end"],
  );
});

test("insertCommand sends title, seconds, the anchor and the rendered rev", () => {
  const s = snap(9);
  const anchor = { place: "before", itemId: "B" } as const;
  assert.deepEqual(insertCommand(s, "Call", "15", anchor), {
    name: "insertTask",
    args: ["Call", 900, { place: "before", itemId: "B" }, 9],
  });
  assert.deepEqual(insertCommand(s, "", "", { place: "now", itemId: null }), {
    name: "insertTask",
    args: ["", 0, { place: "now", itemId: null }, 9],
  });
});

test("work-end sends HH:MM as minutes after midnight; anything else as 1440", () => {
  assert.deepEqual(workEndCommand("18:00"), { name: "setWorkEnd", args: [1080] });
  assert.deepEqual(workEndCommand("00:00"), { name: "setWorkEnd", args: [0] });
  assert.deepEqual(workEndCommand("23:59"), { name: "setWorkEnd", args: [1439] });
  for (const bad of ["", "7:00", "24:00", "12:60", "18:00:00", "ab:cd"]) {
    assert.deepEqual(workEndCommand(bad), { name: "setWorkEnd", args: [1440] }, bad);
  }
});

test("overflow items follow snap.overflow's order and skip unknown ids", () => {
  const s = { ...snap(4), overflow: ["B", "gone", "A"] };
  assert.deepEqual(
    overflowItems(s).map((i) => i.id),
    ["B", "A"],
  );
  assert.deepEqual(overflowItems(snap(4)), []);
});
