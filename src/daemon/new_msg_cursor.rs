//! Cursor rules for `new-messages` (uubot #132, #157).
//!
//! The cursor is one whole second per chat (`sessions`). WeChat's
//! `create_time` has one-second resolution, and a message can become visible
//! after messages of the same or later seconds were already returned: a
//! member's message a moment after the helper's own of the same second
//! (#132), or a file, sticker or system row seconds after the next message
//! (#157, measured on the mini: 10 such rows in 30 days of group 1).
//!
//! So for every chat the cursor file also keeps a *boundary*. Since #157 it
//! carries a *window*: from second `from` on, every message already returned
//! (or marked seen when the window was set up) is listed with its
//! `create_time`. While the window matches the cursor, the chat is read with
//! `create_time >= from` minus those messages, so a late message of the last
//! `WINDOW_SECONDS` is still returned once, and never twice. The cursor never
//! moves back; `from` follows the cursor at `cursor - WINDOW_SECONDS`.
//!
//! A chat with a cursor but no usable window (a boundary written by
//! `uubot.3`/`uubot.4`, one that does not parse, or one for another second)
//! is read once by the old rules, and in the same snapshot a second query
//! without a row limit lists the ids visible from `cursor - WINDOW_SECONDS`
//! up to the cursor second; they are marked seen, not returned. At the
//! cursor second only the old boundary's ids are marked (the old rules
//! return the rest); with no old boundary the whole second is (the strict
//! rule). The seed read has no limit so a busy window cannot leave rows out
//! of it (u1-r1 F1). A message visible before that first read and never returned
//! stays missed (history; out of scope of #157). A chat seen for the first
//! time keeps the strict rule from the fallback second.
//!
//! `ids` keeps the old meaning (messages of the cursor second already
//! returned), so a core rolled back to `uubot.4` still reads the file.
//! `local_id` is only used as an identity within a shard, never as an order.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// How far back a chat is re-read for late messages. #157: N3 and N3b
/// (uubot issuecomment-6950294347) found no late row with a positive lower
/// bound, so the formula gave its 30 s minimum; 300 s is the largest window
/// that needs no more re-checks than `BOUNDARY_RECHECK_POLLS` already gives.
pub const WINDOW_SECONDS: i64 = 300;

/// Seconds between two polls of the collector (`sidecar` loop, about 5 s).
pub const POLL_SECONDS: i64 = 5;

/// Polls with an unchanged session that still re-read the window:
/// `max(120, ceil(2 * WINDOW_SECONDS / POLL_SECONDS))`. Counted in polls, not
/// wall-clock time: a collection pause uses none.
pub const BOUNDARY_RECHECK_POLLS: u32 = 120;

const _: () = assert!(BOUNDARY_RECHECK_POLLS as i64 * POLL_SECONDS >= 2 * WINDOW_SECONDS);

/// Messages already returned from second `from` on, with their `create_time`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub from: i64,
    /// `"<shard rel_key>:<local_id>"` → `create_time`.
    pub seen: BTreeMap<String, i64>,
}

/// Messages of second `ts` already returned for one chat, and the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub ts: i64,
    /// Sorted, unique `"<shard rel_key>:<local_id>"` of second `ts`.
    pub ids: Vec<String>,
    /// Re-reads left while the session does not move past `ts`.
    pub left: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Window>,
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

fn local_ids_of<'a>(ids: impl Iterator<Item = &'a String>, rel_key: &str) -> Vec<i64> {
    ids.filter_map(|id| split_id(id))
        .filter(|(rel, _)| *rel == rel_key)
        .map(|(_, local_id)| local_id)
        .collect()
}

impl Boundary {
    fn window_matches(&self) -> bool {
        let Some(w) = &self.window else { return false };
        let at_ts: BTreeSet<&String> = w.seen.iter().filter(|(_, t)| **t == self.ts).map(|(id, _)| id).collect();
        w.from <= self.ts
            && w.seen.iter().all(|(id, t)| split_id(id).is_some() && *t >= w.from && *t <= self.ts)
            && self.ids.iter().collect::<BTreeSet<_>>() == at_ts
    }
}

