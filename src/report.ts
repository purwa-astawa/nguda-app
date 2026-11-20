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
