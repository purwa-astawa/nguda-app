import { useEffect, useRef, useState } from "react";
import { events, commands, type CalendarEventView } from "./bindings";
import { nowNs } from "./elapsed";
import {
  addCalendarEventCommand,
  addCommand,
  asksLockChoice,
  carryOverCommand,
  clearGoogleClientCommand,
  googleClientCommand,
  googleSection,
  insertCommand,
  insertPositions,
  lockBadge,
  lockDayCommand,
  mergeSnapshot,
  overflowItems,
  includeCalendarCommand,
  pickerCalendars,
  scheduleLabel,
  signInCommand,
  signOutCommand,
  withLock,
  workEndCommand,
  type Command,
  type LockChoice,
  type Snapshot,
} from "./plan";
import { PlanItemRow } from "./PlanItemRow";
import { Summary } from "./Summary";
import { Week } from "./Week.tsx";
import "./App.css";

/** Sends one command. Resolves to the error text of a malformed-input `Err`. */
async function dispatch(cmd: Command): Promise<string | null> {
  let res: { status: "ok" } | { status: "error"; error: string };
  switch (cmd.name) {
    case "createTask":
      res = await commands.createTask(...cmd.args);
      break;
    case "editTask":
      res = await commands.editTask(...cmd.args);
      break;
    case "reorderTask":
      res = await commands.reorderTask(...cmd.args);
      break;
    case "toggleFixed":
      res = await commands.toggleFixed(...cmd.args);
      break;
    case "removeTask":
      res = await commands.removeTask(...cmd.args);
      break;
    case "startTask":
      res = await commands.startTask(...cmd.args);
      break;
    case "lockDay":
      res = await commands.lockDay(...cmd.args);
      break;
    case "pauseTask":
      res = await commands.pauseTask(...cmd.args);
      break;
    case "switchTask":
      res = await commands.switchTask(...cmd.args);
      break;
    case "extendTask":
      res = await commands.extendTask(...cmd.args);
      break;
    case "setTags":
      res = await commands.setTags(...cmd.args);
      break;
    case "insertTask":
      res = await commands.insertTask(...cmd.args);
      break;
    case "setWorkEnd":
      res = await commands.setWorkEnd(...cmd.args);
      break;
    case "carryOverToTomorrow":
      res = await commands.carryOverToTomorrow(...cmd.args);
      break;
    case "signIn":
      res = await commands.signIn();
      break;
    case "signOut":
      res = await commands.signOut();
      break;
    case "setGoogleClient":
      res = await commands.setGoogleClient(...cmd.args);
      break;
    case "setIncludedCalendars":
      res = await commands.setIncludedCalendars(...cmd.args);
      break;
    case "excludeEvent":
      res = await commands.excludeEvent(...cmd.args);
      break;
    case "includeEvent":
      res = await commands.includeEvent(...cmd.args);
      break;
    case "addCalendarEvent":
      res = await commands.addCalendarEvent(...cmd.args);
      break;
  }
  return res.status === "error" ? res.error : null;
}

/** The picker's last reply, with the calendar it lists. */
type Listed = { calendarId: string } & (
  { kind: "error"; message: string } | { kind: "events"; events: CalendarEventView[] }
);

/** Adds single events of today from a calendar that is not included (R4). The
 * events come from the engine's `queryCalendarEvents`; only the newest request
 * may set the list, and a `"stale"` reply is ignored. */
