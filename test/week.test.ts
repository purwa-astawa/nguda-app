// The week view's pure helpers (src/week.ts).
import { test } from "node:test";
import assert from "node:assert/strict";
import { containsDate, exportMessage, formatRatio } from "../src/week.ts";

test("formatRatio shows one decimal and a times sign, or a dash for none", () => {
  assert.equal(formatRatio(1.6), "1.6×");
  assert.equal(formatRatio(1), "1.0×");
  assert.equal(formatRatio(0.25), "0.3×");
  assert.equal(formatRatio(0), "0.0×");
  assert.equal(formatRatio(null), "—");
});

test("containsDate is inclusive on both ends and works across a year edge", () => {
  const week = { start: "2026-12-28", end: "2027-01-03" };
  assert.equal(containsDate(week, "2026-12-28"), true);
  assert.equal(containsDate(week, "2027-01-01"), true);
  assert.equal(containsDate(week, "2027-01-03"), true);
  assert.equal(containsDate(week, "2026-12-27"), false);
  assert.equal(containsDate(week, "2027-01-04"), false);
});

test("exportMessage counts rows in the singular or the plural", () => {
  assert.equal(exportMessage(1), "Exported 1 row.");
  assert.equal(exportMessage(0), "Exported 0 rows.");
  assert.equal(exportMessage(12), "Exported 12 rows.");
});
