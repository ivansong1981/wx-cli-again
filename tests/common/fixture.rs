//! Synthetic, SQLCipher-encrypted WeChat-like databases for the
//! `new-messages` cursor tests (uubot #132).
//!
//! The files are encrypted with the bundled SQLCipher and a raw key, the same
//! format the core decrypts for real WeChat 4 databases, so the tests go
//! through the core's own decryption path. Everything here is made-up data.

#![allow(dead_code)]

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const KEY_HEX: &str = "8f3a1c5e7b9d2f4a6c8e0b1d3f5a7c9e1b3d5f7a9c2e4b6d8f0a2c4e6b8d0f1a";
pub const GROUP: &str = "51515151@chatroom";
pub const SESSION_DB: &str = "session/session.db";

/// Name2Id rowids double as `real_sender_id`.
pub const BOT: i64 = 1;
pub const ALICE: i64 = 2;
pub const BOB: i64 = 3;
const USERNAMES: [&str; 3] = ["wxid_fixturebot", "wxid_fixturealice", "wxid_fixturebob"];

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn shard_key(index: usize) -> String {
    format!("message/message_{}.db", index)
}

pub fn sender_username(sender: i64) -> &'static str {
    USERNAMES[(sender - 1) as usize]
}

pub fn msg_table(username: &str) -> String {
    format!("Msg_{:x}", md5::compute(username.as_bytes()))
}

pub struct Fixture {
    pub root: PathBuf,
    pub db_dir: PathBuf,
    pub shards: usize,
}

fn open_encrypted(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", KEY_HEX))
        .unwrap();
    conn
}

impl Fixture {
    /// A fresh root under `base` (short paths keep the daemon socket under the
    /// macOS 104-byte limit).
    pub fn new(base: &Path, tag: &str, shards: usize) -> Fixture {
        let root = base.join(format!(
            "wx132-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst),
            tag
        ));
        let _ = std::fs::remove_dir_all(&root);
        let db_dir = root.join("db_storage");
        std::fs::create_dir_all(db_dir.join("session")).unwrap();
        std::fs::create_dir_all(db_dir.join("message")).unwrap();

        let session = open_encrypted(&db_dir.join(SESSION_DB));
        session
            .execute_batch(
                "CREATE TABLE SessionTable(
                    username TEXT PRIMARY KEY, last_timestamp INTEGER,
                    unread_count INTEGER, summary TEXT, last_msg_type INTEGER,
                    last_msg_sender TEXT, last_sender_display_name TEXT);",
            )
            .unwrap();
        drop(session);

        for index in 0..shards {
            let conn = open_encrypted(&db_dir.join(shard_key(index)));
            conn.execute_batch(&format!(
                "CREATE TABLE Name2Id(user_name TEXT);
                 CREATE TABLE Timestamp(timestamp INTEGER);
                 INSERT INTO Timestamp VALUES (0);
                 CREATE TABLE [{}](
                    local_id INTEGER PRIMARY KEY AUTOINCREMENT, local_type INTEGER,
                    create_time INTEGER, real_sender_id INTEGER,
                    message_content TEXT, WCDB_CT_message_content INTEGER);",
                msg_table(GROUP)
            ))
            .unwrap();
            for name in USERNAMES {
                conn.execute("INSERT INTO Name2Id(user_name) VALUES (?1)", [name])
                    .unwrap();
            }
        }
        let fixture = Fixture { root, db_dir, shards };
        fixture.assert_encrypted();
        fixture
    }

    pub fn shard_keys(&self) -> Vec<String> {
        (0..self.shards).map(shard_key).collect()
    }

    /// Every database key the core needs, for `all_keys.json` or `DbCache`.
    pub fn key_names(&self) -> Vec<String> {
        let mut keys = vec![SESSION_DB.to_string()];
        keys.extend(self.shard_keys());
        keys
    }

    fn assert_encrypted(&self) {
        for key in self.key_names() {
            let raw = std::fs::read(self.db_dir.join(&key)).unwrap();
            assert!(raw.len() >= 4096, "{} too small", key);
            assert_ne!(&raw[..16], b"SQLite format 3\x00", "{} is not encrypted", key);
        }
    }

    /// Make sure the next write lands on a new mtime so caches notice it.
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    /// Set the group's `SessionTable.last_timestamp`.
    pub fn set_session(&self, ts: i64) {
        Self::tick();
        let conn = open_encrypted(&self.db_dir.join(SESSION_DB));
        conn.execute(
            "INSERT INTO SessionTable(username, last_timestamp) VALUES (?1, ?2)
             ON CONFLICT(username) DO UPDATE SET last_timestamp = excluded.last_timestamp",
            params![GROUP, ts],
        )
        .unwrap();
    }

    /// Insert one text message; returns its `local_id`. The text is the label
    /// the tests look for in the returned `content`.
    pub fn insert(&self, shard: usize, local_id: Option<i64>, ts: i64, sender: i64, text: &str) -> i64 {
        Self::tick();
        let conn = open_encrypted(&self.db_dir.join(shard_key(shard)));
        let content = format!("{}:\n{}", sender_username(sender), text);
        conn.execute(
            &format!(
                "INSERT INTO [{}](local_id, local_type, create_time, real_sender_id,
                                  message_content, WCDB_CT_message_content)
                 VALUES (?1, 1, ?2, ?3, ?4, 0)",
                msg_table(GROUP)
            ),
            params![local_id, ts, sender, content],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Insert many same-time messages in one transaction (no per-row tick).
    pub fn insert_many(&self, shard: usize, ts: i64, sender: i64, texts: &[String]) {
        Self::tick();
        let mut conn = open_encrypted(&self.db_dir.join(shard_key(shard)));
        let tx = conn.transaction().unwrap();
        for text in texts {
            tx.execute(
                &format!(
                    "INSERT INTO [{}](local_type, create_time, real_sender_id,
                                      message_content, WCDB_CT_message_content)
                     VALUES (1, ?1, ?2, ?3, 0)",
                    msg_table(GROUP)
                ),
                params![ts, sender, format!("{}:\n{}", sender_username(sender), text)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    /// Insert a message and move the session forward to its time, the way a
    /// message that WeChat has fully synced looks.
    pub fn post(&self, shard: usize, ts: i64, sender: i64, text: &str) -> i64 {
        let id = self.insert(shard, None, ts, sender, text);
        self.set_session(ts);
        id
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
