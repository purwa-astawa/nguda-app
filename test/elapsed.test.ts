// Rust/TS conformance (AD-11): the same fixture as `nguda_core::elapsed` tests.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { elapsedSecs, formatElapsed, parseInstantNs } from "../src/elapsed.ts";

type Case = {
  name: string;
  segmentStartedAt: string;
  accumulatedSecs: number;
  now: string;
  elapsedSecs: number;
  formatted: string;
};

const cases: Case[] = JSON.parse(
  readFileSync(new URL("../fixtures/elapsed.json", import.meta.url), "utf8"),
);

test("fixture has enough cases", () => {
  assert.ok(cases.length >= 12);
});

for (const c of cases) {
  test(`conformance: ${c.name}`, () => {
    const secs = elapsedSecs(c, parseInstantNs(c.now));
    assert.equal(secs, c.elapsedSecs);
    assert.equal(formatElapsed(secs), c.formatted);
  });
}

test("parseInstantNs keeps sub-millisecond digits and offsets", () => {
  assert.equal(parseInstantNs("1970-01-01T00:00:01.5Z"), 1_500_000_000n);
  assert.equal(parseInstantNs("1970-01-01T00:00:00.000000001Z"), 1n);
  assert.equal(parseInstantNs("1970-01-01T01:00:00+01:00"), 0n);
  assert.throws(() => parseInstantNs("2026-11-11"));
  assert.throws(() => parseInstantNs("2026-11-11T09:30:00"));
});
