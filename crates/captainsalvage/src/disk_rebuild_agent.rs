//! What an agent changes on a Disk Rebuild, kept so the person can see it and take it back.
//!
//! An agent may build and change a Disk Rebuild. Every change it makes is journaled as the rows it
//! added, removed and altered, with the agent session that made it. From that journal the window
//! marks what the agent made, and the person can undo the last change, undo every change of one
//! agent session, or restore a Disk Rebuild to how it was when the agent first touched it in that
//! session (a snapshot taken before that first change). All of it lives in the app's own
//! database. Nothing here reads or writes a source disk.
//!
//! A session is one agent connection: the MCP session a client opens with `initialize`, and in the
//! window's chat one question to the agent. Its id is stored with each change, so it survives
//! the app closing.
//!
//! Limits keep an agent from running away: a change touches at most [`MAX_CHANGES_PER_CALL`] rows
//! at once, and a session at most [`MAX_CHANGES_PER_SESSION`] in all. Undoing does not give changes
//! back, so an agent cannot get round the session limit by undoing.

use crate::map::MapDb;
use crate::workspace::now_millis;
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// The most rows (items, chosen copies, the Disk Rebuild itself) one agent call may change.
pub const MAX_CHANGES_PER_CALL: u64 = 1_000;
/// The most rows one agent session may change in all.
pub const MAX_CHANGES_PER_SESSION: u64 = 10_000;
/// The session of an agent that did not come in through a connection, such as a test.
pub const DEFAULT_SESSION: &str = "direct";

// The Disk Rebuild tables, named once so a rename of them touches only this block.
const REBUILDS: &str = "disk_rebuilds";
const ITEMS: &str = "disk_rebuild_items";
const CHOICES: &str = "disk_rebuild_choices";
const SUGGESTIONS: &str = "disk_rebuild_suggestions";
const OWNER: &str = "disk_rebuild";

/// The limits on what an agent may change. Defaults are the constants above.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLimits {
    pub per_call: u64,
    pub per_session: u64,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            per_call: MAX_CHANGES_PER_CALL,
            per_session: MAX_CHANGES_PER_SESSION,
        }
    }
}

/// Which agent connection a call came in through.
#[derive(Clone, Debug)]
pub struct AgentConnection {
    pub session: String,
    pub client: Option<String>,
}

