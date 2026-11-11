// Pure view helpers for the day plan (AD-5). They never sort, schedule, or check
// plan rules: they turn a user action on the rendered snapshot into the exact
// command and arguments, and decide which buttons a row shows. The engine decides
// everything else and reports rejections through `snapshot.status`.
// Erasable TS only, so `node --test` runs this file directly.

import type { ItemView_Serialize, SnapshotEvent_Serialize } from "./bindings.ts";
import { elapsedSecs, formatElapsed } from "./elapsed.ts";

export type Snapshot = SnapshotEvent_Serialize;
export type Item = ItemView_Serialize;

/** A command call, named as in `commands` of `bindings.ts`, with its arguments. */
export type Command =
  | { name: "createTask"; args: [title: string, plannedSecs: number] }
  | { name: "editTask"; args: [itemId: string, title: string | null, plannedSecs: number | null] }
  | {
      name: "reorderTask";
      args: [itemId: string, anchor: { place: "before" | "after"; itemId: string }, expectedRev: number];
    }
  | { name: "toggleFixed"; args: [itemId: string, pinnedStart: string | null, expectedRev: number] }
  | { name: "removeTask"; args: [itemId: string] }
  | { name: "startTask"; args: [itemId: string, expectedRev: number] }
  | { name: "pauseTask"; args: [itemId: string] }
  | { name: "switchTask"; args: [itemId: string, expectedRev: number] };

/** Keeps `next` only if it is newer than `prev`. Events and the `currentSnapshot`
 * reply can arrive in either order. */
export function mergeSnapshot(prev: Snapshot | null, next: Snapshot): Snapshot {
  return !prev || next.rev > prev.rev ? next : prev;
}

/** Minutes typed in a form to whole seconds. Blank or invalid becomes 0, which is
 * sent as is so the engine can reject it. */
export function minutesToSecs(minutes: string): number {
  const n = Number(minutes.trim());
  return Number.isFinite(n) && n > 0 ? Math.round(n * 60) : 0;
}

/** Seconds to minutes for display and form defaults. */
export function secsToMinutes(secs: number): number {
  return Math.round((secs / 60) * 100) / 100;
}

/** The pin instant for local `HH:MM` on `date` (`YYYY-MM-DD`), in the browser's
 * zone, as an ISO string. `null` when no valid time is entered. */
export function pinInstant(date: string, time: string): string | null {
  if (!/^\d{2}:\d{2}$/.test(time)) return null;
  const d = new Date(`${date}T${time}`);
  return Number.isNaN(d.getTime()) ? null : d.toISOString();
}

/** The pending or paused flexible items just before and after `id`, in the order
 * given. Done, running and carried-over items sit at their actual times rather than
 * in order-key order, so they are never neighbours, and have none themselves. */
export function flexibleNeighbours(
  items: readonly Item[],
  id: string,
): { prev: Item | null; next: Item | null } {
  const flexible = items.filter(
    (i) => !i.fixed && (i.status === "pending" || i.status === "paused"),
  );
  const at = flexible.findIndex((i) => i.id === id);
  if (at < 0) return { prev: null, next: null };
  return { prev: flexible[at - 1] ?? null, next: flexible[at + 1] ?? null };
}

export function addCommand(title: string, minutes: string): Command {
  return { name: "createTask", args: [title, minutesToSecs(minutes)] };
}

/** Sends only the changed fields, `null` for the rest. `null` when nothing changed. */
export function editCommand(item: Item, title: string, minutes: string): Command | null {
  const secs = minutesToSecs(minutes);
  const newTitle = title === item.title ? null : title;
  const newSecs = secs === item.plannedSecs ? null : secs;
  if (newTitle === null && newSecs === null) return null;
  return { name: "editTask", args: [item.id, newTitle, newSecs] };
}

export function pinCommand(snap: Snapshot, item: Item, time: string): Command | null {
  if (item.fixed || !snap.day) return null;
  const at = pinInstant(snap.day.date, time);
  return at === null ? null : { name: "toggleFixed", args: [item.id, at, snap.rev] };
}

/** Everything a row offers. `null` means the button is not shown. Pin is shown
 * (`canPin`) on flexible items and is built with `pinCommand` once a time exists. */
export type RowActions = {
  up: Command | null;
  down: Command | null;
  canPin: boolean;
  unpin: Command | null;
  start: Command | null;
  pause: Command | null;
  /** Finish this running item and move on to the next (rev-checked). */
  switch: Command | null;
  remove: Command | null;
};

export function rowActions(snap: Snapshot, item: Item): RowActions {
  const { prev, next } = item.fixed
    ? { prev: null, next: null }
    : flexibleNeighbours(snap.items, item.id);
  const running = item.status === "running";
  const startable = item.status === "pending" || item.status === "paused";
  return {
    up: prev && {
      name: "reorderTask",
      args: [item.id, { place: "before", itemId: prev.id }, snap.rev],
    },
    down: next && {
      name: "reorderTask",
      args: [item.id, { place: "after", itemId: next.id }, snap.rev],
    },
    canPin: !item.fixed,
    unpin: item.fixed ? { name: "toggleFixed", args: [item.id, null, snap.rev] } : null,
    start: startable ? { name: "startTask", args: [item.id, snap.rev] } : null,
    pause: running ? { name: "pauseTask", args: [item.id] } : null,
    switch: running ? { name: "switchTask", args: [item.id, snap.rev] } : null,
    remove: running ? null : { name: "removeTask", args: [item.id] },
  };
}

/** A row's time: ticking elapsed for the running item, banked time otherwise. */
export function rowTime(snap: Snapshot, item: Item, nowNs: bigint): string {
  const r = snap.running;
  return r && r.itemId === item.id
    ? formatElapsed(elapsedSecs(r, nowNs))
    : formatElapsed(item.actualSecs);
}

/** Local `HH:MM` of an RFC 3339 instant in the IANA zone `tz`, 24-hour. Falls back
 * to the browser's zone if `tz` is unknown. */
export function zonedTime(iso: string, tz: string): string {
  const opts: Intl.DateTimeFormatOptions = { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };
  const at = new Date(iso);
  try {
    return at.toLocaleTimeString("en-GB", { ...opts, timeZone: tz });
  } catch {
    return at.toLocaleTimeString("en-GB", opts);
  }
}

/** A row's scheduled `HH:MM–HH:MM` in the day's zone, or `null` when the schedule
 * gives it no span. Only formats the engine's times; never computes them. */
export function scheduleLabel(item: Item, tz: string): string | null {
  if (!item.scheduledStart || !item.scheduledEnd) return null;
  return `${zonedTime(item.scheduledStart, tz)}\u2013${zonedTime(item.scheduledEnd, tz)}`;
}