/// Parse the `boundary` object of a cursor file. An entry that does not
/// parse cleanly is dropped: the chat then falls back to the strict rules,
/// which can only delay a late message, never return one twice. A window
/// that does not match its entry is dropped alone; the chat then sets up a
/// new one from the snapshot.
pub fn parse_boundary(value: Option<&Value>) -> BoundaryMap {
    let mut out = BoundaryMap::new();
    let Some(map) = value.and_then(Value::as_object) else {
        return out;
    };
    for (chat, entry) in map {
        // The window is parsed on its own: a broken window must not take the
        // cursor-second ids with it (u1-r1 F2).
        let mut entry = entry.clone();
        let window = entry.as_object_mut().and_then(|o| o.remove("window"));
        let Ok(boundary) = serde_json::from_value::<Boundary>(entry) else {
            continue;
        };
        let window = window.and_then(|v| serde_json::from_value::<Window>(v).ok());
        let ids: BTreeSet<String> = boundary.ids.iter().cloned().collect();
        let well_formed = ids.len() == boundary.ids.len()
            && boundary.ids.iter().all(|id| split_id(id).is_some());
        if !well_formed {
            continue;
        }
        let mut b = Boundary {
            ts: boundary.ts,
            ids: ids.into_iter().collect(),
            left: boundary.left.min(BOUNDARY_RECHECK_POLLS),
            window,
        };
        if !b.window_matches() {
            b.window = None;
        }
        out.insert(chat.clone(), b);
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
    /// Quiet chat, re-reading the window inside the re-check polls.
    Recheck,
    /// Quiet chat active since the fallback second, with a cursor but no
    /// usable window (first poll after an upgrade, a broken entry): read to
    /// set the window up, then re-checked like a chat that just moved, so a
    /// row that becomes visible after the upgrade is still returned. Older
    /// chats set their window up when they next move.
    Setup,
}

fn window_usable(prev_ts: Option<i64>, boundary: Option<&Boundary>) -> bool {
    matches!((prev_ts, boundary), (Some(ts), Some(b)) if b.ts == ts && b.window.is_some())
}

pub fn gate(session_ts: i64, prev_ts: Option<i64>, fallback_ts: i64, boundary: Option<&Boundary>) -> Gate {
    if session_ts > prev_ts.unwrap_or(fallback_ts) {
        return Gate::Changed;
    }
    match boundary {
        Some(b) if applies(prev_ts, Some(b)) && b.left > 0 => Gate::Recheck,
        _ if prev_ts.is_some_and(|ts| ts > fallback_ts) && !window_usable(prev_ts, boundary) => Gate::Setup,
        _ => Gate::Skip,
    }
}

/// How one chat is read this poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// First sight: `create_time > after` (the fallback second).
    Strict { after: i64 },
    /// No usable window: read by the old rules from `cursor`, and seed the
    /// window from `from = cursor - WINDOW_SECONDS` (`seed_clause`).
    Init { from: i64, cursor: i64, old_ids: Option<BTreeSet<String>> },
    /// Window in force: `create_time >= from` minus the seen messages.
    Window { from: i64 },
}

pub fn plan(prev_ts: Option<i64>, fallback_ts: i64, boundary: Option<&Boundary>) -> Read {
    let Some(ts) = prev_ts else {
        return Read::Strict { after: fallback_ts };
    };
    match boundary {
        Some(b) if window_usable(prev_ts, boundary) => Read::Window { from: b.window.as_ref().unwrap().from },
        _ => Read::Init {
            from: ts.saturating_sub(WINDOW_SECONDS),
            cursor: ts,
            old_ids: boundary.filter(|b| b.ts == ts).map(|b| b.ids.iter().cloned().collect()),
        },
    }
}

impl Read {
    /// The `?1` of `where_clause`.
    pub fn since(&self) -> i64 {
        match self {
            Read::Strict { after } => *after,
            Read::Init { cursor, .. } => *cursor,
            Read::Window { from } => *from,
        }
    }

    /// Lower time bound for shard routing (`find_msg_shards`).
    pub fn route_since(&self) -> i64 {
        match self {
            Read::Strict { after } => after.saturating_add(1),
            Read::Init { from, .. } | Read::Window { from } => *from,
        }
    }

    /// `local_id`s of shard `rel_key` that the query leaves out.
    pub fn excluded_in(&self, boundary: Option<&Boundary>, rel_key: &str) -> Vec<i64> {
        match (self, boundary.and_then(|b| b.window.as_ref())) {
            (Read::Window { .. }, Some(w)) => local_ids_of(w.seen.keys(), rel_key),
            (Read::Init { old_ids: Some(ids), .. }, _) => local_ids_of(ids.iter(), rel_key),
            _ => Vec::new(),
        }
    }

    /// The seed query of an `Init` read: `?1` is `from`, `?2` the cursor
    /// second; no row limit. `None` for the other reads.
    pub fn seed_clause(&self) -> Option<&'static str> {
        match self {
            Read::Init { old_ids: Some(_), .. } => Some("create_time >= ?1 AND create_time < ?2"),
            Read::Init { old_ids: None, .. } => Some("create_time >= ?1 AND create_time <= ?2"),
            _ => None,
        }
    }
}