impl Default for AgentConnection {
    fn default() -> Self {
        Self {
            session: DEFAULT_SESSION.to_string(),
            client: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ItemRow {
    id: i64,
    owner: i64,
    kind: String,
    head: Option<u32>,
    name: Option<String>,
    parent_item: Option<i64>,
    parent_head: Option<u32>,
    entry: Option<i64>,
    position: i64,
    created_at_millis: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RebuildRow {
    id: i64,
    name: String,
    created_at_millis: i64,
    updated_at_millis: i64,
}

/// A Disk Rebuild as stored: its row, its items and its chosen copies.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    rebuild: Option<RebuildRow>,
    items: BTreeMap<i64, ItemRow>,
    choices: BTreeMap<String, i64>,
}

/// One row that a change added, removed or altered, with it before and after.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "camelCase")]
enum RowChange {
    Item {
        id: i64,
        before: Option<ItemRow>,
        after: Option<ItemRow>,
    },
    Choice {
        path: String,
        owner: i64,
        before: Option<i64>,
        after: Option<i64>,
    },
    Rebuild {
        before: Option<RebuildRow>,
        after: Option<RebuildRow>,
    },
}

fn item_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    Ok(ItemRow {
        id: row.get(0)?,
        owner: row.get(1)?,
        kind: row.get(2)?,
        head: row.get(3)?,
        name: row.get(4)?,
        parent_item: row.get(5)?,
        parent_head: row.get(6)?,
        entry: row.get(7)?,
        position: row.get(8)?,
        created_at_millis: row.get(9)?,
    })
}

fn item_columns() -> String {
    format!(
        "id, {OWNER}, kind, head, name, parent_item, parent_head, entry, position, created_at_millis"
    )
}

/// What the agent changed and what the person may do about it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChange {
    pub id: i64,
    pub disk_rebuild: i64,
    pub session: String,
    pub tool: String,
    pub summary: String,
    /// How many rows it changed.
    pub size: u64,
    pub at_millis: u64,
    pub undone: bool,
    /// Always `agent`: only agent changes are journaled.
    pub origin: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSession {
    pub id: String,
    pub client: Option<String>,
    pub started_at_millis: u64,
    pub last_at_millis: u64,
    /// Rows changed so far, counted against the session limit. Undoing does not lower it.
    pub changes_used: u64,
    /// Changes of this session not undone yet.
    pub active_changes: u64,
    pub disk_rebuilds: Vec<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChanges {
    pub changes: Vec<AgentChange>,
    pub sessions: Vec<AgentSession>,
    /// The latest change that can still be undone.
    pub last: Option<AgentChange>,
    pub limits: AgentLimits,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoReport {
    pub undone: u64,
    /// Changes of the session that are still there, because the undo stopped.
    pub remaining: u64,
    /// Why it stopped early: something was changed since, so undoing would lose it.
    pub stopped: Option<String>,
    pub session: Option<String>,
    pub disk_rebuilds: Vec<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub disk_rebuild: i64,
    pub session: String,
    /// The Disk Rebuild did not exist when the agent session began, so it was removed.
    pub removed: bool,
    /// Agent changes this took back.
    pub undone: u64,
}

/// What an agent made on one Disk Rebuild, and has not been undone.
#[derive(Clone, Debug, Default)]
pub struct AgentMarks {
    pub items: HashSet<i64>,
    pub choices: HashSet<String>,
    /// The agent made or renamed the Disk Rebuild itself.
    pub rebuild: bool,
}

fn read_state(db: &MapDb, id: i64) -> Result<State> {
    let rebuild = db
        .connection
        .query_row(
            &format!(
                "SELECT id, name, created_at_millis, updated_at_millis FROM {REBUILDS} WHERE id = ?1"
            ),
            [id],
            |row| {
                Ok(RebuildRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    created_at_millis: row.get(2)?,
                    updated_at_millis: row.get(3)?,
                })
            },
        )
        .optional()?;
    let mut statement = db.connection.prepare(&format!(
        "SELECT {} FROM {ITEMS} WHERE {OWNER} = ?1 ORDER BY id",
        item_columns()
    ))?;
    let items = statement
        .query_map([id], item_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|item| (item.id, item))
        .collect();
    let mut statement = db.connection.prepare(&format!(
        "SELECT path, entry FROM {CHOICES} WHERE {OWNER} = ?1"
    ))?;
    let choices = statement
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, i64>>>()?;
    Ok(State {
        rebuild,
        items,
        choices,
    })
}

fn read_item(db: &MapDb, id: i64) -> Result<Option<ItemRow>> {
    Ok(db
        .connection
        .query_row(
            &format!("SELECT {} FROM {ITEMS} WHERE id = ?1", item_columns()),
            [id],
            item_row,
        )
        .optional()?)
}

fn insert_item(db: &MapDb, item: &ItemRow) -> Result<()> {
    db.connection.execute(
        &format!(
            "INSERT INTO {ITEMS} (id, {OWNER}, kind, head, name, parent_item, parent_head, entry, position, created_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ),
        params![
            item.id,
            item.owner,
            item.kind,
            item.head,
            item.name,
            item.parent_item,
            item.parent_head,
            item.entry,
            item.position,
            item.created_at_millis
        ],
    )?;
    Ok(())
}

fn update_item(db: &MapDb, item: &ItemRow) -> Result<()> {
    db.connection.execute(
        &format!(
            "UPDATE {ITEMS} SET kind = ?2, head = ?3, name = ?4, parent_item = ?5, parent_head = ?6, \
             entry = ?7, position = ?8 WHERE id = ?1"
        ),
        params![
            item.id,
            item.kind,
            item.head,
            item.name,
            item.parent_item,
            item.parent_head,
            item.entry,
            item.position
        ],
    )?;
    Ok(())
}

fn read_choice(db: &MapDb, owner: i64, path: &str) -> Result<Option<i64>> {
    Ok(db
        .connection
        .query_row(
            &format!("SELECT entry FROM {CHOICES} WHERE {OWNER} = ?1 AND path = ?2"),
            params![owner, path],
            |row| row.get(0),
        )
        .optional()?)
}

fn write_choice(db: &MapDb, owner: i64, path: &str, entry: Option<i64>) -> Result<()> {
    match entry {
        Some(entry) => db.connection.execute(
            &format!("INSERT OR REPLACE INTO {CHOICES} ({OWNER}, path, entry) VALUES (?1, ?2, ?3)"),
            params![owner, path, entry],
        )?,
        None => db.connection.execute(
            &format!("DELETE FROM {CHOICES} WHERE {OWNER} = ?1 AND path = ?2"),
            params![owner, path],
        )?,
    };
    Ok(())
}

/// Puts a Disk Rebuild back exactly as a state says, or removes it when the state has none.
fn write_state(db: &MapDb, id: i64, state: &State) -> Result<()> {
    db.connection
        .execute(&format!("DELETE FROM {ITEMS} WHERE {OWNER} = ?1"), [id])?;
    db.connection
        .execute(&format!("DELETE FROM {CHOICES} WHERE {OWNER} = ?1"), [id])?;
    let Some(rebuild) = &state.rebuild else {
        db.connection.execute(
            &format!("DELETE FROM {SUGGESTIONS} WHERE {OWNER} = ?1"),
            [id],
        )?;
        db.connection
            .execute(&format!("DELETE FROM {REBUILDS} WHERE id = ?1"), [id])?;
        return Ok(());
    };
    db.connection.execute(
        &format!(
            "INSERT OR REPLACE INTO {REBUILDS} (id, name, created_at_millis, updated_at_millis) \
             VALUES (?1, ?2, ?3, ?4)"
        ),
        params![
            rebuild.id,
            rebuild.name,
            rebuild.created_at_millis,
            now_millis() as i64
        ],
    )?;
    for item in state.items.values() {
        insert_item(db, item)?;
    }
    for (path, entry) in &state.choices {
        write_choice(db, id, path, Some(*entry))?;
    }
    Ok(())
}

/// The rows that differ between two states of one Disk Rebuild.
fn diff_states(id: i64, before: &State, after: &State) -> Vec<RowChange> {
    let mut diff = Vec::new();
    let same_rebuild = match (&before.rebuild, &after.rebuild) {
        (Some(a), Some(b)) => a.name == b.name,
        (None, None) => true,
        _ => false,
    };
    if !same_rebuild {
        diff.push(RowChange::Rebuild {
            before: before.rebuild.clone(),
            after: after.rebuild.clone(),
        });
    }
    let ids: std::collections::BTreeSet<i64> = before
        .items
        .keys()
        .chain(after.items.keys())
        .copied()
        .collect();
    for item in ids {
        let (old, new) = (before.items.get(&item), after.items.get(&item));
        if old != new {
            diff.push(RowChange::Item {
                id: item,
                before: old.cloned(),
                after: new.cloned(),
            });
        }
    }
    let paths: std::collections::BTreeSet<&String> =
        before.choices.keys().chain(after.choices.keys()).collect();
    for path in paths {
        let (old, new) = (before.choices.get(path), after.choices.get(path));
        if old != new {
            diff.push(RowChange::Choice {
                path: path.clone(),
                owner: id,
                before: old.copied(),
                after: new.copied(),
            });
        }
    }
    diff
}

impl MapDb {
    /// Runs one change an agent asked for, and journals it. Everything happens or nothing does:
    /// a change over a limit is undone before it is reported. The first change an agent session
    /// makes to a Disk Rebuild also keeps a snapshot of it from before, to restore to.
    ///
    /// `target` is the Disk Rebuild the change is on, or none when it makes a new one. `op`
    /// returns what to tell the agent, and the Disk Rebuild it changed.
    pub fn agent_change(
        &self,
        who: &AgentConnection,
        limits: &AgentLimits,
        tool: &str,
        summary: &str,
        target: Option<i64>,
        op: impl FnOnce(&MapDb) -> Result<(Value, i64)>,
    ) -> Result<Value> {
        self.atomically(|| {
            let used = self.touch_agent_session(who)?;
            let before = match target {
                Some(id) => read_state(self, id)?,
                None => State::default(),
            };
            if let (Some(id), Some(_)) = (target, &before.rebuild) {
                self.keep_snapshot(id, &who.session, &before)?;
            }
            let (value, id) = op(self)?;
            let after = read_state(self, id)?;
            if target.is_none() && after.rebuild.is_some() {
                // It did not exist when the session began, so restoring removes it.
                self.keep_snapshot(id, &who.session, &State::default())?;
            }
            let diff = diff_states(id, &before, &after);
            if diff.is_empty() {
                return Ok(value);
            }
            let size = diff.len() as u64;
            if size > limits.per_call {
                bail!(
                    "This would change {size} rows, and one call may change at most {} (MAX_CHANGES_PER_CALL). Nothing was changed. Do it in smaller steps.",
                    limits.per_call
                );
            }
            if used + size > limits.per_session {
                bail!(
                    "This agent session has changed {used} of the {} rows it may (MAX_CHANGES_PER_SESSION), and this would change {size} more. Nothing was changed. The person can start a new agent session to go on.",
                    limits.per_session
                );
            }
            self.connection.execute(
                "INSERT INTO disk_rebuild_changes (disk_rebuild, session, tool, summary, diff, size, at_millis, undone) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                params![
                    id,
                    who.session,
                    tool,
                    summary,
                    serde_json::to_string(&diff)?,
                    size as i64,
                    now_millis() as i64
                ],
            )?;
            let change = self.connection.last_insert_rowid();
            for row in &diff {
                let mark = match row {
                    RowChange::Item {
                        id,
                        after: Some(_),
                        ..
                    } => Some(("item", id.to_string())),
                    RowChange::Choice {
                        path,
                        after: Some(_),
                        ..
                    } => Some(("choice", path.clone())),
                    RowChange::Rebuild { after: Some(_), .. } => Some(("rebuild", String::new())),
                    _ => None,
                };
                if let Some((kind, key)) = mark {
                    self.connection.execute(
                        "INSERT INTO disk_rebuild_agent_marks (change, disk_rebuild, kind, key) VALUES (?1, ?2, ?3, ?4)",
                        params![change, id, kind, key],
                    )?;
                }
            }
            self.connection.execute(
                "UPDATE agent_sessions SET changes = changes + ?2, last_at_millis = ?3 WHERE id = ?1",
                params![who.session, size as i64, now_millis() as i64],
            )?;
            Ok(value)
        })
    }

    /// Notes the agent session, and says how many rows it has changed already.
    fn touch_agent_session(&self, who: &AgentConnection) -> Result<u64> {
        let now = now_millis() as i64;
        self.connection.execute(
            "INSERT OR IGNORE INTO agent_sessions (id, client, started_at_millis, last_at_millis, changes) \
             VALUES (?1, ?2, ?3, ?3, 0)",
            params![who.session, who.client, now],
        )?;
        self.connection.execute(
            "UPDATE agent_sessions SET last_at_millis = ?2 WHERE id = ?1",
            params![who.session, now],
        )?;
        let used: i64 = self.connection.query_row(
            "SELECT changes FROM agent_sessions WHERE id = ?1",
            [&who.session],
            |row| row.get(0),
        )?;
        Ok(used as u64)
    }

    fn keep_snapshot(&self, id: i64, session: &str, state: &State) -> Result<()> {
        let journal: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM disk_rebuild_changes",
            [],
            |row| row.get(0),
        )?;
        self.connection.execute(
            "INSERT OR IGNORE INTO disk_rebuild_snapshots (disk_rebuild, session, existed, state, journal_before, taken_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                session,
                state.rebuild.is_some(),
                serde_json::to_string(state)?,
                journal,
                now_millis() as i64
            ],
        )?;
        Ok(())
    }

    /// What the agent made on a Disk Rebuild and the person has not undone.
    pub fn agent_marks(&self, disk_rebuild: i64) -> Result<AgentMarks> {
        let mut statement = self.connection.prepare_cached(
            "SELECT m.kind, m.key FROM disk_rebuild_agent_marks m \
             JOIN disk_rebuild_changes c ON c.id = m.change \
             WHERE m.disk_rebuild = ?1 AND c.undone = 0",
        )?;
        let rows = statement
            .query_map([disk_rebuild], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut marks = AgentMarks::default();
        for (kind, key) in rows {
            match kind.as_str() {
                "item" => {
                    if let Ok(id) = key.parse() {
                        marks.items.insert(id);
                    }
                }
                "choice" => {
                    marks.choices.insert(key);
                }
                _ => marks.rebuild = true,
            }
        }
        Ok(marks)
    }

    /// The Disk Rebuilds an agent made or renamed, and the person has not undone.
    pub fn agent_rebuilds(&self) -> Result<HashSet<i64>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT DISTINCT m.disk_rebuild FROM disk_rebuild_agent_marks m \
             JOIN disk_rebuild_changes c ON c.id = m.change \
             WHERE m.kind = 'rebuild' AND c.undone = 0",
        )?;
        let rows = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        Ok(rows)
    }

