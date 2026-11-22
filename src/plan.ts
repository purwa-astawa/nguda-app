// Pure view helpers for the day plan (AD-5). They never sort, schedule, or check
// plan rules: they turn a user action on the rendered snapshot into the exact
// command and arguments, and decide which buttons a row shows. The engine decides
// everything else and reports rejections through `snapshot.status`.
// Erasable TS only, so `node --test` runs this file directly.

import type {
  CalendarView,
  InsertAnchorArg_Serialize,
  ItemView_Serialize,
  LockChoiceArg,
  ReorderAnchorArg,
  SnapshotEvent_Serialize,
} from "./bindings.ts";
import { elapsedSecs, formatElapsed } from "./elapsed.ts";

export type Snapshot = SnapshotEvent_Serialize;
export type Item = ItemView_Serialize;
/** The answer to "lock the day?": `lock` (Lock & start) or `noLock`. */
export type LockChoice = LockChoiceArg;

/** A command call, named as in `commands` of `bindings.ts`, with its arguments. */
export type Command =
  | { name: "createTask"; args: [title: string, plannedSecs: number] }
  | { name: "editTask"; args: [itemId: string, title: string | null, plannedSecs: number | null] }
  | {
      name: "reorderTask";
      args: [itemId: string, anchor: ReorderAnchorArg, expectedRev: number];
    }
  | { name: "toggleFixed"; args: [itemId: string, pinnedStart: string | null, expectedRev: number] }
  | { name: "removeTask"; args: [itemId: string] }
  | { name: "startTask"; args: [itemId: string, expectedRev: number, lock: LockChoice | null] }
  | { name: "lockDay"; args: [expectedRev: number] }
  | { name: "pauseTask"; args: [itemId: string] }
  | { name: "switchTask"; args: [itemId: string, expectedRev: number] }
  | { name: "extendTask"; args: [itemId: string, secs: number, expectedRev: number] }
  | { name: "setTags"; args: [itemId: string, tags: string[], expectedRev: number] }
  | {
      name: "insertTask";
      args: [
        title: string,
        plannedSecs: number,
        anchor: InsertAnchor,
        expectedRev: number,
        lock: LockChoice | null,
      ];
    }
  | { name: "setWorkEnd"; args: [minutes: number] }
  | { name: "setLaunchAtLogin"; args: [enabled: boolean] }
  | { name: "carryOverToTomorrow"; args: [expectedRev: number] }
  | { name: "signIn"; args: [] }
  | { name: "signOut"; args: [] }
  | { name: "setGoogleClient"; args: [clientId: string | null, clientSecret: string | null] }
  | { name: "setIncludedCalendars"; args: [calendarIds: string[]] }
  | { name: "excludeEvent"; args: [itemId: string, expectedRev: number] }
  | { name: "includeEvent"; args: [itemId: string, expectedRev: number] }
  | { name: "addCalendarEvent"; args: [calendarId: string, eventId: string] };

/** Where an inserted task goes, as `insertTask` takes it. Never a numeric index. */
export type InsertAnchor = InsertAnchorArg_Serialize;

/** One choice of the Insert form's position picker. */
export type InsertPosition = { value: string; label: string; anchor: InsertAnchor };

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

/** A pending or paused flexible item that is neither interrupted nor excluded:
 * the only kind a reorder or an insert can be placed next to. */
function isFlexibleAnchor(i: Item): boolean {
  return (
    !i.fixed && !i.interrupted && !i.excluded && (i.status === "pending" || i.status === "paused")
  );
}

/** The pending or paused flexible items just before and after `id`, in the order
 * given. Done, running and carried-over items sit at their actual times rather than
 * in order-key order, so they are never neighbours, and have none themselves.
 * Interrupted items flow first whatever their key, and the engine rejects them in
 * a reorder, so they are skipped the same way. */
