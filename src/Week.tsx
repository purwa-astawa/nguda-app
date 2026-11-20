import { useEffect, useRef, useState } from "react";
import { commands } from "./bindings";
import { formatSecs } from "./report";
import { containsDate, formatRatio, type Gap, type WeekReport } from "./week.ts";

/** The last reply, with the date it answers. */
type Loaded = { date: string } & (
  { kind: "error"; message: string } | { kind: "report"; report: WeekReport }
);

type Props = {
  /** A date in the week shown, `YYYY-MM-DD`. */
  date: string;
  /** The snapshot's today, never the browser clock (AD-5). */
  today: string;
  /** The snapshot rev: while the shown week holds today, a new rev asks again. */
  rev: number;
  /** A date in the week to show; `null` for the week holding today. */
  onDate: (date: string | null) => void;
};

function Counts({ gap }: { gap: Gap }) {
  return (
    <>
      <td className="num">{gap.items}</td>
      <td className="num">{formatSecs(gap.actualSecs)}</td>
      <td className="num">{formatSecs(gap.plannedSecs)}</td>
    </>
  );
}

// The estimate gap of one ISO week (AD-6, AD-16). Every figure and every date
// comes from the engine's `queryWeek` report; the view only formats it and
// navigates with the dates it returns.
export function Week({ date, today, rev, onDate }: Props) {
  const [reply, setReply] = useState<Loaded | null>(null);
  // Only the newest request may set the view; a late reply for an older one is dropped.
  const latest = useRef(0);

  // A reply for another date is never shown under this one: that is still loading.
  const loaded = reply && reply.date === date ? reply : null;
  const report = loaded?.kind === "report" ? loaded.report : null;
  const holdsToday = date === today || (report !== null && containsDate(report, today));
  const liveRev = holdsToday ? rev : -1;

  useEffect(() => {
    const mine = ++latest.current;
    commands
      .queryWeek(date)
      .then((res) => {
        if (mine !== latest.current) return;
        if (res.status === "error") setReply({ date, kind: "error", message: res.error });
        else setReply({ date, kind: "report", report: res.data });
      })
      .catch(
        (e) => mine === latest.current && setReply({ date, kind: "error", message: String(e) }),
      );
  }, [date, liveRev]);

  return (
    <section className="summary week">
      <button
        type="button"
        disabled={!report?.previousStart}
        onClick={() => report?.previousStart && onDate(report.previousStart)}
      >
        Previous
      </button>
      <button
        type="button"
        disabled={!report?.nextStart}
        onClick={() => report?.nextStart && onDate(report.nextStart)}
      >
        Next
      </button>
      {!holdsToday && (
        <button type="button" onClick={() => onDate(null)}>
          This week
        </button>
      )}

      {!loaded ? (
        <p>Loading…</p>
      ) : loaded.kind === "error" ? (
        <p className="status" role="alert">
          {loaded.message}
        </p>
      ) : null}

      {report && (
        <>
          <h2>
            Week {report.week}, {report.year}{" "}
            <small>
              {report.start} – {report.end}
            </small>
          </h2>

          {report.overall.ratio === null ? (
            <p>
              {report.countedDays.length === 0
                ? "No locked days this week."
                : "No baseline time on this week's locked days."}
            </p>
          ) : (
            <p className="gap">
              Estimate gap <strong>{formatRatio(report.overall.ratio)}</strong>{" "}
              <span className="counts">
                {report.overall.items} {report.overall.items === 1 ? "item" : "items"} · actual{" "}
                {formatSecs(report.overall.actualSecs)} · planned{" "}
                {formatSecs(report.overall.plannedSecs)}
              </span>
            </p>
          )}

          {report.tags.length > 0 && (
            <table className="report">
              <thead>
                <tr>
                  <th scope="col">Tag</th>
                  <th scope="col">Gap</th>
                  <th scope="col">Items</th>
                  <th scope="col">Actual</th>
                  <th scope="col">Planned</th>
                </tr>
              </thead>
              <tbody>
                {report.tags.map((t) => (
                  <tr key={t.tag}>
                    <td>
                      <span className="tag">{t.tag}</span>
                    </td>
                    <td className="num">{formatRatio(t.gap.ratio)}</td>
                    <Counts gap={t.gap} />
                  </tr>
                ))}
              </tbody>
            </table>
          )}

          {report.unlockedDays.length > 0 && (
            <p className="excluded">
              Excluded as unlocked: {report.unlockedDays.join(", ")}
            </p>
          )}
        </>
      )}
    </section>
  );
}
