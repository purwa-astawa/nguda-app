//! Embedded, forward-only schema migrations (AD-6, AD-12).
//!
//! Instants are INTEGER Unix nanoseconds (UTC), durations INTEGER seconds, dates
//! `YYYY-MM-DD` text, and ids hyphenated UUID text. Never edit a shipped migration:
//! append a new one.

use rusqlite_migration::{M, Migrations};

/// v1: days, plan items (no stored schedule), and time segments with at most one open.
const V1: &str = r#"
CREATE TABLE day (
    date TEXT PRIMARY KEY NOT NULL CHECK (length(date) = 10),
    tz   TEXT NOT NULL
) STRICT;

CREATE TABLE plan_item (
    id           TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    day_date     TEXT NOT NULL REFERENCES day (date),
    title        TEXT NOT NULL,
    planned_secs INTEGER NOT NULL CHECK (planned_secs >= 0),
    "order"      TEXT NOT NULL,
    fixed        INTEGER NOT NULL CHECK (fixed IN (0, 1)),
    pinned_start INTEGER,
    status       TEXT NOT NULL CHECK (status IN
                     ('pending', 'running', 'paused', 'done', 'carried_over', 'removed')),
    removed_at   INTEGER,
    CHECK (fixed = (pinned_start IS NOT NULL))
) STRICT;

CREATE INDEX plan_item_day_date ON plan_item (day_date);

CREATE TABLE time_segment (
    id         INTEGER PRIMARY KEY,
    item_id    TEXT NOT NULL REFERENCES plan_item (id),
    started_at INTEGER NOT NULL,
    ended_at   INTEGER,
    UNIQUE (item_id, started_at),
    CHECK (ended_at IS NULL OR ended_at >= started_at)
) STRICT;

CREATE UNIQUE INDEX time_segment_single_open
    ON time_segment ((ended_at IS NULL)) WHERE ended_at IS NULL;
"#;

/// v2: the engine heartbeat (one row) and the append-only day log. `kind` has no
/// CHECK, so later kinds need no table rebuild; the adapter rejects unknown ones.
const V2: &str = r#"
CREATE TABLE engine_meta (
    id            INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    last_alive_at INTEGER NOT NULL
) STRICT;

CREATE TABLE day_log (
    id       INTEGER PRIMARY KEY,
    day_date TEXT NOT NULL REFERENCES day (date),
    at       INTEGER NOT NULL,
    kind     TEXT NOT NULL,
    item_id  TEXT REFERENCES plan_item (id)
) STRICT;

CREATE INDEX day_log_day_date ON day_log (day_date);
"#;

/// v3: the second item of a day-log entry (`switched`: `item_id` is the item
/// switched from, `to_item_id` the one switched to, NULL when none was left).
const V3: &str = r#"
ALTER TABLE day_log ADD COLUMN to_item_id TEXT REFERENCES plan_item (id);
"#;

/// v4: the seconds of a day-log entry (`extended`: how many seconds were added to
/// `item_id`'s planned length). NULL for kinds without one.
const V4: &str = r#"
ALTER TABLE day_log ADD COLUMN secs INTEGER CHECK (secs IS NULL OR secs > 0);
"#;

/// v5: each day's interrupt stack (AD-15), one row per entry, `position` 0 at the
/// bottom. An item is on at most one stack, once.
const V5: &str = r#"
CREATE TABLE interrupt_entry (
    day_date TEXT NOT NULL REFERENCES day (date),
    position INTEGER NOT NULL CHECK (position >= 0),
    item_id  TEXT NOT NULL UNIQUE REFERENCES plan_item (id),
    PRIMARY KEY (day_date, position)
) STRICT;
"#;

