import { useEffect, useState } from "react";
import { events, commands } from "./bindings";
import { nowNs } from "./elapsed";
import {
  addCommand,
  carryOverCommand,
  insertCommand,
  insertPositions,
  mergeSnapshot,
  overflowItems,
  workEndCommand,
  type Command,
  type Snapshot,
} from "./plan";
import { PlanItemRow } from "./PlanItemRow";
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
    case "pauseTask":
      res = await commands.pauseTask(...cmd.args);
      break;
    case "switchTask":
      res = await commands.switchTask(...cmd.args);
      break;
    case "extendTask":
      res = await commands.extendTask(...cmd.args);
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
  }
  return res.status === "error" ? res.error : null;
}

// Thin view (AD-5): it keeps only the newest snapshot plus local form state, shows
// items in the order given, and sends every change as a command.
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
  // Bumped by the tray's Extend…; the running row focuses its extend field.
  const [focusExtend, setFocusExtend] = useState(0);

  useEffect(() => {
    const unlisten = events.focusExtendEvent.listen(() => setFocusExtend((n) => n + 1));
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

  return (
    <main>
      <h1>nguda{snap.day ? ` · ${snap.day.date}` : ""}</h1>

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
          send(insertCommand(snap, insertTitle, insertMinutes, position.anchor));
          setInsertTitle("");
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
              focusExtend={focusExtend}
            />
          ))}
        </ol>
      )}
    </main>
  );
}

export default App;
