//! uubot #132, binary level: the real `wx new-messages --json --with-meta`
//! (CLI → daemon → cursor file) against SQLCipher-encrypted fixtures, laid
//! out the way uubot's reader adapter lays them out (`HOME/.wx-cli/
//! config.json` + `all_keys.json`; the daemon is stopped after every round).
//!
//! With `WX132_GOLDEN_DIR` set, every scenario also writes its rounds
//! (cursor text before, stdout, cursor text after) to `<dir>/<scenario>.json`
//! for uubot's contract test. Only synthetic data goes into those files.

#[path = "common/fixture.rs"]
mod fixture;

use fixture::{Fixture, ALICE, BOB, BOT, GROUP};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn now_minus_hour() -> i64 {
    chrono::Utc::now().timestamp() - 3600
}

struct Cli {
    name: &'static str,
    fx: Fixture,
    home: PathBuf,
    state: PathBuf,
    limit: Option<usize>,
    rounds: Vec<Value>,
}

impl Cli {
    fn new(name: &'static str, shards: usize, cursor: Option<String>) -> Cli {
        // A short base keeps HOME/.wx-cli/daemon.sock under the 104-byte limit.
        let fx = Fixture::new(Path::new("/tmp"), name, shards);
        let state = fx.root.join("state");
        let home = state.join("home");
        let wx_home = home.join(".wx-cli");
        std::fs::create_dir_all(state.join("tmp")).unwrap();
        std::fs::create_dir_all(&wx_home).unwrap();
        let keys: serde_json::Map<String, Value> = fx
            .key_names()
            .into_iter()
            .map(|k| (k, json!({ "enc_key": fixture::KEY_HEX })))
            .collect();
        std::fs::write(wx_home.join("all_keys.json"), Value::Object(keys).to_string()).unwrap();
        std::fs::write(
            wx_home.join("config.json"),
            json!({
                "db_dir": fx.db_dir,
                "keys_file": "all_keys.json",
                "decrypted_dir": "decrypted",
            })
            .to_string(),
        )
        .unwrap();
        if let Some(text) = cursor {
            std::fs::write(wx_home.join("last_check.json"), text).unwrap();
        }
        Cli { name, fx, home, state, limit: None, rounds: Vec::new() }
    }

    fn cursor_path(&self) -> PathBuf {
        self.home.join(".wx-cli").join("last_check.json")
    }

    fn read_cursor(&self) -> Option<String> {
        std::fs::read_to_string(self.cursor_path()).ok()
    }

    fn wx(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_wx"))
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("TMPDIR", self.state.join("tmp"))
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .current_dir(&self.state)
            .output()
            .unwrap()
    }

