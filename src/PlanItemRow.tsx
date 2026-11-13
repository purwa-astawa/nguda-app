import { useEffect, useRef, useState } from "react";
import {
  editCommand,
  extendCommand,
  pinCommand,
  rowActions,
  rowTime,
  scheduleLabel,
  secsToMinutes,
  type Command,
  type Item,
  type Snapshot,
} from "./plan";

const STATUS_LABEL: Record<Item["status"], string> = {
  pending: "pending",
  running: "running",
  paused: "paused",
  done: "done",
  carriedOver: "carried over",
  removed: "removed",
};

function localTime(iso: string): string {
  return new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

type Props = {
  snap: Snapshot;
  item: Item;
  now: bigint;
  send: (cmd: Command) => void;
  /** Changes each time the tray's or a notification's Extend… asks to focus the
   * running row's extend field. */
  focusExtend: number;
};

/** One plan item: its facts, and the buttons `rowActions` allows. */
export function PlanItemRow({ snap, item, now, send, focusExtend }: Props) {
  const [editing, setEditing] = useState(false);
  const [title, setTitle] = useState(item.title);
  const [minutes, setMinutes] = useState(String(secsToMinutes(item.plannedSecs)));
  const [pinTime, setPinTime] = useState("");
  const [extendMinutes, setExtendMinutes] = useState("5");
  const extendRef = useRef<HTMLInputElement>(null);
  // The last focus request from the tray's or a notification's Extend… this row has
  // handled; starts at the value at mount.
  const handledFocus = useRef(focusExtend);
  const running = item.status === "running";
  const extend = extendCommand(snap, item, extendMinutes);

  useEffect(() => {
    if (focusExtend === handledFocus.current || !running) return;
    handledFocus.current = focusExtend;
    extendRef.current?.focus();
    extendRef.current?.select();
  }, [focusExtend, running]);
  const actions = rowActions(snap, item);
  const pin = pinCommand(snap, item, pinTime);
  const scheduled = snap.day ? scheduleLabel(item, snap.day.tz) : null;

  const startEdit = () => {
    setTitle(item.title);
    setMinutes(String(secsToMinutes(item.plannedSecs)));
    setEditing(true);
  };

  return (
    <li className={`item ${item.status}`}>
      {editing ? (
        <form
          noValidate
          className="edit"
          onSubmit={(e) => {
            e.preventDefault();
            const cmd = editCommand(item, title, minutes);
            if (cmd) send(cmd);
            setEditing(false);
          }}
        >
          <input aria-label="Title" value={title} onChange={(e) => setTitle(e.target.value)} />
          <input
            aria-label="Planned minutes"
            type="number"
            min="1"
            value={minutes}
            onChange={(e) => setMinutes(e.target.value)}
          />
          <button type="submit">Save</button>
          <button type="button" onClick={() => setEditing(false)}>
            Cancel
          </button>
        </form>
      ) : (
        <div className="facts">
          {scheduled && <span className="when">{scheduled}</span>}
          <span className="title">{item.title}</span>
          <span className="meta">
            {STATUS_LABEL[item.status]} · {rowTime(snap, item, now)} /{" "}
            {secsToMinutes(item.plannedSecs)} min
            {item.pinnedStart && ` · at ${localTime(item.pinnedStart)}`}
          </span>
        </div>
      )}

      <div className="actions">
        {actions.start && <button onClick={() => send(actions.start!)}>Start</button>}
        {actions.pause && <button onClick={() => send(actions.pause!)}>Pause</button>}
        {actions.switch && <button onClick={() => send(actions.switch!)}>Switch</button>}
        {running && (
          <form
            noValidate
            className="extend"
            onSubmit={(e) => {
              e.preventDefault();
              if (extend) send(extend);
            }}
          >
            <input
              ref={extendRef}
              aria-label="Extend minutes"
              type="number"
              min="1"
              value={extendMinutes}
              onChange={(e) => setExtendMinutes(e.target.value)}
            />
            <button type="submit" disabled={!extend}>
              Extend
            </button>
          </form>
        )}
        {!editing && <button onClick={startEdit}>Edit</button>}
        {actions.up && (
          <button aria-label="Move up" onClick={() => send(actions.up!)}>
            Up
          </button>
        )}
        {actions.down && (
          <button aria-label="Move down" onClick={() => send(actions.down!)}>
            Down
          </button>
        )}
        {actions.canPin && (
          <>
            <input
              aria-label="Pin time"
              type="time"
              value={pinTime}
              onChange={(e) => setPinTime(e.target.value)}
            />
            <button
              disabled={!pin}
              onClick={() => {
                if (!pin) return;
                send(pin);
                setPinTime("");
              }}
            >
              Pin
            </button>
          </>
        )}
        {actions.unpin && <button onClick={() => send(actions.unpin!)}>Unpin</button>}
        {actions.remove && <button onClick={() => send(actions.remove!)}>Remove</button>}
      </div>
    </li>
  );
}
