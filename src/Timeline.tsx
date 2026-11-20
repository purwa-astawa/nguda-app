import { hourTicks, layoutTimeline, type DayReport, type TimelineLane } from "./report";

/** Height of one lane sub-row, in rem. */
const ROW_REM = 1.5;

/** Height of one marker-label row, in rem: each label gets its own row. */
const MARKER_REM = 1.2;

const pct = (f: number) => `${f * 100}%`;

/** Grid placement in the track column. Every child is placed explicitly so the
 * marker-line overlay can share cells with the lanes. */
const TRACK = (gridRow: number) => ({ gridRow, gridColumn: 2 });

/** Local `HH:MM` of an epoch-ms instant (display only). */
function clock(ms: number): string {
  const d = new Date(ms);
  const two = (n: number) => String(n).padStart(2, "0");
  return `${two(d.getHours())}:${two(d.getMinutes())}`;
}

type LaneProps = { lane: TimelineLane; kind: "planned" | "actual"; gridRow: number };

function Lane({ lane, kind, gridRow }: LaneProps) {
  const height = `${Math.max(1, lane.rows) * ROW_REM}rem`;
  return (
    <div className={`tl-lane ${kind}`} style={{ ...TRACK(gridRow), height }}>
      {lane.bars.map((bar, i) => (
        <div
          key={`${bar.itemId}-${bar.startMs}-${i}`}
          className={`tl-bar${bar.open ? " open" : ""}`}
          style={{ left: pct(bar.left), width: pct(bar.width), top: `${bar.row * ROW_REM}rem` }}
          title={`${bar.title} ${clock(bar.startMs)}–${clock(bar.endMs)}${bar.open ? " (running)" : ""}`}
        >
          {bar.title}
        </div>
      ))}
    </div>
  );
}

// The day's shape (CAP-7): the frozen baseline and the actual segments on one
// time axis, with the day-log markers. Laid out by `layoutTimeline`; drawn here.
export function Timeline({ report }: { report: DayReport }) {
  const t = layoutTimeline(report);
  if (!t) return null;
  const span = t.endMs - t.startMs;
  // Labels go one per row in time order, so close markers never overprint.
  const markers = [...t.markers].sort((a, b) => a.atMs - b.atMs);
  const ticks = hourTicks(t.startMs, t.endMs, -new Date(t.startMs).getTimezoneOffset());

  return (
    <section className="timeline" aria-label="Day timeline">
      <div className="tl-ticks" style={TRACK(1)}>
        {ticks.map((ms) => (
          <span
            key={ms}
            className="tl-tick"
            style={{
              left: pct((ms - t.startMs) / span),
              transform: `translateX(-${((ms - t.startMs) / span) * 100}%)`,
            }}
          >
            {clock(ms)}
          </span>
        ))}
      </div>

      <span className="tl-name" style={{ gridRow: 2 }}>
        Planned
      </span>
      {t.unlocked ? (
        <div className="tl-lane planned" style={{ ...TRACK(2), height: `${ROW_REM}rem` }}>
          <span className="tl-unlocked">unlocked: no baseline</span>
        </div>
      ) : (
        <Lane lane={t.baseline} kind="planned" gridRow={2} />
      )}

      <span className="tl-name" style={{ gridRow: 3 }}>
        Actual
      </span>
      <Lane lane={t.actual} kind="actual" gridRow={3} />

      <span className="tl-name" style={{ gridRow: 4 }}>
        Changes
      </span>
      <div
        className="tl-markers"
        style={{ ...TRACK(4), height: `${markers.length * MARKER_REM}rem` }}
      >
        {markers.map((m, i) => (
          <span
            key={`${m.kind}-${m.atMs}-${i}`}
            className={`tl-marker ${m.kind}`}
            style={{
              left: pct(m.left),
              top: `${i * MARKER_REM}rem`,
              transform: `translateX(-${m.left * 100}%)`,
            }}
            title={`${m.label} · ${m.title} · ${clock(m.atMs)}`}
          >
            {m.label}
          </span>
        ))}
      </div>

      <div className="tl-lines" aria-hidden="true">
        {t.markers.map((m, i) => (
          <span
            key={`${m.kind}-${m.atMs}-${i}`}
            className="tl-line"
            style={{ left: pct(m.left) }}
          />
        ))}
      </div>
    </section>
  );
}