export function flexibleNeighbours(
  items: readonly Item[],
  id: string,
): { prev: Item | null; next: Item | null } {
  const flexible = items.filter(isFlexibleAnchor);
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

/** Extends the running `item` by `minutes` at the rendered rev. `null` when the
 * item is not running or the minutes are blank, invalid or not positive. */
export function extendCommand(snap: Snapshot, item: Item, minutes: string): Command | null {
  if (item.status !== "running") return null;
  const secs = minutesToSecs(minutes);
  return secs > 0 ? { name: "extendTask", args: [item.id, secs, snap.rev] } : null;
}

/** Replaces `item`'s tags with `tags` at the rendered rev. The engine normalises
 * them (trim, blanks, case-insensitive duplicates, a known tag's spelling). */
export function setTagsCommand(snap: Snapshot, item: Item, tags: readonly string[]): Command {
  return { name: "setTags", args: [item.id, [...tags], snap.rev] };
}

/** The tag list after adding the typed `text`. Blank text gives back `tags`
 * itself, so the caller can skip sending; anything else is appended as typed. */
export function addTag(tags: readonly string[], text: string): readonly string[] {
  return text.trim() === "" ? tags : [...tags, text];
}

/** The tag list without `tag`. */
export function removeTag(tags: readonly string[], tag: string): string[] {
  return tags.filter((t) => t !== tag);
}

/** The known tags to suggest for an item: those it does not already carry,
 * compared case-insensitively, in the order given. */
export function tagSuggestions(known: readonly string[], current: readonly string[]): string[] {
  const have = new Set(current.map((t) => t.toLowerCase()));
  return known.filter((k) => !have.has(k.toLowerCase()));
}

/** The Insert form's positions, in this order: "Now" and "After current" while an
 * item runs; "Before" and "After" each pending or paused flexible item that is not
 * `interrupted`, in the order given; then "At end". Only lists anchors; the engine
 * places the item. */
export function insertPositions(snap: Snapshot): InsertPosition[] {
  const out: InsertPosition[] = [];
  const running = snap.running;
  if (running) {
    out.push({ value: "now", label: "Now", anchor: { place: "now", itemId: null } });
    out.push({
      value: "after-current",
      label: "After current",
      anchor: { place: "after", itemId: running.itemId },
    });
  }
  for (const i of snap.items) {
    if (!isFlexibleAnchor(i)) continue;
    out.push({
      value: `before:${i.id}`,
      label: `Before \u201c${i.title}\u201d`,
      anchor: { place: "before", itemId: i.id },
    });
    out.push({
      value: `after:${i.id}`,
      label: `After \u201c${i.title}\u201d`,
      anchor: { place: "after", itemId: i.id },
    });
  }
  out.push({ value: "end", label: "At end", anchor: { place: "end", itemId: null } });
  return out;
}

/** Inserts a task at `anchor` with the rendered rev and no lock choice. Blank or
 * invalid minutes are sent as 0 so the engine can reject them. An insert-now on an
 * undecided day gets its choice through `withLock` first (`asksLockChoice`). */
export function insertCommand(
  snap: Snapshot,
  title: string,
  minutes: string,
  anchor: InsertAnchor,
): Command {
  return { name: "insertTask", args: [title, minutesToSecs(minutes), anchor, snap.rev, null] };
}

/** The day is undecided (AD-7): its first Start or insert-now must carry a lock
 * choice, asked before anything is sent. Decided days never ask. */
export function needsLockChoice(snap: Snapshot): boolean {
  return snap.day?.lockDecision === "none";
}

/** Whether sending `cmd` must first ask "Lock & start / Start without lock": a
 * Start, or an insert with the `now` anchor, on an undecided day. */
export function asksLockChoice(snap: Snapshot, cmd: Command): boolean {
  if (!needsLockChoice(snap)) return false;
  return cmd.name === "startTask" || (cmd.name === "insertTask" && cmd.args[2].place === "now");
}

/** `cmd` (a Start or an insert) carrying the lock `choice`; other commands are
 * returned unchanged. */
export function withLock(cmd: Command, choice: LockChoice): Command {
  switch (cmd.name) {
    case "startTask":
      return { name: "startTask", args: [cmd.args[0], cmd.args[1], choice] };
    case "insertTask": {
      const [title, secs, anchor, rev] = cmd.args;
      return { name: "insertTask", args: [title, secs, anchor, rev, choice] };
    }
    default:
      return cmd;
  }
}

/** Locks the undecided day without starting anything, at the rendered rev. `null`
 * (no Lock day button) once the day is decided. */
export function lockDayCommand(snap: Snapshot): Command | null {
  return needsLockChoice(snap) ? { name: "lockDay", args: [snap.rev] } : null;
}

/** The day's lock badge: `locked` or `unlocked` once decided, `null` while
 * undecided (the Lock day button shows instead). */
export function lockBadge(snap: Snapshot): "locked" | "unlocked" | null {
  switch (snap.day?.lockDecision) {
    case "locked":
      return "locked";
    case "noLock":
      return "unlocked";
    default:
      return null;
  }
}

/** Sets the work-end from a time field's `HH:MM`. Anything else is sent as 1440,
 * out of range, so the engine rejects it. */
export function workEndCommand(time: string): Command {
  const m = /^(\d{2}):(\d{2})$/.exec(time);
  const [h, min] = m ? [Number(m[1]), Number(m[2])] : [24, 0];
  return { name: "setWorkEnd", args: [h < 24 && min < 60 ? h * 60 + min : 1440] };
}

/** The items the engine listed in `snap.overflow`, in that order. Only looks them
 * up; never decides what overflows. */
export function overflowItems(snap: Snapshot): Item[] {
  const byId = new Map(snap.items.map((i) => [i.id, i] as const));
  return snap.overflow.flatMap((id) => byId.get(id) ?? []);
}

/** Moves the snapshot's overflow list to tomorrow, checked against its rev. */
export function carryOverCommand(snap: Snapshot): Command {
  return { name: "carryOverToTomorrow", args: [snap.rev] };
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
  /** Leave this imported event out of today's schedule (rev-checked). */
  exclude: Command | null;
  /** Take this excluded event back into the schedule (rev-checked). */
  include: Command | null;
};

export function rowActions(snap: Snapshot, item: Item): RowActions {
  const { prev, next } = flexibleNeighbours(snap.items, item.id);
  const running = item.status === "running";
  const startable = !item.excluded && (item.status === "pending" || item.status === "paused");
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
    // No lock choice: an undecided day asks for one first (`asksLockChoice`).
    start: startable ? { name: "startTask", args: [item.id, snap.rev, null] } : null,
    pause: running ? { name: "pauseTask", args: [item.id] } : null,
    switch: running ? { name: "switchTask", args: [item.id, snap.rev] } : null,
    remove: running ? null : { name: "removeTask", args: [item.id] },
    exclude: excludeEventCommand(snap, item),
    include: includeEventCommand(snap, item),
  };
}

