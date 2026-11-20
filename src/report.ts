// Pure view helpers for the day summary (AD-5). They only format what the engine's
// `queryDay` report says: planned values, sources and differences all come from
// `nguda_core::report`, and nothing here recomputes a plan rule.
// Erasable TS only, so `node --test` runs this file directly.

import type { DayReportView_Serialize, ReportRowView_Serialize } from "./bindings.ts";
import { formatElapsed } from "./elapsed.ts";

export type DayReport = DayReportView_Serialize;
export type ReportRow = ReportRowView_Serialize;

/** How a row is marked next to its title. */
export type RowMark = "unplanned" | "removed" | "carried over" | "carried in";

/** A duration like the tray shows it (`MM:SS`, `H:MM:SS`), or `—` for none. */
export function formatSecs(secs: number | null): string {
  return secs === null ? "—" : formatElapsed(Math.max(0, secs));
}

/** A signed difference: `+MM:SS` over, `−MM:SS` under, `00:00` on plan, `—` for none. */
export function formatDiff(secs: number | null): string {
  if (secs === null) return "—";
  if (secs === 0) return formatElapsed(0);
  return `${secs > 0 ? "+" : "−"}${formatElapsed(Math.abs(secs))}`;
}

/** The marks a row carries: no planned value on a locked day, removed, carried
 * over to tomorrow, and carried in from an earlier day. */
export function rowMarks(row: ReportRow): RowMark[] {
  const marks: RowMark[] = [];
  if (row.plannedSource === "unplanned") marks.push("unplanned");
  if (row.status === "removed") marks.push("removed");
  if (row.status === "carriedOver") marks.push("carried over");
  if (row.carriedFrom !== null) marks.push("carried in");
  return marks;
}

/** Whether "planned" is the current plan rather than a baseline: the day is not
 * locked (undecided or decided against locking). */
export function isUnlocked(report: DayReport): boolean {
  return report.lockDecision !== "locked";
}

/** The CSS class for a difference: over plan, under plan, or neither. */
export function diffClass(secs: number | null): "over" | "under" | "" {
  if (secs === null || secs === 0) return "";
  return secs > 0 ? "over" : "under";
}

// ---- Day timeline (story 3.4) ----
// The lanes are laid out from the report as given: baseline slots exactly as frozen,
// segments as recorded, markers as logged. Nothing here schedules anything (AD-5).
// Output is timezone-free epoch ms plus fractions of one shared span.

type MarkerView = DayReport["markers"][number];
type MarkerKind = MarkerView["kind"];

/** One bar of a lane: an item's span, positioned as fractions of the timeline span. */
export type TimelineBar = {
  itemId: string;
  title: string;
  startMs: number;
  endMs: number;
  /** Fraction of the span, in [0, 1]. */
  left: number;
  /** Fraction of the span, in [0, 1]. */
  width: number;
  /** Sub-row within the lane, from 0. */
  row: number;
  /** Still running: ends at the engine's `now`. */
  open: boolean;
};

/** A lane of bars; overlapping bars sit on separate sub-rows. */
export type TimelineLane = { bars: TimelineBar[]; rows: number };

/** A labelled day-log entry on the shared axis. */
export type TimelineMarker = {
  kind: MarkerKind;
  label: string;
  title: string;
  atMs: number;
  left: number;
};

/** The day's shape: planned (baseline) and actual lanes on one axis, plus markers. */
export type Timeline = {
  startMs: number;
  endMs: number;
  baseline: TimelineLane;
  actual: TimelineLane;
  /** No baseline to draw: the day is not locked. */
  unlocked: boolean;
  markers: TimelineMarker[];
};

const HOUR_MS = 3_600_000;

/** A marker's label by kind; `extended` carries the added time. */
export function markerLabel(marker: Pick<MarkerView, "kind" | "secs">): string {
  switch (marker.kind) {
    case "extended":
      return `Extended +${formatElapsed(Math.max(0, marker.secs ?? 0))}`;
    case "inserted":
      return "Inserted";
    case "switched":
      return "Switched";
    case "removed":
      return "Removed";
    case "carriedOver":
      return "Carried over";
    case "recovered":
      return "Recovered";
  }
}

/** Epoch ms of each local whole hour in [startMs, endMs], local = UTC + offsetMinutes. */
export function hourTicks(startMs: number, endMs: number, offsetMinutes: number): number[] {
  const off = offsetMinutes * 60_000;
  const ticks: number[] = [];
  for (let t = Math.ceil((startMs + off) / HOUR_MS) * HOUR_MS - off; t <= endMs; t += HOUR_MS) {
    ticks.push(t);
  }
  return ticks;
}

type Span = { itemId: string; startMs: number; endMs: number; open: boolean };

/** Positions spans on a lane, first-fit in start order so no sub-row overlaps. */
function layLane(
  spans: Span[],
  startMs: number,
  spanMs: number,
  titleOf: (id: string) => string,
): TimelineLane {
  const frac = (ms: number) => Math.min(1, Math.max(0, (ms - startMs) / spanMs));
  const rowEnds: number[] = [];
  const bars = [...spans]
    .sort((a, b) => a.startMs - b.startMs)
    .map((s): TimelineBar => {
      let row = rowEnds.findIndex((end) => end <= s.startMs);
      if (row === -1) row = rowEnds.length;
      rowEnds[row] = Math.max(s.startMs, s.endMs);
      const left = frac(s.startMs);
      return {
        itemId: s.itemId,
        title: titleOf(s.itemId),
        startMs: s.startMs,
        endMs: s.endMs,
        left,
        width: Math.max(0, Math.min(1 - left, (s.endMs - s.startMs) / spanMs)),
        row,
        open: s.open,
      };
    });
  return { bars, rows: rowEnds.length };
}

/** Lays out the report's baseline, segments and markers on one time axis, or
 * `null` when there is nothing with a time to draw. */
export function layoutTimeline(report: DayReport): Timeline | null {
  const titles = new Map(report.rows.map((r) => [r.itemId, r.title]));
  const titleOf = (id: string) => titles.get(id) ?? "?";

  const planned: Span[] = [];
  for (const slot of report.baseline) {
    if (slot.scheduledStart === null || slot.scheduledEnd === null) continue;
    planned.push({
      itemId: slot.itemId,
      startMs: Date.parse(slot.scheduledStart),
      endMs: Date.parse(slot.scheduledEnd),
      open: false,
    });
  }
  const actual: Span[] = report.segments.map((s) => ({
    itemId: s.itemId,
    startMs: Date.parse(s.start),
    endMs: Date.parse(s.end),
    open: s.open,
  }));
  const marks = report.markers.map((m) => ({ m, atMs: Date.parse(m.at) }));

  const instants = [
    ...planned.flatMap((s) => [s.startMs, s.endMs]),
    ...actual.flatMap((s) => [s.startMs, s.endMs]),
    ...marks.map((x) => x.atMs),
  ].filter((ms) => !Number.isNaN(ms));
  if (instants.length === 0) return null;

  const startMs = Math.min(...instants);
  const endMs = Math.max(startMs + HOUR_MS, Math.max(...instants));
  const spanMs = endMs - startMs;

  return {
    startMs,
    endMs,
    baseline: layLane(planned, startMs, spanMs, titleOf),
    actual: layLane(actual, startMs, spanMs, titleOf),
    unlocked: isUnlocked(report),
    markers: marks.map(({ m, atMs }) => ({
      kind: m.kind,
      label: markerLabel(m),
      title: titleOf(m.itemId),
      atMs,
      left: Math.min(1, Math.max(0, (atMs - startMs) / spanMs)),
    })),
  };
}
