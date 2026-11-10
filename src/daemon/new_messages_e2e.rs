//! uubot #132: `new-messages` must return every message exactly once even
//! when a message becomes visible after another message of the same second
//! has already been returned and the cursor stopped on that second.
//!
//! Each test drives `q_new_messages` round by round against real
//! SQLCipher-encrypted fixture databases (decrypted by `DbCache`), carrying
//! the cursor file text between rounds the way the CLI does.

#[path = "../../tests/common/fixture.rs"]
mod fixture;

use super::cache::DbCache;
use super::new_msg_cursor::BOUNDARY_RECHECK_POLLS;
use super::query::{q_new_messages, Names};
use crate::cli::new_messages::{cursor_from_response, parse_cursor, render_cursor};
use fixture::{Fixture, ALICE, BOB, BOT, GROUP};
use std::collections::HashMap;

/// Polls with an unchanged `SessionTable` that still re-check the cursor
/// second.
const K: usize = BOUNDARY_RECHECK_POLLS as usize;

fn now_minus_hour() -> i64 {
    chrono::Utc::now().timestamp() - 3600
}

struct Poller {
    fx: Fixture,
    db: DbCache,
    names: Names,
    /// Text of `last_check.json` (None = no file yet).
    cursor: Option<String>,
    limit: usize,
}

impl Poller {
    async fn new(tag: &str, shards: usize, cursor: Option<String>) -> Poller {
        let fx = Fixture::new(&std::env::temp_dir(), tag, shards);
        let keys: HashMap<String, String> = fx
            .key_names()
            .into_iter()
            .map(|k| (k, fixture::KEY_HEX.to_string()))
            .collect();
        let db = DbCache::with_dirs(
            fx.db_dir.clone(),
            fx.root.join("cache"),
            fx.root.join("cache").join("_mtimes.json"),
            keys,
        )
        .await
        .unwrap();
        let names = Names {
            map: HashMap::new(),
            md5_to_uname: HashMap::new(),
            msg_db_keys: fx.shard_keys(),
            biz_msg_db_keys: Vec::new(),
            verify_flags: HashMap::new(),
        };
        Poller { fx, db, names, cursor, limit: 200 }
    }