    fn change_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentChange> {
        Ok(AgentChange {
            id: row.get(0)?,
            disk_rebuild: row.get(1)?,
            session: row.get(2)?,
            tool: row.get(3)?,
            summary: row.get(4)?,
            size: row.get::<_, i64>(5)? as u64,
            at_millis: row.get::<_, i64>(6)? as u64,
            undone: row.get::<_, i64>(7)? != 0,
            origin: "agent",
        })
    }

    /// What agents changed, newest first, with the sessions that made it.
    pub fn agent_changes(
        &self,
        disk_rebuild: Option<i64>,
        session: Option<&str>,
        include_undone: bool,
        limit: u64,
        limits: &AgentLimits,
    ) -> Result<AgentChanges> {
        let columns = "id, disk_rebuild, session, tool, summary, size, at_millis, undone";
        let mut statement = self.connection.prepare(&format!(
            "SELECT {columns} FROM disk_rebuild_changes \
             WHERE (?1 IS NULL OR disk_rebuild = ?1) AND (?2 IS NULL OR session = ?2) \
             AND (?3 OR undone = 0) ORDER BY id DESC LIMIT ?4"
        ))?;
        let changes = statement
            .query_map(
                params![disk_rebuild, session, include_undone, limit as i64],
                Self::change_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let last = self
            .connection
            .query_row(
                &format!(
                    "SELECT {columns} FROM disk_rebuild_changes \
                     WHERE undone = 0 AND (?1 IS NULL OR disk_rebuild = ?1) ORDER BY id DESC LIMIT 1"
                ),
                [disk_rebuild],
                Self::change_row,
            )
            .optional()?;
        let mut statement = self.connection.prepare(
            "SELECT id, client, started_at_millis, last_at_millis, changes, \
             (SELECT COUNT(*) FROM disk_rebuild_changes c WHERE c.session = s.id AND c.undone = 0) \
             FROM agent_sessions s ORDER BY last_at_millis DESC, rowid DESC LIMIT 20",
        )?;
        let mut sessions = statement
            .query_map([], |row| {
                Ok(AgentSession {
                    id: row.get(0)?,
                    client: row.get(1)?,
                    started_at_millis: row.get::<_, i64>(2)? as u64,
                    last_at_millis: row.get::<_, i64>(3)? as u64,
                    changes_used: row.get::<_, i64>(4)? as u64,
                    active_changes: row.get::<_, i64>(5)? as u64,
                    disk_rebuilds: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for found in &mut sessions {
            let mut statement = self.connection.prepare(
                "SELECT DISTINCT disk_rebuild FROM disk_rebuild_changes WHERE session = ?1 AND undone = 0 ORDER BY disk_rebuild",
            )?;
            found.disk_rebuilds = statement
                .query_map([&found.id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(AgentChanges {
            changes,
            sessions,
            last,
            limits: *limits,
        })
    }

    /// Takes back the latest agent change that is not undone yet, on one Disk Rebuild or any.
    pub fn agent_undo_last(&self, disk_rebuild: Option<i64>) -> Result<UndoReport> {
        let latest: Option<(i64, String)> = self
            .connection
            .query_row(
                "SELECT id, session FROM disk_rebuild_changes WHERE undone = 0 \
                 AND (?1 IS NULL OR disk_rebuild = ?1) ORDER BY id DESC LIMIT 1",
                [disk_rebuild],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (id, session) = latest.context("There is no agent change left to undo.")?;
        self.undo_ids(vec![id], Some(session), true)
    }

    /// Takes back every change of one agent session that is not undone yet, newest first. Without
    /// a session, the latest one that has changes left. It stops at a change that something
    /// changed since, so the person's work is not lost.
    pub fn agent_undo_session(
        &self,
        session: Option<&str>,
        disk_rebuild: Option<i64>,
    ) -> Result<UndoReport> {
        let session = match session {
            Some(session) => session.to_string(),
            None => self
                .connection
                .query_row(
                    "SELECT session FROM disk_rebuild_changes WHERE undone = 0 \
                     AND (?1 IS NULL OR disk_rebuild = ?1) ORDER BY id DESC LIMIT 1",
                    [disk_rebuild],
                    |row| row.get(0),
                )
                .optional()?
                .context("There is no agent change left to undo.")?,
        };
        let mut statement = self.connection.prepare(
            "SELECT id FROM disk_rebuild_changes WHERE undone = 0 AND session = ?1 \
             AND (?2 IS NULL OR disk_rebuild = ?2) ORDER BY id DESC",
        )?;
        let ids = statement
            .query_map(params![session, disk_rebuild], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        if ids.is_empty() {
            bail!("That agent session has no change left to undo.");
        }
        self.undo_ids(ids, Some(session), false)
    }

    fn undo_ids(&self, ids: Vec<i64>, session: Option<String>, strict: bool) -> Result<UndoReport> {
        let mut undone = 0;
        let mut stopped = None;
        let mut rebuilds: Vec<i64> = Vec::new();
        let total = ids.len() as u64;
        for id in ids {
            match self.undo_one(id) {
                Ok(rebuild) => {
                    undone += 1;
                    if !rebuilds.contains(&rebuild) {
                        rebuilds.push(rebuild);
                    }
                }
                Err(error) => {
                    stopped = Some(format!("{error:#}"));
                    break;
                }
            }
        }
        if undone == 0 || (strict && stopped.is_some()) {
            if let Some(reason) = stopped {
                bail!("{reason}");
            }
        }
        Ok(UndoReport {
            undone,
            remaining: total - undone,
            stopped,
            session,
            disk_rebuilds: rebuilds,
        })
    }

    /// Takes back one journaled change, if every row it touched is still as it left it.
    fn undo_one(&self, change: i64) -> Result<i64> {
        self.atomically(|| {
            let (rebuild, diff, summary): (i64, String, String) = self.connection.query_row(
                "SELECT disk_rebuild, diff, summary FROM disk_rebuild_changes WHERE id = ?1 AND undone = 0",
                [change],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let diff: Vec<RowChange> = serde_json::from_str(&diff)?;
            self.revert(rebuild, &diff).with_context(|| {
                format!("Could not undo that change ({summary}), because something changed since")
            })?;
            self.connection.execute(
                "UPDATE disk_rebuild_changes SET undone = 1 WHERE id = ?1",
                [change],
            )?;
            Ok(rebuild)
        })
    }

    fn revert(&self, rebuild: i64, diff: &[RowChange]) -> Result<()> {
        let deleting: HashSet<i64> = diff
            .iter()
            .filter_map(|row| match row {
                RowChange::Item {
                    id, before: None, ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        let inserting: HashSet<i64> = diff
            .iter()
            .filter_map(|row| match row {
                RowChange::Item {
                    id,
                    before: Some(_),
                    after: None,
                } => Some(*id),
                _ => None,
            })
            .collect();
        for row in diff {
            let RowChange::Item { id, before, after } = row else {
                continue;
            };
            if read_item(self, *id)? != *after {
                bail!("item {id} is no longer as the agent left it");
            }
            match (before, after) {
                (None, Some(_)) => {
                    let mut statement = self
                        .connection
                        .prepare(&format!("SELECT id FROM {ITEMS} WHERE parent_item = ?1"))?;
                    let children = statement
                        .query_map([id], |row| row.get::<_, i64>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    if children.iter().any(|child| !deleting.contains(child)) {
                        bail!("something was put inside item {id} since");
                    }
                    self.connection
                        .execute(&format!("DELETE FROM {ITEMS} WHERE id = ?1"), [id])?;
                }
                (Some(old), None) => {
                    if let Some(parent) = old.parent_item {
                        if !inserting.contains(&parent) && read_item(self, parent)?.is_none() {
                            bail!("the folder item {id} was in is gone");
                        }
                    }
                    insert_item(self, old)?;
                }
                (Some(old), Some(_)) => update_item(self, old)?,
                (None, None) => {}
            }
        }
        for row in diff {
            let RowChange::Choice {
                path,
                owner,
                before,
                after,
            } = row
            else {
                continue;
            };
            if read_choice(self, *owner, path)? != *after {
                bail!("the chosen copy of {path} is no longer as the agent left it");
            }
            write_choice(self, *owner, path, *before)?;
        }
        for row in diff {
            let RowChange::Rebuild { before, after } = row else {
                continue;
            };
            let current = read_state(self, rebuild)?;
            match (before, after) {
                (None, Some(_)) => {
                    if !current.items.is_empty() || !current.choices.is_empty() {
                        bail!("something was put in the Disk Rebuild since");
                    }
                    write_state(self, rebuild, &State::default())?;
                }
                (Some(old), None) => {
                    if current.rebuild.is_some() {
                        bail!("the Disk Rebuild number {rebuild} is taken again");
                    }
                    self.connection.execute(
                        &format!(
                            "INSERT INTO {REBUILDS} (id, name, created_at_millis, updated_at_millis) VALUES (?1, ?2, ?3, ?4)"
                        ),
                        params![old.id, old.name, old.created_at_millis, now_millis() as i64],
                    )?;
                }
                (Some(old), Some(new)) => {
                    if current.rebuild.as_ref().map(|found| &found.name) != Some(&new.name) {
                        bail!("the Disk Rebuild was renamed since");
                    }
                    self.connection.execute(
                        &format!(
                            "UPDATE {REBUILDS} SET name = ?2, updated_at_millis = ?3 WHERE id = ?1"
                        ),
                        params![rebuild, old.name, now_millis() as i64],
                    )?;
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// Puts a Disk Rebuild back as it was when an agent session first changed it, and marks
    /// every agent change since as undone. Without a session, the latest one that has a snapshot.
    /// Whatever the person did to it since is lost too, which is why the person asks for this.
    pub fn agent_restore(&self, disk_rebuild: i64, session: Option<&str>) -> Result<RestoreReport> {
        self.atomically(|| {
            let found: Option<(String, String, i64)> = match session {
                Some(session) => self
                    .connection
                    .query_row(
                        "SELECT session, state, journal_before FROM disk_rebuild_snapshots \
                         WHERE disk_rebuild = ?1 AND session = ?2",
                        params![disk_rebuild, session],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?,
                None => self
                    .connection
                    .query_row(
                        "SELECT session, state, journal_before FROM disk_rebuild_snapshots \
                         WHERE disk_rebuild = ?1 ORDER BY taken_at_millis DESC, rowid DESC LIMIT 1",
                        [disk_rebuild],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?,
            };
            let (session, state, journal_before) =
                found.context("No agent session has changed that Disk Rebuild, so there is nothing to restore it to.")?;
            let state: State = serde_json::from_str(&state)?;
            write_state(self, disk_rebuild, &state)?;
            let undone = self.connection.execute(
                "UPDATE disk_rebuild_changes SET undone = 1 WHERE disk_rebuild = ?1 AND id > ?2 AND undone = 0",
                params![disk_rebuild, journal_before],
            )?;
            Ok(RestoreReport {
                disk_rebuild,
                session,
                removed: state.rebuild.is_none(),
                undone: undone as u64,
            })
        })
    }
}