    fn stop_daemon(&self) {
        let out = self.wx(&["daemon", "stop"]);
        assert!(out.status.success(), "daemon stop failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// One adapter-style round: run the core, then stop its daemon.
    fn poll(&mut self) -> Vec<String> {
        let before = self.read_cursor();
        let mut args = vec!["new-messages", "--json", "--with-meta"];
        let limit_text;
        if let Some(limit) = self.limit {
            limit_text = limit.to_string();
            args.extend(["-n", limit_text.as_str()]);
        }
        let out = self.wx(&args);
        self.stop_daemon();
        assert!(
            out.status.success(),
            "new-messages failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let payload: Value = serde_json::from_str(&stdout).unwrap();
        self.rounds.push(json!({
            "cursor_before": before,
            "stdout": stdout,
            "cursor_after": self.read_cursor(),
        }));
        payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let content = m["content"].as_str().unwrap_or("");
                content.rsplit('\n').next().unwrap_or(content).to_string()
            })
            .collect()
    }

    /// Write the golden rounds when asked to; call before asserting so the
    /// rounds of a failing (pre-fix) core are kept too.
    fn finish(&self) {
        if let Ok(dir) = std::env::var("WX132_GOLDEN_DIR") {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let doc = json!({ "scenario": self.name, "chat": GROUP, "rounds": self.rounds });
            std::fs::write(
                dir.join(format!("{}.json", self.name)),
                serde_json::to_string_pretty(&doc).unwrap(),
            )
            .unwrap();
        }
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = self.wx(&["daemon", "stop"]);
    }
}

fn cursor_at(ts: i64) -> Option<String> {
    Some(json!({ "sessions": { GROUP: ts } }).to_string())
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

// Scenario numbers follow the plan (uubot #132 v3).

#[test]
fn s1_same_second_late_row() {
    let t = now_minus_hour();
    let mut c = Cli::new("s1", 1, cursor_at(t - 10));
    c.fx.post(0, t, BOT, "bot-list");
    let r1 = c.poll();
    c.fx.insert(0, None, t, ALICE, "alice-at-bot");
    let r2 = c.poll();
    let r3 = c.poll();
    c.finish();
    assert_eq!(r1, strs(&["bot-list"]));
    assert_eq!(r2, strs(&["alice-at-bot"]));
    assert!(r3.is_empty());
}

#[test]
fn s2_identical_message_in_a_later_round() {
    let t = now_minus_hour();
    let mut c = Cli::new("s2", 1, cursor_at(t - 10));
    c.fx.post(0, t, ALICE, "same-text");
    let r1 = c.poll();
    c.fx.insert(0, None, t, ALICE, "same-text");
    let r2 = c.poll();
    let r3 = c.poll();
    c.finish();
    assert_eq!(r1, strs(&["same-text"]));
    assert_eq!(r2, strs(&["same-text"]));
    assert!(r3.is_empty());
}

#[test]
fn s2b_identical_messages_in_one_round() {
    let t = now_minus_hour();
    let mut c = Cli::new("s2b", 1, cursor_at(t - 10));
    c.fx.insert(0, None, t, ALICE, "same-text");
    c.fx.post(0, t, ALICE, "same-text");
    let r1 = c.poll();
    let r2 = c.poll();
    c.finish();
    assert_eq!(r1, strs(&["same-text", "same-text"]));
    assert!(r2.is_empty());
}

#[test]
fn s4_same_local_id_in_two_shards() {
    let t = now_minus_hour();
    let mut c = Cli::new("s4", 2, cursor_at(t - 10));
    c.fx.insert(0, Some(7), t, BOT, "shard0-id7");
    c.fx.set_session(t);
    let r1 = c.poll();
    c.fx.insert(1, Some(7), t, ALICE, "shard1-id7");
    let r2 = c.poll();
    let r3 = c.poll();
    c.finish();
    assert_eq!(r1, strs(&["shard0-id7"]));
    assert_eq!(r2, strs(&["shard1-id7"]));
    assert!(r3.is_empty());
}

#[test]
fn s5_global_limit_cut_inside_a_second() {
    let t = now_minus_hour();
    let mut c = Cli::new("s5", 1, cursor_at(t - 10));
    c.limit = Some(2);
    c.fx.insert(0, None, t, BOT, "r1");
    c.fx.insert(0, None, t, ALICE, "r2");
    c.fx.insert(0, None, t, BOB, "r3");
    c.fx.post(0, t + 1, ALICE, "r4");
    let mut all = Vec::new();
    for _ in 0..4 {
        all.extend(c.poll());
    }
    c.finish();
    assert_eq!(sorted(all), strs(&["r1", "r2", "r3", "r4"]));
    assert!(c.rounds.len() == 4);
}

#[test]
fn s8_legacy_cursor_is_not_replayed() {
    let t = now_minus_hour();
    let mut c = Cli::new("s8", 1, cursor_at(t));
    c.fx.post(0, t, BOT, "returned-by-old-core");
    let r1 = c.poll();
    c.fx.post(0, t + 2, ALICE, "after-upgrade");
    let r2 = c.poll();
    c.finish();
    assert!(r1.is_empty());
    assert_eq!(r2, strs(&["after-upgrade"]));
}

#[test]
fn s13_lagging_session_table() {
    let t = now_minus_hour();
    let mut c = Cli::new("s13", 1, cursor_at(t - 10));
    c.fx.post(0, t, BOT, "a");
    let r1 = c.poll();
    c.fx.insert(0, None, t, ALICE, "b");
    c.fx.insert(0, None, t + 1, BOB, "c");
    let r2 = c.poll();
    let r3 = c.poll();
    c.fx.set_session(t + 1);
    let r4 = c.poll();
    c.finish();
    assert_eq!(r1, strs(&["a"]));
    assert_eq!(sorted(r2), strs(&["b", "c"]));
    assert!(r3.is_empty());
    assert!(r4.is_empty());
}