/** Leaves the imported `item` out of today's schedule at the rendered rev: only
 * a pending imported item with no time recorded that is not excluded yet, else
 * `null`. The engine checks again. */
export function excludeEventCommand(snap: Snapshot, item: Item): Command | null {
  const excludable =
    item.imported && !item.excluded && item.status === "pending" && item.actualSecs === 0;
  return excludable ? { name: "excludeEvent", args: [item.id, snap.rev] } : null;
}

/** Takes the excluded `item` back into the schedule at the rendered rev; `null`
 * unless it is excluded. */
export function includeEventCommand(snap: Snapshot, item: Item): Command | null {
  return item.excluded ? { name: "includeEvent", args: [item.id, snap.rev] } : null;
}

/** Adds the single event `eventId` of calendar `calendarId` to today's plan. */
export function addCalendarEventCommand(calendarId: string, eventId: string): Command {
  return { name: "addCalendarEvent", args: [calendarId, eventId] };
}

/** The calendars the picker offers single events from: signed in only, those
 * not included in the plan (an included one's events are imported already), in
 * the snapshot's order. */
export function pickerCalendars(snap: Snapshot): CalendarView[] {
  return snap.google === "signedIn" ? snap.calendars.filter((c) => !c.included) : [];
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
function zonedTime(iso: string, tz: string): string {
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
export function scheduleLabel(
  item: Pick<Item, "scheduledStart" | "scheduledEnd">,
  tz: string,
): string | null {
  if (!item.scheduledStart || !item.scheduledEnd) return null;
  return `${zonedTime(item.scheduledStart, tz)}\u2013${zonedTime(item.scheduledEnd, tz)}`;
}

/** Why Sign in is disabled when no OAuth client is there to sign in with. */
export const NO_CLIENT =
  "No Google client is built in. Enter your own OAuth client ID to sign in.";

/** What the Google section shows for the snapshot's account state: its label,
 * whether a Sign in button shows, whether that button is disabled (`busy`) and
 * why (`noClient`), whether a Sign out button shows, whether the "your own
 * OAuth client" form shows (signed out only) with the current custom id, and
 * the calendars to offer as checkboxes (signed in only, in the snapshot's
 * order). */
export type GoogleSection = {
  label: string;
  signIn: boolean;
  busy: boolean;
  noClient: string | null;
  signOut: boolean;
  clientForm: boolean;
  customId: string | null;
  calendars: CalendarView[];
};

export function googleSection(snap: Snapshot): GoogleSection {
  const hidden = {
    signIn: false,
    busy: false,
    noClient: null,
    signOut: false,
    clientForm: false,
    customId: null,
    calendars: [],
  };
  switch (snap.google) {
    case "unknown":
      return { ...hidden, label: "Checking…" };
    case "signedOut": {
      const available = snap.googleClient.available;
      return {
        ...hidden,
        label: "Not signed in",
        signIn: true,
        busy: !available,
        noClient: available ? null : NO_CLIENT,
        clientForm: true,
        customId: snap.googleClient.customId ?? null,
      };
    }
    case "signingIn":
      return { ...hidden, label: "Waiting for browser…", signIn: true, busy: true };
    case "signedIn":
      return { ...hidden, label: "Signed in", signOut: true, calendars: snap.calendars };
  }
}

/** Ticks (`included`) or unticks calendar `calendarId`: sends the full set of
 * included ids after the change, in the snapshot's order. */
export function includeCalendarCommand(
  snap: Snapshot,
  calendarId: string,
  included: boolean,
): Command {
  const ids = snap.calendars
    .filter((c) => (c.id === calendarId ? included : c.included))
    .map((c) => c.id);
  return { name: "setIncludedCalendars", args: [ids] };
}

/** Signs in to Google; progress comes back in `snapshot.google`. */
export function signInCommand(): Command {
  return { name: "signIn", args: [] };
}

/** Turns launch at login on or off; the OS login item as read back comes back in
 * `snapshot.autostart`. */
export function launchAtLoginCommand(enabled: boolean): Command {
  return { name: "setLaunchAtLogin", args: [enabled] };
}

/** Signs out of Google; every plan item stays. */
export function signOutCommand(): Command {
  return { name: "signOut", args: [] };
}

/** Saves the user's own OAuth client: the id and secret trimmed, a blank secret
 * sent as none. A blank id clears the client, like {@link clearGoogleClientCommand}. */
export function googleClientCommand(clientId: string, clientSecret: string): Command {
  const id = clientId.trim();
  const secret = clientSecret.trim();
  return { name: "setGoogleClient", args: [id || null, id && secret ? secret : null] };
}

/** Clears the user's own OAuth client, going back to the built-in one. */
export function clearGoogleClientCommand(): Command {
  return { name: "setGoogleClient", args: [null, null] };
}