/// `WHERE` condition for one shard table; `?1` is `Read::since`. The
/// exclusion sits in the condition, before `LIMIT`, so returned rows never
/// take up the per-table limit.
pub fn where_clause(read: &Read, excluded_local_ids: &[i64]) -> String {
    match read {
        // The old rules (uubot.4): the cursor second minus the old ids.
        Read::Strict { .. } | Read::Init { old_ids: None, .. } => "create_time > ?1".to_string(),
        Read::Init { .. } if excluded_local_ids.is_empty() => "create_time >= ?1".to_string(),
        Read::Init { .. } => {
            let list: Vec<String> = excluded_local_ids.iter().map(i64::to_string).collect();
            format!("(create_time > ?1 OR (create_time = ?1 AND local_id NOT IN ({})))", list.join(","))
        }
        Read::Window { .. } if excluded_local_ids.is_empty() => "create_time >= ?1".to_string(),
        Read::Window { .. } => {
            let list: Vec<String> = excluded_local_ids.iter().map(i64::to_string).collect();
            format!("(create_time >= ?1 AND local_id NOT IN ({}))", list.join(","))
        }
    }
}

/// Next cursor second and boundary for one chat present in `SessionTable`.
/// `returned` holds `(create_time, message_id)` of this chat's messages that
/// survived the global limit; `marked` the ids of an `Init` read's seed
/// query; `truncated` whether the global limit cut this poll; `incomplete`
/// whether a shard of this chat could not be read, so `marked` may miss
/// rows and an `Init` read must not set a window up yet (u1-r2 F3).
#[allow(clippy::too_many_arguments)]
pub fn advance(
    session_ts: i64,
    prev_ts: Option<i64>,
    prev_boundary: Option<&Boundary>,
    gate: Gate,
    read: &Read,
    returned: &[(i64, String)],
    marked: &[(i64, String)],
    truncated: bool,
    incomplete: bool,
) -> (i64, Option<Boundary>) {
    if gate == Gate::Skip {
        // Never move a known cursor back: SessionTable can lag behind the
        // message tables, and moving back would return messages again.
        let next = prev_ts.map_or(session_ts, |ts| ts.max(session_ts));
        let kept = prev_boundary.filter(|b| b.ts == next).cloned();
        return (next, kept);
    }
    if incomplete && matches!(read, Read::Init { .. }) {
        // The main read follows the old rules, so advance exactly as the old
        // core did (u1-r3 F4); a later poll sets the window up. A Setup read
        // is a read the old core would not have made: count it as Changed.
        let old_gate = if gate == Gate::Setup { Gate::Changed } else { gate };
        let old = prev_boundary.map(|b| Boundary { window: None, ..b.clone() });
        return advance_old_rules(session_ts, prev_ts, old.as_ref(), old_gate, returned);
    }
    let newest = returned.iter().map(|(ts, _)| *ts).max();
    let next = match (prev_ts, newest) {
        (Some(p), Some(m)) => p.max(m),
        (Some(p), None) => p,
        (None, Some(m)) => m,
        // First run (or a new chat) cut off by the global limit: as before,
        // start from the session's time.
        (None, None) => return (session_ts, None),
    };
    let (mut from, mut seen, prev_left) = match (read, prev_boundary.and_then(|b| b.window.as_ref())) {
        (Read::Window { .. }, Some(w)) => (w.from, w.seen.clone(), prev_boundary.map_or(0, |b| b.left)),
        (Read::Init { from, cursor, old_ids }, _) => {
            let mut seen: BTreeMap<String, i64> = marked.iter().map(|(t, id)| (id.clone(), *t)).collect();
            for id in old_ids.iter().flatten() {
                seen.insert(id.clone(), *cursor);
            }
            (*from, seen, 0)
        }
        (Read::Strict { after }, _) => (after.saturating_add(1), BTreeMap::new(), 0),
        (Read::Window { .. }, None) => unreachable!("a window read needs the window"),
    };
    seen.extend(returned.iter().map(|(t, id)| (id.clone(), *t)));
    from = from.max(next.saturating_sub(WINDOW_SECONDS));
    seen.retain(|_, t| *t >= from);
    // A new window, or new rows, starts the re-checks again.
    let fresh = !returned.is_empty() || !matches!(read, Read::Window { .. });
    let left = if fresh {
        BOUNDARY_RECHECK_POLLS
    } else if gate == Gate::Recheck && !truncated {
        prev_left.saturating_sub(1)
    } else {
        prev_left
    };
    let ids: Vec<String> = seen.iter().filter(|(_, t)| **t == next).map(|(id, _)| id.clone()).collect();
    (next, Some(Boundary { ts: next, ids, left, window: Some(Window { from, seen }) }))
}

