// The TypeScript twin of `nguda_core::{elapsed_secs, format_elapsed}` (AD-11).
// It must agree with Rust bit for bit; `fixtures/elapsed.json` is checked by both
// `cargo test` and `npm test`. Erasable TS only, so Node's type stripping runs it.

/** The parts of a running item the formula needs. */
export type RunningTime = {
  segmentStartedAt: string;
  accumulatedSecs: number;
};

const NS_PER_SEC = 1_000_000_000n;
const INSTANT = /^(.+T\d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:\d{2})$/i;

/** Parses an RFC 3339 instant to epoch nanoseconds, keeping sub-millisecond digits. */
export function parseInstantNs(s: string): bigint {
  const m = INSTANT.exec(s);
  if (!m) throw new Error(`invalid RFC 3339 instant: ${s}`);
  const [, base, fraction = "", offset] = m;
  const ms = Date.parse(base + offset);
  if (Number.isNaN(ms)) throw new Error(`invalid RFC 3339 instant: ${s}`);
  // `base` has whole seconds only, so `ms` is a multiple of 1000.
  return BigInt(ms) * 1_000_000n + BigInt(fraction.padEnd(9, "0"));
}

/** The current wall-clock time in epoch nanoseconds (millisecond resolution). */
export function nowNs(): bigint {
  return BigInt(Date.now()) * 1_000_000n;
}

/**
 * Whole seconds spent on `running` as of `nowNs`. The open segment is truncated
 * toward zero, and a clock behind the segment start counts it as zero.
 */
export function elapsedSecs(running: RunningTime, nowNs: bigint): number {
  const open = (nowNs - parseInstantNs(running.segmentStartedAt)) / NS_PER_SEC;
  return running.accumulatedSecs + Number(open > 0n ? open : 0n);
}

/** `MM:SS` below one hour, `H:MM:SS` from one hour on. */
export function formatElapsed(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const two = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${two(m)}:${two(s)}`;
}
