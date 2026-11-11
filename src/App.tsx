import { useEffect, useState } from "react";
import { events, commands } from "./bindings";
import { nowNs } from "./elapsed";
import { addCommand, mergeSnapshot, type Command, type Snapshot } from "./plan";
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

  return (
    <main>
      <h1>nguda{snap.day ? ` · ${snap.day.date}` : ""}</h1>

      {snap.status.map((s) => (
        <p key={s.key} className="status" role="alert">
          {s.message}
        </p>
      ))}
      {localError && (
        <p className="status" role="alert">
          {localError}
        </p>
      )}

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

      {snap.day === null ? (
        <p>Starting…</p>
      ) : snap.items.length === 0 ? (
        <p>No tasks yet.</p>
      ) : (
        <ol className="plan">
          {snap.items.map((item) => (
            <PlanItemRow key={item.id} snap={snap} item={item} now={now} send={send} />
          ))}
        </ol>
      )}
    </main>
  );
}

export default App;