    /// One round from an explicit cursor text; returns (labels, new cursor).
    /// The cursor goes through the CLI's own parse / render functions.
    async fn round_from(&self, cursor: Option<&str>) -> (Vec<String>, Option<String>) {
        let parsed = cursor.map(parse_cursor).unwrap_or_default();
        let resp = q_new_messages(
            &self.db,
            &self.names,
            parsed.sessions,
            parsed.boundary,
            self.limit,
            true,
            false,
        )
        .await
        .unwrap();
        let labels = resp["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| label(m["content"].as_str().unwrap_or("")))
            .collect();
        let (sessions, boundary) = cursor_from_response(&resp);
        let next = if sessions.is_empty() {
            cursor.map(str::to_string)
        } else {
            Some(render_cursor(&sessions, &boundary))
        };
        (labels, next)
    }

    /// One round from the current cursor; keeps the new cursor (an acked poll).
    async fn poll(&mut self) -> Vec<String> {
        let (labels, next) = self.round_from(self.cursor.as_deref()).await;
        self.cursor = next;
        labels
    }

    /// Poll until a round returns nothing; all labels in order.
    async fn drain(&mut self, max_rounds: usize) -> Vec<String> {
        let mut all = Vec::new();
        for _ in 0..max_rounds {
            let got = self.poll().await;
            if got.is_empty() {
                return all;
            }
            all.extend(got);
        }
        panic!("still returning messages after {} rounds", max_rounds);
    }
}

fn label(content: &str) -> String {
    content.rsplit('\n').next().unwrap_or(content).to_string()
}

fn cursor_at(ts: i64) -> Option<String> {
    Some(serde_json::json!({ "sessions": { GROUP: ts } }).to_string())
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// 1. A bot message is returned first; a member's message of the same second
//    shows up one round later.
#[tokio::test]
async fn same_second_late_row_is_returned_next_round() {
    let t = now_minus_hour();
    let mut p = Poller::new("s1", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "bot-list");
    assert_eq!(p.poll().await, strs(&["bot-list"]));
    p.fx.insert(0, None, t, ALICE, "alice-at-bot");
    assert_eq!(p.poll().await, strs(&["alice-at-bot"]));
    assert!(p.poll().await.is_empty());
}

// 2. Same sender, same text, same second, two rounds apart.
#[tokio::test]
async fn identical_message_in_a_later_round_is_returned_again() {
    let t = now_minus_hour();
    let mut p = Poller::new("s2", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, ALICE, "same-text");
    assert_eq!(p.poll().await, strs(&["same-text"]));
    p.fx.insert(0, None, t, ALICE, "same-text");
    assert_eq!(p.poll().await, strs(&["same-text"]));
    assert!(p.poll().await.is_empty());
}

// 2b. Two identical messages visible in the same round are both returned.
#[tokio::test]
async fn identical_messages_in_one_round_are_both_returned() {
    let t = now_minus_hour();
    let mut p = Poller::new("s2b", 1, cursor_at(t - 10)).await;
    p.fx.insert(0, None, t, ALICE, "same-text");
    p.fx.post(0, t, ALICE, "same-text");
    assert_eq!(p.poll().await, strs(&["same-text", "same-text"]));
    assert!(p.poll().await.is_empty());
}

// 3. Three messages of one second arrive over three rounds.
#[tokio::test]
async fn three_same_second_rows_over_three_rounds() {
    let t = now_minus_hour();
    let mut p = Poller::new("s3", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "first");
    assert_eq!(p.poll().await, strs(&["first"]));
    p.fx.insert(0, None, t, ALICE, "second");
    assert_eq!(p.poll().await, strs(&["second"]));
    p.fx.insert(0, None, t, BOB, "third");
    assert_eq!(p.poll().await, strs(&["third"]));
    assert!(p.poll().await.is_empty());
}

// 4. Two shards, same second, same local_id.
#[tokio::test]
async fn same_local_id_in_another_shard_is_a_different_message() {
    let t = now_minus_hour();
    let mut p = Poller::new("s4", 2, cursor_at(t - 10)).await;
    p.fx.insert(0, Some(7), t, BOT, "shard0-id7");
    p.fx.set_session(t);
    assert_eq!(p.poll().await, strs(&["shard0-id7"]));
    p.fx.insert(1, Some(7), t, ALICE, "shard1-id7");
    assert_eq!(p.poll().await, strs(&["shard1-id7"]));
    assert!(p.poll().await.is_empty());
}

// 5. The global limit cuts through one second.
#[tokio::test]
async fn global_limit_cut_inside_a_second_loses_nothing() {
    let t = now_minus_hour();
    let mut p = Poller::new("s5", 1, cursor_at(t - 10)).await;
    p.limit = 2;
    p.fx.insert(0, None, t, BOT, "r1");
    p.fx.insert(0, None, t, ALICE, "r2");
    p.fx.insert(0, None, t, BOB, "r3");
    p.fx.post(0, t + 1, ALICE, "r4");
    let all = p.drain(10).await;
    assert_eq!(sorted(all), strs(&["r1", "r2", "r3", "r4"]));
}

// 6. More already-returned rows of one second than the per-table limit,
//    followed by a newer message (p1 F2).
#[tokio::test]
async fn returned_rows_do_not_fill_the_per_table_limit() {
    let t = now_minus_hour();
    let mut p = Poller::new("s6", 1, cursor_at(t - 10)).await;
    p.limit = 40; // per-table limit max(40 * 5, 200) = 200
    let burst: Vec<String> = (0..201).map(|i| format!("burst-{:03}", i)).collect();
    p.fx.insert_many(0, t, ALICE, &burst);
    p.fx.post(0, t + 1, BOB, "tail");
    let all = p.drain(20).await;
    let mut want = burst.clone();
    want.push("tail".to_string());
    assert_eq!(sorted(all), sorted(want));
}

// 7. The shard's newest message is exactly the cursor second when a
//    same-second message shows up together with a newer one elsewhere (p1 F1).
#[tokio::test]
async fn shard_whose_newest_row_is_the_cursor_second_is_still_read() {
    let t = now_minus_hour();
    let mut p = Poller::new("s7", 2, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "bot-in-shard0");
    assert_eq!(p.poll().await, strs(&["bot-in-shard0"]));
    p.fx.insert(0, None, t, ALICE, "late-in-shard0");
    p.fx.post(1, t + 5, BOB, "newer-in-shard1");
    assert_eq!(sorted(p.poll().await), strs(&["late-in-shard0", "newer-in-shard1"]));
    assert!(p.poll().await.is_empty());
}

// 8. A cursor file from the old core (no boundary) does not replay the
//    cursor second after the upgrade (p1 F5).
#[tokio::test]
async fn legacy_cursor_does_not_replay_the_cursor_second() {
    let t = now_minus_hour();
    let mut p = Poller::new("s8", 1, cursor_at(t)).await;
    p.fx.post(0, t, BOT, "returned-by-old-core");
    assert!(p.poll().await.is_empty());
    p.fx.post(0, t + 2, ALICE, "after-upgrade");
    assert_eq!(p.poll().await, strs(&["after-upgrade"]));
    assert!(p.poll().await.is_empty());
}

// 9. An unacknowledged poll is replayed from the same cursor text.
#[tokio::test]
async fn replaying_an_unacked_poll_gives_the_same_result() {
    let t = now_minus_hour();
    let mut p = Poller::new("s9", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "bot-list");
    assert_eq!(p.poll().await, strs(&["bot-list"]));
    p.fx.insert(0, None, t, ALICE, "alice-at-bot");
    let before = p.cursor.clone();
    let (first, first_next) = p.round_from(before.as_deref()).await;
    let (again, again_next) = p.round_from(before.as_deref()).await;
    assert_eq!(first, strs(&["alice-at-bot"]));
    assert_eq!(again, first);
    let parse = |s: &Option<String>| -> serde_json::Value {
        serde_json::from_str(s.as_deref().unwrap()).unwrap()
    };
    assert_eq!(parse(&first_next), parse(&again_next));
}

// 10a. Inside the re-check window a quiet group is still re-read.
#[tokio::test]
async fn late_row_inside_the_recheck_window_is_returned() {
    let t = now_minus_hour();
    let mut p = Poller::new("s10a", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "bot-list");
    assert_eq!(p.poll().await, strs(&["bot-list"]));
    for _ in 0..K - 1 {
        assert!(p.poll().await.is_empty());
    }
    p.fx.insert(0, None, t, ALICE, "late-inside-window");
    assert_eq!(p.poll().await, strs(&["late-inside-window"]));
}

// 10b. After the window a quiet group is no longer re-read; the late row
//      comes with the group's next message instead of being lost (p1 F3).
#[tokio::test]
async fn late_row_after_the_window_comes_with_the_next_message() {
    let t = now_minus_hour();
    let mut p = Poller::new("s10b", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "bot-list");
    assert_eq!(p.poll().await, strs(&["bot-list"]));
    for _ in 0..K {
        assert!(p.poll().await.is_empty());
    }
    p.fx.insert(0, None, t, ALICE, "late-after-window");
    assert!(p.poll().await.is_empty());
    p.fx.post(0, t + 3, BOB, "next-message");
    assert_eq!(sorted(p.poll().await), strs(&["late-after-window", "next-message"]));
    assert!(p.poll().await.is_empty());
}

// 11. First run (no cursor file) still reads the last 24 hours, and the
//     cursor it writes protects the second it stopped on.
#[tokio::test]
async fn first_run_then_same_second_late_row() {
    let t = now_minus_hour();
    let mut p = Poller::new("s11", 1, None).await;
    p.fx.post(0, t, BOT, "bot-list");
    assert_eq!(p.poll().await, strs(&["bot-list"]));
    p.fx.insert(0, None, t, ALICE, "alice-at-bot");
    assert_eq!(p.poll().await, strs(&["alice-at-bot"]));
    assert!(p.poll().await.is_empty());
}

// 13. SessionTable lags behind the message table for two rounds (p2 F3).
#[tokio::test]
async fn lagging_session_table_neither_loses_nor_repeats() {
    let t = now_minus_hour();
    let mut p = Poller::new("s13", 1, cursor_at(t - 10)).await;
    p.fx.post(0, t, BOT, "a");
    assert_eq!(p.poll().await, strs(&["a"]));
    p.fx.insert(0, None, t, ALICE, "b");
    p.fx.insert(0, None, t + 1, BOB, "c");
    assert_eq!(sorted(p.poll().await), strs(&["b", "c"]));
    assert!(p.poll().await.is_empty());
    p.fx.set_session(t + 1);
    assert!(p.poll().await.is_empty());
}
