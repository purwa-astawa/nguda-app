import { useEffect, useRef, useState } from "react";
import { commands } from "./bindings";
import { diffClass, formatDiff, formatSecs, isUnlocked, rowMarks, type DayReport } from "./report";

/** The last reply, with the date it answers. */
type Loaded = { date: string } & (
  { kind: "error"; message: string } | { kind: "none" } | { kind: "report"; report: DayReport }
);

type Props = {
  /** The date shown, `YYYY-MM-DD`; empty while the input is cleared. */
  date: string;
  /** The snapshot's today, never the browser clock (AD-5). */
  today: string;
  /** The snapshot rev: while today is shown, a new rev asks again. */
  rev: number;
  /** Bumped each time the summary is opened from the tray. */
  opened: number;
  onDate: (date: string) => void;
};

// The planned-versus-actual table of one day (AD-16). Every figure comes from the
// engine's `queryDay` report; the view only formats it.
export function Summary({ date, today, rev, opened, onDate }: Props) {
  const [reply, setReply] = useState<Loaded | null>(null);
  // Only the newest request may set the table; a late reply for an older one is dropped.
  const latest = useRef(0);
  const liveRev = date === today ? rev : -1;

  useEffect(() => {
    if (!date) return;
    const mine = ++latest.current;
    commands
      .queryDay(date)
      .then((res) => {
        if (mine !== latest.current) return;
        if (res.status === "error") setReply({ date, kind: "error", message: res.error });
        else if (res.data === null) setReply({ date, kind: "none" });
        else setReply({ date, kind: "report", report: res.data });
      })
      .catch(
        (e) => mine === latest.current && setReply({ date, kind: "error", message: String(e) }),
      );
  }, [date, liveRev, opened]);

  // A reply for another date is never shown under this one: that is still loading.
  const loaded = reply && reply.date === date ? reply : null;
  const report = loaded?.kind === "report" ? loaded.report : null;

  return (
    <section className="summary">
      <label>
        Day{" "}
        <input
          aria-label="Summary date"
          type="date"
          value={date}
          onChange={(e) => onDate(e.target.value)}
        />
      </label>
      {date !== today && (
        <button type="button" onClick={() => onDate(today)}>
          Today
        </button>
      )}
      {report && isUnlocked(report) && <span className="badge unlocked">unlocked</span>}

      {!date ? (
        <p>Pick a day.</p>
      ) : !loaded ? (
        <p>Loading…</p>
      ) : loaded.kind === "error" ? (
        <p className="status" role="alert">
          {loaded.message}
        </p>
      ) : loaded.kind === "none" ? (
        <p>No data for this day.</p>
      ) : !report ? null : report.rows.length === 0 ? (
        <p>No tasks on this day.</p>
      ) : (
        <table className="report">
          <thead>
            <tr>
              <th scope="col">Title</th>
              <th scope="col">Planned</th>
              <th scope="col">Actual</th>
              <th scope="col">Difference</th>
            </tr>
          </thead>
          <tbody>
            {report.rows.map((row) => (
              <tr key={row.itemId} className={row.status}>
                <td>
                  {row.title}
                  {rowMarks(row).map((m) => (
                    <span key={m} className="mark">
                      {m}
                    </span>
                  ))}
                </td>
                <td className="num">{formatSecs(row.plannedSecs)}</td>
                <td className="num">{formatSecs(row.actualSecs)}</td>
                <td className={`num ${diffClass(row.differenceSecs)}`}>
                  {formatDiff(row.differenceSecs)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
