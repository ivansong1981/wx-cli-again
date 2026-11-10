//! Cursor rules for `new-messages` (uubot #132).
//!
//! The cursor is one whole second per chat (`sessions`). WeChat's
//! `create_time` has one-second resolution and a member's message can become
//! visible a moment after a message of the same second (typically the
//! helper's own, written locally) was already returned. A strict
//! `create_time > cursor` never returns such a message.
//!
//! So for every chat the cursor file also keeps a *boundary*: the messages of
//! the cursor second that were already returned, as `"<shard>:<local_id>"`.
//! While the boundary matches the cursor, the chat is read with
//! `create_time >= cursor` minus those messages, and a quiet chat (whose
//! `SessionTable.last_timestamp` did not move past the cursor) is still
//! re-read for `BOUNDARY_RECHECK_POLLS` polls. Without a matching boundary
//! (old cursor files, first run) the old strict rules apply, so nothing that
//! was returned before is returned again.
//!
//! `local_id` is only used as an identity within a shard, never as an order.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Polls with an unchanged session that still re-read the cursor second.
/// Counted in polls, not wall-clock time: a collection pause uses none.
pub const BOUNDARY_RECHECK_POLLS: u32 = 120;

/// Messages of second `ts` already returned for one chat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub ts: i64,
    /// Sorted, unique `"<shard rel_key>:<local_id>"`.
    pub ids: Vec<String>,
    /// Re-reads left while the session does not move past `ts`.
    pub left: u32,
}

/// Chat username → boundary. Ordered, so the cursor text is deterministic.
pub type BoundaryMap = BTreeMap<String, Boundary>;

pub fn message_id(rel_key: &str, local_id: i64) -> String {
    format!("{}:{}", rel_key, local_id)
}

fn split_id(id: &str) -> Option<(&str, i64)> {
    let (rel_key, local_id) = id.rsplit_once(':')?;
    if rel_key.is_empty() {
        return None;
    }
    local_id.parse().ok().map(|n| (rel_key, n))
}

impl Boundary {
    fn new(ts: i64, ids: BTreeSet<String>) -> Boundary {
        Boundary {
            ts,
            ids: ids.into_iter().collect(),
            left: BOUNDARY_RECHECK_POLLS,
        }
    }

    /// `local_id`s of this boundary that live in shard `rel_key`.
    pub fn local_ids_in(&self, rel_key: &str) -> Vec<i64> {
        self.ids
            .iter()
            .filter_map(|id| split_id(id))
            .filter(|(rel, _)| *rel == rel_key)
            .map(|(_, local_id)| local_id)
            .collect()
    }
}

/// Parse the `boundary` object of a cursor file. An entry that does not
/// parse cleanly is dropped: the chat then falls back to the strict rules,
/// which can only delay a same-second message, never return one twice.
pub fn parse_boundary(value: Option<&Value>) -> BoundaryMap {
    let mut out = BoundaryMap::new();
    let Some(map) = value.and_then(Value::as_object) else {
        return out;
    };
    for (chat, entry) in map {
        let Ok(boundary) = serde_json::from_value::<Boundary>(entry.clone()) else {
            continue;
        };
        let ids: BTreeSet<String> = boundary.ids.iter().cloned().collect();
        let well_formed = ids.len() == boundary.ids.len()
            && boundary.ids.iter().all(|id| split_id(id).is_some());
        if !well_formed {
            continue;
        }
        out.insert(
            chat.clone(),
            Boundary {
                ts: boundary.ts,
                ids: ids.into_iter().collect(),
                left: boundary.left.min(BOUNDARY_RECHECK_POLLS),
            },
        );
    }
    out
}

/// The boundary is in force only when it describes the cursor second.
pub fn applies(prev_ts: Option<i64>, boundary: Option<&Boundary>) -> bool {
    matches!((prev_ts, boundary), (Some(ts), Some(b)) if b.ts == ts)
}

/// Why a chat is read this poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Not read.
    Skip,
    /// `SessionTable` moved past the cursor (or the chat is new).
    Changed,
    /// Quiet chat, re-reading the cursor second inside the window.
    Recheck,
}