/// `advance` of `uubot.4`, unchanged: the cursor-second rules without a
/// window. Used for a setup poll whose seed is incomplete.
fn advance_old_rules(
    session_ts: i64,
    prev_ts: Option<i64>,
    prev_boundary: Option<&Boundary>,
    gate: Gate,
    returned: &[(i64, String)],
) -> (i64, Option<Boundary>) {
    if gate == Gate::Skip {
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
        let ids = ids.into_iter().collect();
        return (max_ts, Some(Boundary { ts: max_ts, ids, left: BOUNDARY_RECHECK_POLLS, window: None }));
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
        None => (session_ts, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values are written out (300 s, 120 polls, uubot #157 W
    // decision), not taken from the constants, so a wrong window fails here.

    fn b(ts: i64, ids: &[&str], left: u32) -> Boundary {
        Boundary { ts, ids: ids.iter().map(|s| s.to_string()).collect(), left, window: None }
    }

    fn bw(ts: i64, from: i64, seen: &[(&str, i64)], left: u32) -> Boundary {
        let seen: BTreeMap<String, i64> = seen.iter().map(|(id, t)| (id.to_string(), *t)).collect();
        let ids = seen.iter().filter(|(_, t)| **t == ts).map(|(id, _)| id.clone()).collect();
        Boundary { ts, ids, left, window: Some(Window { from, seen }) }
    }

    fn ret(rows: &[(i64, &str)]) -> Vec<(i64, String)> {
        rows.iter().map(|(ts, id)| (*ts, id.to_string())).collect()
    }

    /// Run one poll for one chat against the visible rows `(create_time, id)`
    /// of one shard `m`, as `q_new_messages` does (ordered by time, the
    /// global limit cutting the prefix); return (returned, next ts, boundary).
    fn poll(
        visible: &[(i64, &str)],
        session_ts: i64,
        prev_ts: Option<i64>,
        prev: Option<&Boundary>,
        limit: usize,
    ) -> (Vec<(i64, String)>, i64, Option<Boundary>) {
        poll_with(visible, session_ts, prev_ts, prev, limit, 1000)
    }

    /// `poll` with the per-table limit of the main shard query set too.
    fn poll_with(
        visible: &[(i64, &str)],
        session_ts: i64,
        prev_ts: Option<i64>,
        prev: Option<&Boundary>,
        limit: usize,
        per_table: usize,
    ) -> (Vec<(i64, String)>, i64, Option<Boundary>) {
        let g = gate(session_ts, prev_ts, 0, prev);
        let read = plan(prev_ts, 0, prev);
        if g == Gate::Skip {
            let (ts, nb) = advance(session_ts, prev_ts, prev, g, &read, &[], &[], false, false);
            return (Vec::new(), ts, nb);
        }
        let excluded = read.excluded_in(prev, "m");
        let since = read.since();
        let clause = where_clause(&read, &excluded);
        let mut out: Vec<(i64, String)> = visible
            .iter()
            .filter(|(t, id)| {
                let local: i64 = id.rsplit_once(':').unwrap().1.parse().unwrap();
                if clause == "create_time > ?1" {
                    *t > since
                } else if clause.starts_with("(create_time > ?1 OR") {
                    *t > since || (*t == since && !excluded.contains(&local))
                } else {
                    *t >= since && !excluded.contains(&local)
                }
            })
            .map(|(t, id)| (*t, id.to_string()))
            .collect();
        out.sort();
        out.truncate(per_table);
        let marked: Vec<(i64, String)> = match (&read, read.seed_clause()) {
            (Read::Init { from, cursor, .. }, Some(seed)) => visible
                .iter()
                .filter(|(t, _)| *t >= *from && if seed.ends_with("< ?2") { *t < *cursor } else { *t <= *cursor })
                .map(|(t, id)| (*t, id.to_string()))
                .collect(),
            _ => Vec::new(),
        };
        let truncated = out.len() > limit;
        out.truncate(limit);
        let (ts, nb) = advance(session_ts, prev_ts, prev, g, &read, &out, &marked, truncated, false);
        (out, ts, nb)
    }

    #[test]
    fn constants_are_the_measured_window_and_cover_two_windows_of_rechecks() {
        assert_eq!(WINDOW_SECONDS, 300);
        assert_eq!(BOUNDARY_RECHECK_POLLS, 120);
        assert!(i64::from(BOUNDARY_RECHECK_POLLS) * POLL_SECONDS >= 2 * WINDOW_SECONDS);
    }

    #[test]
    fn gate_reads_changed_sessions_and_rechecks_only_a_matching_boundary() {
        let bd = b(100, &["m:1"], 3);
        assert_eq!(gate(101, Some(100), 0, None), Gate::Changed);
        assert_eq!(gate(100, Some(100), 0, Some(&bd)), Gate::Recheck);
        assert_eq!(gate(99, Some(100), 0, Some(&bd)), Gate::Recheck);
        // A window used up: quiet, not read.
        assert_eq!(gate(100, Some(100), 0, Some(&bw(100, 0, &[], 0))), Gate::Skip);
        // A cursor without a usable window is read once to set one up.
        assert_eq!(gate(100, Some(100), 0, None), Gate::Setup);
        assert_eq!(gate(100, Some(100), 0, Some(&b(100, &["m:1"], 0))), Gate::Setup);
        assert_eq!(gate(100, Some(100), 0, Some(&b(99, &["m:1"], 3))), Gate::Setup);
        // ... but only for a chat active since the fallback second.
        assert_eq!(gate(100, Some(100), 100, None), Gate::Skip);
        assert_eq!(gate(50, None, 40, Some(&bd)), Gate::Changed);
        assert_eq!(gate(30, None, 40, Some(&bd)), Gate::Skip);
    }

    #[test]
    fn plan_picks_strict_init_or_window() {
        assert_eq!(plan(None, 40, None), Read::Strict { after: 40 });
        let w = bw(1000, 700, &[("m:1", 1000)], 5);
        assert_eq!(plan(Some(1000), 40, Some(&w)), Read::Window { from: 700 });
        let ids: BTreeSet<String> = ["m:1".to_string()].into_iter().collect();
        assert_eq!(plan(Some(1000), 40, Some(&b(1000, &["m:1"], 5))), Read::Init { from: 700, cursor: 1000, old_ids: Some(ids) });
        assert_eq!(plan(Some(1000), 40, Some(&b(999, &["m:1"], 5))), Read::Init { from: 700, cursor: 1000, old_ids: None });
        assert_eq!(plan(Some(1000), 40, None), Read::Init { from: 700, cursor: 1000, old_ids: None });
    }

    #[test]
    fn route_and_where_follow_the_read() {
        let s = Read::Strict { after: 100 };
        let w = Read::Window { from: 70 };
        let i = Read::Init { from: 70, cursor: 100, old_ids: None };
        assert_eq!((s.route_since(), s.since()), (101, 100));
        assert_eq!((w.route_since(), w.since()), (70, 70));
        assert_eq!((i.route_since(), i.since()), (70, 100));
        assert_eq!(where_clause(&s, &[1]), "create_time > ?1");
        assert_eq!(where_clause(&i, &[1]), "create_time > ?1");
        let io = Read::Init { from: 70, cursor: 100, old_ids: Some(BTreeSet::new()) };
        assert_eq!(where_clause(&io, &[]), "create_time >= ?1");
        assert_eq!(where_clause(&io, &[3]), "(create_time > ?1 OR (create_time = ?1 AND local_id NOT IN (3)))");
        assert_eq!(i.seed_clause(), Some("create_time >= ?1 AND create_time <= ?2"));
        assert_eq!(io.seed_clause(), Some("create_time >= ?1 AND create_time < ?2"));
        assert_eq!(w.seed_clause(), None);
        assert_eq!(where_clause(&w, &[]), "create_time >= ?1");
        assert_eq!(where_clause(&w, &[3, 7]), "(create_time >= ?1 AND local_id NOT IN (3,7))");
    }

    #[test]
    fn ids_are_split_by_shard() {
        let bd = bw(9, 1, &[("message/message_0.db:7", 9), ("message/message_1.db:7", 5), ("message/message_0.db:9", 2)], 1);
        assert_eq!(Read::Window { from: 1 }.excluded_in(Some(&bd), "message/message_0.db"), vec![7, 9]);
        assert_eq!(Read::Window { from: 1 }.excluded_in(Some(&bd), "message/message_1.db"), vec![7]);
        assert!(Read::Init { from: 1, cursor: 9, old_ids: None }.excluded_in(Some(&bd), "message/message_0.db").is_empty());
        let old: BTreeSet<String> = ["message/message_0.db:4".to_string(), "message/message_1.db:5".to_string()].into_iter().collect();
        assert_eq!(Read::Init { from: 1, cursor: 9, old_ids: Some(old) }.excluded_in(None, "message/message_0.db"), vec![4]);
    }

    // #157 acceptance 2: the message of second T becomes visible only after
    // the cursor reached T+1. The old rules (uubot.4) skip it for good; the
    // window returns it on a later poll, exactly once.
    #[test]
    fn a_late_message_is_returned_once_after_the_cursor_passed_it() {
        let mut visible = vec![(1000, "m:1"), (1001, "m:3")];
        // First poll after an upgrade: old boundary at 999, no window.
        let old = b(999, &["m:0"], 3);
        let (out, ts, nb) = poll(&visible, 1001, Some(999), Some(&old), 200);
        assert_eq!(out, ret(&[(1000, "m:1"), (1001, "m:3")]));
        assert_eq!(ts, 1001);
        // m:2 of second 1000 becomes visible only now.
        visible.push((1000, "m:2"));
        let (out, ts, nb) = poll(&visible, 1001, Some(ts), nb.as_ref(), 200);
        assert_eq!(out, ret(&[(1000, "m:2")]));
        assert_eq!(ts, 1001, "the cursor never moves back");
        // Later polls never return it again.
        let mut state = (ts, nb);
        for _ in 0..130 {
            let (out, ts, nb) = poll(&visible, 1001, Some(state.0), state.1.as_ref(), 200);
            assert!(out.is_empty());
            state = (ts, nb);
        }
        // A new message moves on; nothing old comes back.
        visible.push((1010, "m:4"));
        let (out, _, _) = poll(&visible, 1010, Some(state.0), state.1.as_ref(), 200);
        assert_eq!(out, ret(&[(1010, "m:4")]));
    }

    #[test]
    fn the_old_rules_skip_the_late_message_for_good() {
        // uubot.4: `create_time >= cursor` minus the cursor-second ids only.
        let visible = [(1000, "m:1"), (1001, "m:3"), (1000, "m:2")];
        let cursor = 1001;
        let ids = ["m:3"];
        let old_rule: Vec<_> = visible.iter().filter(|(t, id)| *t > cursor || (*t == cursor && !ids.contains(id))).collect();
        assert!(old_rule.is_empty(), "the old rule never returns m:2");
    }

    #[test]
    fn init_marks_what_is_visible_and_keeps_the_old_cursor_second_rules() {
        // Old boundary applies at 1000 with m:5 returned; m:6 of 1000 is
        // not returned yet; 990 and 995 were visible before.
        let visible = [(990, "m:1"), (995, "m:2"), (1000, "m:5"), (1000, "m:6"), (1003, "m:7")];
        let old = b(1000, &["m:5"], 4);
        let (out, ts, nb) = poll(&visible, 1003, Some(1000), Some(&old), 200);
        assert_eq!(out, ret(&[(1000, "m:6"), (1003, "m:7")]));
        assert_eq!(ts, 1003);
        let w = nb.unwrap();
        assert_eq!(w.window.as_ref().unwrap().from, 703);
        assert_eq!(w.window.as_ref().unwrap().seen.len(), 5);
        assert_eq!(w.ids, vec!["m:7".to_string()]);
        assert_eq!(w.left, 120);
        // Without an old boundary for the cursor second: the strict rule,
        // the cursor second is marked seen too.
        let (out, _, nb) = poll(&visible, 1003, Some(1000), None, 200);
        assert_eq!(out, ret(&[(1003, "m:7")]));
        assert!(nb.unwrap().window.unwrap().seen.contains_key("m:6"));
        // A quiet chat with an old boundary re-checks once and sets up its
        // window without returning anything old.
        let (out, ts, nb) = poll(&visible[..4], 1000, Some(1000), Some(&old), 200);
        assert_eq!(out, ret(&[(1000, "m:6")]));
        assert_eq!(ts, 1000);
        assert_eq!(nb.unwrap().window.unwrap().from, 700);
    }

    #[test]
    fn a_quiet_chat_sets_its_window_up_and_rechecks() {
        let visible = [(990, "m:1"), (1000, "m:2")];
        let (out, ts, nb) = poll(&visible, 1000, Some(1000), None, 200);
        assert!(out.is_empty());
        assert_eq!(ts, 1000);
        let nb = nb.unwrap();
        assert_eq!(nb.left, 120);
        assert_eq!(nb.window.as_ref().unwrap().seen.len(), 2);
        assert_eq!(gate(1000, Some(1000), 0, Some(&nb)), Gate::Recheck);
        let (out, _, again) = poll(&[(990, "m:1"), (1000, "m:2"), (996, "m:5")], 1000, Some(1000), Some(&nb), 200);
        assert_eq!(out, ret(&[(996, "m:5")]));
        let mut nb = again.unwrap();
        nb.left = 0;
        // A row visible only after the setup comes with the chat's next change.
        let visible = [(990, "m:1"), (1000, "m:2"), (997, "m:3"), (1004, "m:4")];
        let (out, _, _) = poll(&visible, 1004, Some(1000), Some(&nb), 200);
        assert_eq!(out, ret(&[(997, "m:3"), (1004, "m:4")]));
    }

    // u1-r1 F1: a window busier than the per-table limit. The main read keeps
    // the old rules, so it is not filled by old rows; the seed read has no
    // limit, so no old row is left out of the window and returned later.
    #[test]
    fn a_busy_window_neither_replays_nor_blocks() {
        let ids: Vec<String> = (0..8).map(|i| format!("m:{}", i)).collect();
        let mut visible: Vec<(i64, &str)> = (0..5).map(|i| (990 + i as i64, ids[i].as_str())).collect();
        visible.push((1000, ids[5].as_str()));
        visible.push((1000, ids[6].as_str()));
        visible.push((1001, ids[7].as_str()));
        let old = b(1000, &["m:5"], 0);
        let (out, ts, nb) = poll_with(&visible, 1001, Some(1000), Some(&old), 200, 3);
        assert_eq!(out, ret(&[(1000, "m:6"), (1001, "m:7")]));
        assert_eq!(ts, 1001);
        let nb = nb.unwrap();
        assert_eq!(nb.window.as_ref().unwrap().seen.len(), 8);
        let (out, _, _) = poll_with(&visible, 1001, Some(1001), Some(&nb), 200, 3);
        assert!(out.is_empty());
    }

    // The seed read must not mark the cursor second's unreturned rows when
    // the old rules still owe them: a global limit of 1 returns them one by one.
    #[test]
    fn setup_under_a_small_global_limit_still_returns_the_cursor_second() {
        let visible = [(1000, "m:5"), (1000, "m:6"), (1000, "m:7"), (1000, "m:8")];
        let mut state = (1000, Some(b(1000, &["m:5"], 3)));
        let mut all = Vec::new();
        for _ in 0..4 {
            let (out, ts, nb) = poll(&visible, 1000, Some(state.0), state.1.as_ref(), 1);
            all.extend(out);
            state = (ts, nb);
        }
        assert_eq!(all, ret(&[(1000, "m:6"), (1000, "m:7"), (1000, "m:8")]));
    }

    // u1-r2 F3: a shard that could not be read leaves the seed incomplete;
    // the poll advances by the old rules and sets no window up.
    #[test]
    fn an_incomplete_setup_keeps_the_old_rules() {
        let old = b(1000, &["m:5"], 4);
        let read = plan(Some(1000), 0, Some(&old));
        let (ts, nb) = advance(1000, Some(1000), Some(&old), Gate::Recheck, &read, &ret(&[(1000, "m:6")]), &[], false, true);
        assert_eq!(ts, 1000);
        assert_eq!(nb, Some(b(1000, &["m:5", "m:6"], 120)));
        let (ts, nb) = advance(1003, Some(1000), Some(&old), Gate::Changed, &read, &[], &[], false, true);
        assert_eq!((ts, nb), (1000, Some(old.clone())));
        // No old boundary and nothing returned: no boundary, as the old core
        // (u1-r3 F4: an empty one would turn the strict rule into `>=`).
        let strict = plan(Some(1000), 0, None);
        assert_eq!(advance(1000, Some(1000), None, Gate::Setup, &strict, &[], &[], false, true), (1000, None));
        // A quiet Recheck counts down, as the old core.
        let (_, nb) = advance(1000, Some(1000), Some(&old), Gate::Recheck, &read, &[], &[], false, true);
        assert_eq!(nb.unwrap().left, 3);
        // A window read is not affected: unread rows stay unseen and come later.
        let w = bw(1000, 700, &[("m:5", 1000)], 4);
        let (_, nb) = advance(1000, Some(1000), Some(&w), Gate::Recheck, &Read::Window { from: 700 }, &[], &[], false, true);
        assert!(nb.unwrap().window.is_some());
    }

    #[test]
    fn the_window_follows_the_cursor_and_forgets_older_ids() {
        let prev = bw(1000, 700, &[("m:1", 700), ("m:2", 900), ("m:3", 1000)], 9);
        let (ts, nb) = advance(1100, Some(1000), Some(&prev), Gate::Changed, &Read::Window { from: 700 }, &ret(&[(1100, "m:4")]), &[], false, false);
        assert_eq!(ts, 1100);
        let nb = nb.unwrap();
        let w = nb.window.unwrap();
        assert_eq!(w.from, 800);
        assert_eq!(w.seen.keys().cloned().collect::<Vec<_>>(), vec!["m:2", "m:3", "m:4"]);
        assert_eq!(nb.ids, vec!["m:4".to_string()]);
        assert_eq!(nb.left, 120);
    }

    #[test]
    fn rechecks_count_down_only_on_quiet_untruncated_polls() {
        let prev = bw(1000, 700, &[("m:3", 1000)], 2);
        let w = Read::Window { from: 700 };
        let (_, nb) = advance(1000, Some(1000), Some(&prev), Gate::Recheck, &w, &[], &[], false, false);
        assert_eq!(nb.unwrap().left, 1);
        // Cut by the global limit (p1 F2): keep the count.
        let (_, nb) = advance(1000, Some(1000), Some(&prev), Gate::Recheck, &w, &[], &[], true, false);
        assert_eq!(nb.unwrap().left, 2);
        // Read because the session moved (rows cut off or not visible yet).
        let (ts, nb) = advance(1004, Some(1000), Some(&prev), Gate::Changed, &w, &[], &[], false, false);
        assert_eq!((ts, nb), (1000, Some(prev.clone())));
        // First run cut off by the global limit: start from the session time.
        assert_eq!(advance(104, None, None, Gate::Changed, &Read::Strict { after: 0 }, &[], &[], true, false), (104, None));
    }

    #[test]
    fn a_cut_poll_returns_the_rest_next_time_without_repeats() {
        let visible: Vec<(i64, String)> = (0..5).map(|i| (1000 + i / 2, format!("m:{}", i))).collect();
        let vis: Vec<(i64, &str)> = visible.iter().map(|(t, id)| (*t, id.as_str())).collect();
        let w = bw(999, 699, &[], 5);
        let mut all = Vec::new();
        let mut state = (999, Some(w));
        for _ in 0..4 {
            let (out, ts, nb) = poll(&vis, 1002, Some(state.0), state.1.as_ref(), 2);
            all.extend(out);
            state = (ts, nb);
        }
        all.sort();
        let mut want = visible.clone();
        want.sort();
        assert_eq!(all, want);
    }

    #[test]
    fn first_sight_keeps_the_strict_rule_from_the_fallback() {
        let read = Read::Strict { after: 500 };
        let (ts, nb) = advance(900, None, None, Gate::Changed, &read, &ret(&[(600, "m:1"), (900, "m:2")]), &[], false, false);
        assert_eq!(ts, 900);
        let w = nb.unwrap().window.unwrap();
        assert_eq!(w.from, 600);
        assert_eq!(w.seen.len(), 2);
        let (ts, nb) = advance(900, None, None, Gate::Changed, &Read::Strict { after: 800 }, &ret(&[(900, "m:2")]), &[], false, false);
        assert_eq!(ts, 900);
        assert_eq!(nb.unwrap().window.unwrap().from, 801);
    }

    #[test]
    fn skipped_sessions_never_move_back_and_drop_stale_boundaries() {
        let prev = b(100, &["m:1"], 0);
        let r = Read::Init { from: 0, cursor: 100, old_ids: None };
        assert_eq!(advance(99, Some(100), Some(&prev), Gate::Skip, &r, &[], &[], false, false), (100, Some(prev.clone())));
        assert_eq!(advance(120, Some(100), Some(&prev), Gate::Skip, &r, &[], &[], false, false), (120, None));
        assert_eq!(advance(90, None, None, Gate::Skip, &Read::Strict { after: 0 }, &[], &[], false, false), (90, None));
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
            "win": {"ts": 5, "ids": ["a:2"], "left": 1, "window": {"from": 2, "seen": {"a:1": 3, "a:2": 5}}},
            "win_after_ts": {"ts": 5, "ids": [], "left": 1, "window": {"from": 2, "seen": {"a:1": 6}}},
            "win_ids_differ": {"ts": 5, "ids": [], "left": 1, "window": {"from": 2, "seen": {"a:2": 5}}},
            "win_bad": {"ts": 5, "ids": ["a:2"], "left": 1, "window": {"from": "x"}},
            "win_seen_bad": {"ts": 5, "ids": ["a:2"], "left": 1, "window": {"from": 2, "seen": {"a:2": "5"}}},
        });
        let parsed = parse_boundary(Some(&v));
        assert_eq!(
            parsed.keys().collect::<Vec<_>>(),
            vec!["ok", "too_many_left", "win", "win_after_ts", "win_bad", "win_ids_differ", "win_seen_bad"]
        );
        assert_eq!(parsed["ok"].ids, vec!["message/message_0.db:1", "message/message_0.db:2"]);
        assert_eq!(parsed["too_many_left"].left, 120);
        assert!(parsed["win"].window.is_some());
        assert!(parsed["win_after_ts"].window.is_none());
        assert!(parsed["win_ids_differ"].window.is_none());
        // A broken window alone keeps the cursor-second ids (u1-r1 F2).
        for k in ["win_bad", "win_seen_bad"] {
            assert!(parsed[k].window.is_none());
            assert_eq!(parsed[k].ids, vec!["a:2".to_string()]);
        }
        assert!(parse_boundary(Some(&serde_json::json!([1]))).is_empty());
        assert!(parse_boundary(None).is_empty());
    }

    #[test]
    fn a_rolled_back_core_still_reads_the_cursor_second() {
        // uubot.4 deserialises Boundary without `window`; unknown fields are ignored.
        #[derive(Deserialize)]
        struct Old {
            ts: i64,
            ids: Vec<String>,
            left: u32,
        }
        let nb = bw(5, 2, &[("a:1", 3), ("a:2", 5)], 7);
        let old: Old = serde_json::from_value(serde_json::to_value(&nb).unwrap()).unwrap();
        assert_eq!((old.ts, old.ids, old.left), (5, vec!["a:2".to_string()], 7));
    }

    #[test]
    fn the_cursor_file_stays_far_below_its_size_limit_at_a_peak() {
        // p1 F4: ten messages a second for the whole window in three chats.
        let mut map = BoundaryMap::new();
        for chat in 0..3 {
            let seen: Vec<(String, i64)> = (0..3000).map(|i| (format!("message/message_0.db:{}", 1_000_000 + i), 1000 + i / 10)).collect();
            let seen_ref: Vec<(&str, i64)> = seen.iter().map(|(id, t)| (id.as_str(), *t)).collect();
            map.insert(format!("{}@chatroom", chat), bw(1299, 999, &seen_ref, 120));
        }
        let text = serde_json::to_string(&map).unwrap();
        assert!(text.len() < 1024 * 1024 / 2, "cursor text {} bytes", text.len());
    }
}
