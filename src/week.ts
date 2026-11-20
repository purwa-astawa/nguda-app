// Pure view helpers for the week view (AD-5). They only format what the engine's
// `queryWeek` report says: every sum and ratio comes from `nguda_core::gap`, and
// nothing here recomputes a rule.
// Erasable TS only, so `node --test` runs this file directly.

import type { GapView_Serialize, WeekReportView_Serialize } from "./bindings.ts";

export type WeekReport = WeekReportView_Serialize;
export type Gap = GapView_Serialize;

/** A gap ratio to one decimal, like `1.6×`, or `—` for none. */
export function formatRatio(ratio: number | null): string {
  return ratio === null ? "—" : `${ratio.toFixed(1)}×`;
}

/** Whether `date` (`YYYY-MM-DD`) is one of the report's dates, Monday to Sunday.
 * `YYYY-MM-DD` strings sort like the dates they name. */
export function containsDate(report: Pick<WeekReport, "start" | "end">, date: string): boolean {
  return report.start <= date && date <= report.end;
}