pub fn gate(session_ts: i64, prev_ts: Option<i64>, fallback_ts: i64, boundary: Option<&Boundary>) -> Gate {
    if session_ts > prev_ts.unwrap_or(fallback_ts) {
        return Gate::Changed;
    }
    match boundary {
        Some(b) if applies(prev_ts, Some(b)) && b.left > 0 => Gate::Recheck,
        _ => Gate::Skip,
    }
}

/// Lower time bound for shard routing (`find_msg_shards`).
pub fn route_since(since_ts: i64, applies: bool) -> i64 {
    if applies {
        since_ts
    } else {
        since_ts.saturating_add(1)
    }
}

/// `WHERE` condition for one shard table; `?1` is the cursor second. The
/// exclusion sits in the condition, before `LIMIT`, so returned rows never
/// take up the per-table limit.
pub fn where_clause(applies: bool, excluded_local_ids: &[i64]) -> String {
    if !applies {
        return "create_time > ?1".to_string();
    }
    if excluded_local_ids.is_empty() {
        return "create_time >= ?1".to_string();
    }
    let list: Vec<String> = excluded_local_ids.iter().map(i64::to_string).collect();
    format!(
        "(create_time > ?1 OR (create_time = ?1 AND local_id NOT IN ({})))",
        list.join(",")
    )
}