function EventPicker({ snap, send }: { snap: Snapshot; send: (cmd: Command) => void }) {
  const calendars = pickerCalendars(snap);
  const [chosen, setChosen] = useState("");
  const [listed, setListed] = useState<Listed | null>(null);
  // Events sent to be added since the list was shown: not offered again.
  const [sent, setSent] = useState<ReadonlySet<string>>(new Set());
  const latest = useRef(0);
  const date = snap.day?.date ?? "";
  // A choice that left the list (now included, or gone) falls back to the first.
  const calendarId = calendars.some((c) => c.id === chosen) ? chosen : (calendars[0]?.id ?? "");

  // A new day or calendar drops the old list.
  useEffect(() => {
    latest.current++;
    setListed(null);
    setSent(new Set());
  }, [date, calendarId]);

  if (calendars.length === 0 || !date) return null;

  const show = () => {
    const mine = ++latest.current;
    commands
      .queryCalendarEvents(calendarId, date)
      .then((res) => {
        if (mine !== latest.current) return;
        if (res.status === "error") {
          if (res.error !== "stale") setListed({ calendarId, kind: "error", message: res.error });
        } else {
          setListed({ calendarId, kind: "events", events: res.data });
          setSent(new Set());
        }
      })
      .catch((e) => {
        if (mine === latest.current) setListed({ calendarId, kind: "error", message: String(e) });
      });
  };

  const tz = snap.day?.tz ?? "UTC";

  return (
    <div className="picker">
      <label>
        Add one event from{" "}
        <select value={calendarId} onChange={(e) => setChosen(e.target.value)}>
          {calendars.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      </label>
      <button type="button" onClick={show}>
        Show today's events
      </button>
      {listed?.calendarId === calendarId && listed.kind === "error" && (
        <p className="status" role="alert">
          {listed.message}
        </p>
      )}
      {listed?.calendarId === calendarId && listed.kind === "events" && (
        listed.events.length === 0 ? (
          <p>No timed events today.</p>
        ) : (
          <ul>
            {listed.events.map((e) => (
              <li key={e.eventId}>
                <span className="when">
                  {scheduleLabel({ scheduledStart: e.start, scheduledEnd: e.end }, tz)}
                </span>
                <span className="title">{e.title}</span>
                <button
                  type="button"
                  disabled={e.inPlan || sent.has(e.eventId)}
                  onClick={() => {
                    send(addCalendarEventCommand(e.calendarId, e.eventId));
                    setSent((s) => new Set(s).add(e.eventId));
                  }}
                >
                  {e.inPlan ? "In plan" : sent.has(e.eventId) ? "Added" : "Add"}
                </button>
              </li>
            ))}
          </ul>
        )
      )}
    </div>
  );
}

type Tab = "plan" | "summary" | "week";

const TABS: { value: Tab; label: string }[] = [
  { value: "plan", label: "Plan" },
  { value: "summary", label: "Summary" },
  { value: "week", label: "Week" },
];

// Thin view (AD-5): it keeps only the newest snapshot, the Summary and Week tabs'
// picked dates, and local form state; it shows items in the order given and sends
// every change as a command.
function App() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  const [now, setNow] = useState<bigint>(nowNs);
  const [title, setTitle] = useState("");
  const [minutes, setMinutes] = useState("25");
  const [insertTitle, setInsertTitle] = useState("");
  const [insertMinutes, setInsertMinutes] = useState("15");
  const [insertAt, setInsertAt] = useState("end");
  // The work-end field's draft; `null` shows the snapshot's value.
  const [workEnd, setWorkEnd] = useState<string | null>(null);
  // The "your own OAuth client" form's fields; the secret is never shown back.
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  // Bumped by the tray's or a notification's Extend…; the running row focuses its extend field.
  const [focusExtend, setFocusExtend] = useState(0);
  // A Start or insert-now held back until the user answers "lock the day?" (AD-7).
  const [askingLock, setAskingLock] = useState<Command | null>(null);
  // Runs once the held command is sent (not on Cancel), e.g. to clear its form.
  const [afterAsk, setAfterAsk] = useState<(() => void) | null>(null);
  const [tab, setTab] = useState<Tab>("plan");
  // The summary's picked date; `null` follows the snapshot's today.
  const [summaryDate, setSummaryDate] = useState<string | null>(null);
  // A date in the week view's week; `null` follows the snapshot's today.
  const [weekDate, setWeekDate] = useState<string | null>(null);
  // Bumped by the tray's "Today's summary", so it asks again even when already shown.
  const [summaryOpened, setSummaryOpened] = useState(0);

  useEffect(() => {
    const unlisten = events.openSummaryEvent.listen(() => {
      setTab("summary");
      setSummaryDate(null);
      setSummaryOpened((n) => n + 1);
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    const unlisten = events.focusExtendEvent.listen(() => {
      setTab("plan");
      setFocusExtend((n) => n + 1);
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    // Subscribe first, then fetch, so no snapshot falls between the two. Either
    // may arrive first; the merge keeps the newer rev.
    let alive = true;
    const unlisten = events.snapshotEvent.listen((e) =>
      setSnap((prev) => mergeSnapshot(prev, e.payload)),
    );
    commands
      .currentSnapshot()
      .then((s) => alive && setSnap((prev) => mergeSnapshot(prev, s)))
      .catch((e) => alive && setLocalError(String(e)));
    return () => {
      alive = false;
      unlisten.then((stop) => stop());
    };
  }, []);

  const running = snap?.running != null;
  useEffect(() => {
    if (!running) return;
    setNow(nowNs());
    const id = setInterval(() => setNow(nowNs()), 250);
    return () => clearInterval(id);
  }, [running]);

  const send = async (cmd: Command) => {
    try {
      setLocalError(await dispatch(cmd));
    } catch (e) {
      setLocalError(String(e));
    }
  };

  // On an undecided day, a Start or insert-now asks for the lock choice first and
  // sends nothing until it is answered.
  // `onSent` runs only once the command is actually sent.
  const sendOrAsk = (cmd: Command, onSent?: () => void) => {
    if (snap && asksLockChoice(snap, cmd)) {
      setAskingLock(cmd);
      setAfterAsk(() => onSent ?? null);
    } else {
      send(cmd);
      onSent?.();
    }
  };

  const answerLock = (choice: LockChoice | null) => {
    if (askingLock && choice) {
      send(withLock(askingLock, choice));
      afterAsk?.();
    }
    setAskingLock(null);
    setAfterAsk(null);
  };

  if (!snap) {
    return (
      <main>
        <h1>nguda</h1>
        <p>Loading…</p>
      </main>
    );
  }

  // Positions come from the snapshot; a choice that is gone falls back to the end.
  const positions = insertPositions(snap);
  const position = positions.find((p) => p.value === insertAt) ?? positions[positions.length - 1];
  const lockDay = lockDayCommand(snap);
  const badge = lockBadge(snap);

  // Status entries and local errors, shown on every tab.
  const statusView = (
    <>
      {snap.status.map((s) =>
        s.key === "overflow" ? (
          <div key={s.key} className="status overflow" role="alert">
            <p>{s.message}</p>
            <ul>
              {overflowItems(snap).map((i) => (
                <li key={i.id}>{i.title}</li>
              ))}
            </ul>
            <button type="button" onClick={() => send(carryOverCommand(snap))}>
              Move to tomorrow
            </button>
          </div>
        ) : (
          <p key={s.key} className="status" role="alert">
            {s.message}
          </p>
        ),
      )}
      {localError && (
        <p className="status" role="alert">
          {localError}
        </p>
      )}
    </>
  );

  const google = googleSection(snap);

  // The Plan tab: the day plan and its forms.
  const planView = (
    <>
      <section className="google" aria-label="Google Calendar">
        <span>Google Calendar: {google.label}</span>
        {google.signIn && (
          <button type="button" disabled={google.busy} onClick={() => send(signInCommand())}>
            Sign in
          </button>
        )}
        {google.noClient && <p className="no-client">{google.noClient}</p>}
        {google.signOut && (
          <button type="button" onClick={() => send(signOutCommand())}>
            Sign out
          </button>
        )}
        {google.clientForm && (
          <form
            noValidate
            className="oauth-client"
            aria-label="Your own OAuth client"
            onSubmit={(e) => {
              e.preventDefault();
              send(googleClientCommand(clientId, clientSecret));
              setClientId("");
              setClientSecret("");
            }}
          >
            <span>
              Your own OAuth client
              {google.customId ? `: ${google.customId}` : " (none: using the built-in client)"}
            </span>
            <input
              aria-label="Client ID"
              placeholder="Client ID"
              value={clientId}
              onChange={(e) => setClientId(e.target.value)}
            />
            <input
              aria-label="Client secret (optional)"
              placeholder="Client secret (optional)"
              type="password"
              autoComplete="off"
              value={clientSecret}
              onChange={(e) => setClientSecret(e.target.value)}
            />
            <button type="submit" disabled={!clientId.trim()}>
              Save
            </button>
            <button
              type="button"
              disabled={!google.customId}
              onClick={() => send(clearGoogleClientCommand())}
            >
              Clear
            </button>
          </form>
        )}
        {google.calendars.length > 0 && (
          <fieldset className="calendars">
            <legend>Calendars in today's plan</legend>
            {google.calendars.map((c) => (
              <label key={c.id}>
                <input
                  type="checkbox"
                  checked={c.included}
                  onChange={(e) => send(includeCalendarCommand(snap, c.id, e.target.checked))}
                />
                {c.name}
              </label>
            ))}
          </fieldset>
        )}
        <EventPicker snap={snap} send={send} />
      </section>

      {lockDay && (
        <button type="button" className="lock-day" onClick={() => send(lockDay)}>
          Lock day
        </button>
      )}

      {askingLock && (
        <div className="ask-lock" role="dialog" aria-label="Lock the day?">
          <p>Lock today's plan as the baseline before starting?</p>
          <button type="button" onClick={() => answerLock("lock")}>
            Lock &amp; start
          </button>
          <button type="button" onClick={() => answerLock("noLock")}>
            Start without lock
          </button>
          <button type="button" onClick={() => answerLock(null)}>
            Cancel
          </button>
        </div>
      )}

      <form
        noValidate
        className="add work-end"
        onSubmit={(e) => {
          e.preventDefault();
          send(workEndCommand(workEnd ?? snap.workEnd));
          setWorkEnd(null);
        }}
      >
        <label>
          Work ends{" "}
          <input
            aria-label="Work ends"
            type="time"
            value={workEnd ?? snap.workEnd}
            onChange={(e) => setWorkEnd(e.target.value)}
          />
        </label>
        <button type="submit">Set</button>
      </form>

      <form
        noValidate
        className="add"
        onSubmit={(e) => {
          e.preventDefault();
          send(addCommand(title, minutes));
          setTitle("");
        }}
      >
        <input
          aria-label="Title"
          placeholder="New task"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
        />
        <input
          aria-label="Planned minutes"
          type="number"
          min="1"
          value={minutes}
          onChange={(e) => setMinutes(e.target.value)}
        />
        <span>min</span>
        <button type="submit">Add</button>
      </form>

      <form
        noValidate
        className="add insert"
        onSubmit={(e) => {
          e.preventDefault();
          sendOrAsk(insertCommand(snap, insertTitle, insertMinutes, position.anchor), () =>
            setInsertTitle(""),
          );
        }}
      >
        <input
          aria-label="Insert title"
          placeholder="Insert task"
          value={insertTitle}
          onChange={(e) => setInsertTitle(e.target.value)}
        />
        <input
          aria-label="Insert planned minutes"
          type="number"
          min="1"
          value={insertMinutes}
          onChange={(e) => setInsertMinutes(e.target.value)}
        />
        <span>min</span>
        <select
          aria-label="Insert position"
          value={position.value}
          onChange={(e) => setInsertAt(e.target.value)}
        >
          {positions.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label}
            </option>
          ))}
        </select>
        <button type="submit">Insert</button>
      </form>

      {snap.day === null ? (
        <p>Starting…</p>
      ) : snap.items.length === 0 ? (
        <p>No tasks yet.</p>
      ) : (
        <ol className="plan">
          {snap.items.map((item) => (
            <PlanItemRow
              key={item.id}
              snap={snap}
              item={item}
              now={now}
              send={send}
              start={(cmd) => sendOrAsk(cmd)}
              focusExtend={focusExtend}
            />
          ))}
        </ol>
      )}
    </>
  );

  return (
    <main>
      <h1>
        nguda{snap.day ? ` · ${snap.day.date}` : ""}
        {tab === "plan" && badge && <span className={`badge ${badge}`}>{badge}</span>}
      </h1>
      <nav className="tabs" role="tablist">
        {TABS.map((t) => (
          <button
            key={t.value}
            type="button"
            role="tab"
            aria-selected={tab === t.value}
            className={tab === t.value ? "active" : ""}
            onClick={() => setTab(t.value)}
          >
            {t.label}
          </button>
        ))}
      </nav>

      {statusView}

      {tab === "summary" &&
        (snap.day ? (
          <Summary
            date={summaryDate ?? snap.day.date}
            today={snap.day.date}
            rev={snap.rev}
            opened={summaryOpened}
            onDate={(d) => setSummaryDate(d === snap.day?.date ? null : d)}
          />
        ) : (
          <p>Starting…</p>
        ))}

      {tab === "week" &&
        (snap.day ? (
          <Week
            date={weekDate ?? snap.day.date}
            today={snap.day.date}
            rev={snap.rev}
            onDate={setWeekDate}
          />
        ) : (
          <p>Starting…</p>
        ))}

      {/* Kept mounted so the tray's Extend… can always focus the running row. */}
      <div hidden={tab !== "plan"}>{planView}</div>
    </main>
  );
}

export default App;
