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

/// The app's migrations, oldest first. The schema version is their count.
pub(super) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(V1), M::up(V2), M::up(V3), M::up(V4)])
}

#[cfg(test)]
pub(super) const V1_SQL: &str = V1;