/// Next cursor second and boundary for one chat present in `SessionTable`.
/// `returned` holds `(create_time, message_id)` of this chat's messages that
/// survived the global limit.
pub fn advance(
    session_ts: i64,
    prev_ts: Option<i64>,
    prev_boundary: Option<&Boundary>,
    gate: Gate,
    returned: &[(i64, String)],
) -> (i64, Option<Boundary>) {
    if gate == Gate::Skip {
        // Never move a known cursor back: SessionTable can lag behind the
        // message tables, and moving back would return messages again.
        let next = prev_ts.map_or(session_ts, |ts| ts.max(session_ts));
        let kept = prev_boundary.filter(|b| b.ts == next).cloned();
        return (next, kept);
    }
    if let Some(max_ts) = returned.iter().map(|(ts, _)| *ts).max() {
        let mut ids: BTreeSet<String> = returned
            .iter()
            .filter(|(ts, _)| *ts == max_ts)
            .map(|(_, id)| id.clone())
            .collect();
        if applies(prev_ts, prev_boundary) && prev_ts == Some(max_ts) {
            ids.extend(prev_boundary.into_iter().flat_map(|b| b.ids.iter().cloned()));
        }
        return (max_ts, Some(Boundary::new(max_ts, ids)));
    }
    match prev_ts {
        Some(ts) => {
            let kept = prev_boundary.filter(|b| b.ts == ts).map(|b| {
                let mut b = b.clone();
                if gate == Gate::Recheck {
                    b.left = b.left.saturating_sub(1);
                }
                b
            });
            (ts, kept)
        }
        // First run (or a new chat) cut off by the global limit: as before,
        // start from the session's time.
        None => (session_ts, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values are written out (120 polls, uubot #132 plan v3), not
    // taken from BOUNDARY_RECHECK_POLLS, so a wrong window fails here.

    fn b(ts: i64, ids: &[&str], left: u32) -> Boundary {
        Boundary { ts, ids: ids.iter().map(|s| s.to_string()).collect(), left }
    }

    fn ret(rows: &[(i64, &str)]) -> Vec<(i64, String)> {
        rows.iter().map(|(ts, id)| (*ts, id.to_string())).collect()
    }

    #[test]
    fn gate_reads_changed_sessions_and_rechecks_only_a_matching_boundary() {
        let bd = b(100, &["m:1"], 3);
        assert_eq!(gate(101, Some(100), 0, None), Gate::Changed);
        assert_eq!(gate(100, Some(100), 0, None), Gate::Skip);
        assert_eq!(gate(100, Some(100), 0, Some(&bd)), Gate::Recheck);
        // SessionTable lagging behind the cursor still re-checks.
        assert_eq!(gate(99, Some(100), 0, Some(&bd)), Gate::Recheck);
        assert_eq!(gate(100, Some(100), 0, Some(&b(100, &["m:1"], 0))), Gate::Skip);
        assert_eq!(gate(100, Some(100), 0, Some(&b(99, &["m:1"], 3))), Gate::Skip);
        // No cursor: compare with the fallback, never re-check.
        assert_eq!(gate(50, None, 40, Some(&bd)), Gate::Changed);
        assert_eq!(gate(30, None, 40, Some(&bd)), Gate::Skip);
    }

    #[test]
    fn route_and_where_keep_the_old_rules_without_a_boundary() {
        assert_eq!(route_since(100, false), 101);
        assert_eq!(route_since(100, true), 100);
        assert_eq!(where_clause(false, &[1, 2]), "create_time > ?1");
        assert_eq!(where_clause(true, &[]), "create_time >= ?1");
        assert_eq!(
            where_clause(true, &[3, 7]),
            "(create_time > ?1 OR (create_time = ?1 AND local_id NOT IN (3,7)))"
        );
    }

    #[test]
    fn local_ids_are_split_by_shard() {
        let bd = b(1, &["message/message_0.db:7", "message/message_1.db:7", "message/message_0.db:9"], 1);
        assert_eq!(bd.local_ids_in("message/message_0.db"), vec![7, 9]);
        assert_eq!(bd.local_ids_in("message/message_1.db"), vec![7]);
        assert!(bd.local_ids_in("message/message_2.db").is_empty());
    }

    #[test]
    fn advance_records_the_newest_second_and_merges_a_repeat_of_the_cursor_second() {
        let prev = b(100, &["m:1"], 5);
        let (ts, nb) = advance(100, Some(100), Some(&prev), Gate::Recheck, &ret(&[(100, "m:2")]));
        assert_eq!(ts, 100);
        assert_eq!(nb, Some(b(100, &["m:1", "m:2"], 120)));

        let (ts, nb) = advance(105, Some(100), Some(&prev), Gate::Changed, &ret(&[(100, "m:2"), (103, "m:3"), (103, "m:4")]));
        assert_eq!(ts, 103);
        assert_eq!(nb, Some(b(103, &["m:3", "m:4"], 120)));
    }

    #[test]
    fn advance_without_rows_counts_down_rechecks_only() {
        let prev = b(100, &["m:1"], 2);
        assert_eq!(advance(100, Some(100), Some(&prev), Gate::Recheck, &[]), (100, Some(b(100, &["m:1"], 1))));
        // Read because the session moved (rows cut off or not visible yet): keep as is.
        assert_eq!(advance(104, Some(100), Some(&prev), Gate::Changed, &[]), (100, Some(prev.clone())));
        // Window used up: the boundary stays until the cursor moves on.
        let spent = b(100, &["m:1"], 0);
        assert_eq!(advance(100, Some(100), Some(&spent), Gate::Skip, &[]), (100, Some(spent.clone())));
        // First run cut off by the global limit: start from the session time.
        assert_eq!(advance(104, None, None, Gate::Changed, &[]), (104, None));
    }

    #[test]
    fn skipped_sessions_never_move_back_and_drop_stale_boundaries() {
        let prev = b(100, &["m:1"], 0);
        assert_eq!(advance(99, Some(100), Some(&prev), Gate::Skip, &[]), (100, Some(prev.clone())));
        assert_eq!(advance(120, Some(100), Some(&prev), Gate::Skip, &[]), (120, None));
        assert_eq!(advance(90, None, None, Gate::Skip, &[]), (90, None));
    }

    #[test]
    fn malformed_boundary_entries_are_dropped() {
        let v = serde_json::json!({
            "ok": {"ts": 5, "ids": ["message/message_0.db:2", "message/message_0.db:1"], "left": 9},
            "too_many_left": {"ts": 5, "ids": [], "left": 100000},
            "bad_id": {"ts": 5, "ids": ["message/message_0.db:x"], "left": 1},
            "no_shard": {"ts": 5, "ids": [":4"], "left": 1},
            "dup": {"ts": 5, "ids": ["a:1", "a:1"], "left": 1},
            "not_an_object": 7,
            "missing_left": {"ts": 5, "ids": []},
        });
        let parsed = parse_boundary(Some(&v));
        assert_eq!(parsed.keys().collect::<Vec<_>>(), vec!["ok", "too_many_left"]);
        assert_eq!(parsed["ok"].ids, vec!["message/message_0.db:1", "message/message_0.db:2"]);
        assert_eq!(parsed["too_many_left"].left, 120);
        assert!(parse_boundary(Some(&serde_json::json!([1]))).is_empty());
        assert!(parse_boundary(None).is_empty());
    }
}