/// v6: user settings (Config convention), one row per key. `work_end` holds the
/// work-end time of day as `HH:MM`; a missing row means the default.
const V6: &str = r#"
CREATE TABLE settings (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
"#;

/// v7: the item on an earlier day a plan item was carried over from (AD-6), NULL
/// for an item that was not carried over.
const V7: &str = r#"
ALTER TABLE plan_item ADD COLUMN carried_from TEXT REFERENCES plan_item (id);
"#;

/// v8: each day's lock decision (AD-7) and its write-once baseline (AD-6). Days
/// that already have time on them were started before locking existed, so they are
/// decided `no_lock`; the rest stay undecided. A baseline holds one row per frozen
/// item, `position` 0 first in schedule order, with its planned length and its
/// scheduled span (both NULL or both set). Triggers refuse any UPDATE or DELETE.
const V8: &str = r#"
ALTER TABLE day ADD COLUMN lock_decision TEXT NOT NULL DEFAULT 'none'
    CHECK (lock_decision IN ('none', 'locked', 'no_lock'));

UPDATE day SET lock_decision = 'no_lock'
WHERE date IN (
    SELECT p.day_date FROM plan_item p JOIN time_segment s ON s.item_id = p.id
);

CREATE TABLE baseline (
    day_date  TEXT PRIMARY KEY NOT NULL REFERENCES day (date),
    locked_at INTEGER NOT NULL
) STRICT;

CREATE TABLE baseline_item (
    day_date        TEXT NOT NULL REFERENCES baseline (day_date),
    position        INTEGER NOT NULL CHECK (position >= 0),
    item_id         TEXT NOT NULL REFERENCES plan_item (id),
    planned_secs    INTEGER NOT NULL CHECK (planned_secs >= 0),
    scheduled_start INTEGER,
    scheduled_end   INTEGER,
    PRIMARY KEY (day_date, item_id),
    UNIQUE (day_date, position),
    CHECK ((scheduled_start IS NULL) = (scheduled_end IS NULL))
) STRICT;

CREATE TRIGGER baseline_no_update BEFORE UPDATE ON baseline
BEGIN SELECT RAISE(ABORT, 'baseline is write-once'); END;
CREATE TRIGGER baseline_no_delete BEFORE DELETE ON baseline
BEGIN SELECT RAISE(ABORT, 'baseline is write-once'); END;
CREATE TRIGGER baseline_item_no_update BEFORE UPDATE ON baseline_item
BEGIN SELECT RAISE(ABORT, 'baseline is write-once'); END;
CREATE TRIGGER baseline_item_no_delete BEFORE DELETE ON baseline_item
BEGIN SELECT RAISE(ABORT, 'baseline is write-once'); END;
"#;

/// v9: tags (CAP-8, AD-9). A tag name is stored once, unique case-insensitively, and
/// is never deleted, so the known names only grow. `plan_item_tag` links an item to
/// its tags, `position` 0 first in the item's order.
const V9: &str = r#"
CREATE TABLE tag (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
) STRICT;

CREATE TABLE plan_item_tag (
    item_id  TEXT NOT NULL REFERENCES plan_item (id),
    tag_id   INTEGER NOT NULL REFERENCES tag (id),
    position INTEGER NOT NULL CHECK (position >= 0),
    PRIMARY KEY (item_id, tag_id)
) STRICT;
"#;

/// v10: where an imported plan item came from (CAP-2, AD-9). An imported item has
/// its calendar id, event id, and the event length at the last import
/// (`source_secs`), plus a recurring instance's original start; a local item has
/// all of them NULL. `removed_by_source_at` is when Google cancelled the event.
/// The merge key `(calendar_id, source_id, instance_start)` is unique per day.
const V10: &str = r#"
ALTER TABLE plan_item ADD COLUMN calendar_id TEXT;
ALTER TABLE plan_item ADD COLUMN source_id TEXT
    CHECK ((source_id IS NULL) = (calendar_id IS NULL));
ALTER TABLE plan_item ADD COLUMN instance_start INTEGER
    CHECK (instance_start IS NULL OR calendar_id IS NOT NULL);
ALTER TABLE plan_item ADD COLUMN source_secs INTEGER
    CHECK ((source_secs IS NULL) = (calendar_id IS NULL) AND
           (source_secs IS NULL OR source_secs >= 0));
ALTER TABLE plan_item ADD COLUMN removed_by_source_at INTEGER
    CHECK (removed_by_source_at IS NULL OR calendar_id IS NOT NULL);

CREATE UNIQUE INDEX plan_item_source_key
    ON plan_item (day_date, calendar_id, source_id, IFNULL(instance_start, -1))
    WHERE calendar_id IS NOT NULL;
"#;

/// v11: each calendar's sync cursor (AD-17): Google's sync token and the local
/// day it was issued for. At most one per calendar.
const V11: &str = r#"
CREATE TABLE calendar_sync (
    calendar_id TEXT PRIMARY KEY NOT NULL,
    sync_token  TEXT NOT NULL,
    day_date    TEXT NOT NULL CHECK (length(day_date) = 10)
) STRICT;
"#;

/// Every migration's SQL, oldest first.
const ALL: [&str; 11] = [V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11];

/// The app's migrations, oldest first. The schema version is their count.
pub(super) fn migrations() -> Migrations<'static> {
    Migrations::new(ALL.iter().map(|sql| M::up(sql)).collect())
}

/// The first `n` migrations, for building a database at an older version.
#[cfg(test)]
pub(super) fn migrations_through(n: usize) -> Migrations<'static> {
    Migrations::new(ALL[..n].iter().map(|sql| M::up(sql)).collect())
}

#[cfg(test)]
pub(super) const V1_SQL: &str = V1;
