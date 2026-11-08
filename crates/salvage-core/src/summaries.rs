//! Totals for an agent to reason about, without names. Each answers a question that would
//! otherwise take hand-written SQL against the map's internal tables, and none of them returns
//! a file or folder name, so they are safe to use on a disk holding private files.

use crate::content::CLASS_NAMES;
use crate::map::MapDb;
use anyhow::{bail, Result};
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const SIZE_BAND_SQL: &str = "CASE WHEN s.length < 1048576 THEN 'under 1 MB' \
    WHEN s.length < 10485760 THEN '1-10 MB' WHEN s.length < 104857600 THEN '10-100 MB' \
    WHEN s.length < 1073741824 THEN '100 MB-1 GB' ELSE 'over 1 GB' END";

/// What `breakdown` can group found files by, and the SQL for each.
pub const BREAKDOWN_DIMENSIONS: &[(&str, &str)] = &[
    ("verdict", "COALESCE(s.verdict, 'not assessed')"),
    ("reason", "COALESCE(s.reason_code, 'not assessed')"),
    ("type", "COALESCE(s.type_group, 'not assessed')"),
    ("extension", "COALESCE(s.extension, '')"),
    ("size", SIZE_BAND_SQL),
    (
        "deleted",
        "CASE WHEN s.in_use = 0 THEN 'deleted' ELSE 'in use' END",
    ),
    ("folder", "COALESCE(n.kind, 'none')"),
];
const DEFAULT_BREAKDOWN_ROWS: usize = 50;
const MAX_BREAKDOWN_ROWS: usize = 1000;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BreakdownRequest {
    pub by: Vec<String>,
    pub verdict: Vec<String>,
    pub reason: Vec<String>,
    #[serde(rename = "type")]
    pub type_group: Vec<String>,
    pub extension: Vec<String>,
    pub deleted: Option<bool>,
    pub min_bytes: Option<u64>,
    pub limit: Option<usize>,
}

fn dimension(name: &str) -> Result<&'static str> {
    BREAKDOWN_DIMENSIONS
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(_, sql)| *sql)
        .ok_or_else(|| {
            let known: Vec<&str> = BREAKDOWN_DIMENSIONS.iter().map(|(id, _)| *id).collect();
            anyhow::anyhow!("unknown dimension {name}; use one of {}", known.join(", "))
        })
}

/// Found files, counted and summed by the dimensions asked for, largest first.
pub fn breakdown(database: &MapDb, request: &BreakdownRequest) -> Result<Value> {
    if request.by.is_empty() {
        bail!("say what to group by, for example by: [\"verdict\", \"reason\"]");
    }
    let columns: Vec<&str> = request
        .by
        .iter()
        .map(|name| dimension(name))
        .collect::<Result<_>>()?;
    let mut filters = vec!["s.is_dir = 0".to_string()];
    let mut values: Vec<SqlValue> = Vec::new();
    for (name, list) in [
        ("verdict", &request.verdict),
        ("reason", &request.reason),
        ("type", &request.type_group),
        ("extension", &request.extension),
    ] {
        if list.is_empty() {
            continue;
        }
        let marks = vec!["?"; list.len()].join(", ");
        filters.push(format!("{} IN ({marks})", dimension(name)?));
        values.extend(list.iter().map(|value| SqlValue::Text(value.clone())));
    }
    if let Some(deleted) = request.deleted {
        filters.push(format!("s.in_use = {}", i32::from(!deleted)));
    }
    if let Some(min) = request.min_bytes {
        filters.push("s.length >= ?".to_string());
        values.push(SqlValue::Integer(min as i64));
    }
    let limit = request
        .limit
        .unwrap_or(DEFAULT_BREAKDOWN_ROWS)
        .clamp(1, MAX_BREAKDOWN_ROWS);
    let selected: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(index, sql)| format!("{sql} AS d{index}"))
        .collect();
    let grouped: Vec<String> = (0..columns.len())
        .map(|index| format!("d{index}"))
        .collect();
    // The byte total is the last column selected, so the order names it by position.
    let sql = format!(
        "SELECT {}, COUNT(*), COALESCE(SUM(s.length), 0) FROM sets s LEFT JOIN nodes n ON n.head = s.node \
         WHERE {} GROUP BY {} ORDER BY {} DESC",
        selected.join(", "),
        filters.join(" AND "),
        grouped.join(", "),
        columns.len() + 2,
    );
    let mut statement = database.connection.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(values.iter()))?;
    let mut out = Vec::new();
    let (mut files, mut bytes, mut groups) = (0i64, 0i64, 0usize);
    while let Some(row) = rows.next()? {
        groups += 1;
        let count: i64 = row.get(columns.len())?;
        let size: i64 = row.get(columns.len() + 1)?;
        files += count;
        bytes += size;
        if out.len() < limit {
            let mut object = Map::new();
            for (index, name) in request.by.iter().enumerate() {
                let value: Option<String> = row.get(index)?;
                object.insert(name.clone(), json!(value));
            }
            object.insert("files".into(), json!(count));
            object.insert("bytes".into(), json!(size));
            out.push(Value::Object(object));
        }
    }
    Ok(json!({
        "rows": out,
        "totalFiles": files,
        "totalBytes": bytes,
        "groupsLeftOut": groups - out.len(),
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainGroup {
    pub head_content: String,
    pub end: String,
    pub chains: u64,
    pub clusters: u64,
    pub bytes: u64,
}

/// FAT chains no entry claims, grouped by what their first cluster holds and how they end.
pub fn chain_summary(database: &MapDb, cluster_size: u64) -> Result<Vec<ChainGroup>> {
    let mut statement = database.connection.prepare(
        "SELECT head_class, end_kind, COUNT(*), SUM(length) FROM islands \
         WHERE owners = 0 AND allocated = 0 GROUP BY head_class, end_kind ORDER BY 4 DESC",
    )?;
    let rows = statement.query_map([], |row| {
        let class: u8 = row.get(0)?;
        let clusters: i64 = row.get(3)?;
        Ok(ChainGroup {
            head_content: CLASS_NAMES
                .get(usize::from(class))
                .copied()
                .unwrap_or("unknown")
                .to_string(),
            end: row.get(1)?,
            chains: row.get::<_, i64>(2)? as u64,
            clusters: clusters as u64,
            bytes: clusters as u64 * cluster_size,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
