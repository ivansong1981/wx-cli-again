use super::output::{emit_warnings, print_response, OutputOpts};
use super::transport;
use crate::daemon::new_msg_cursor::{parse_boundary, BoundaryMap};
use crate::ipc::Request;
use anyhow::Result;
use std::collections::HashMap;

fn state_file() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".wx-cli")
        .join("last_check.json")
}

/// 游标文件 `last_check.json` 的内容。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Cursor {
    /// per-session 时间戳快照；None 触发首次运行逻辑
    pub sessions: Option<HashMap<String, i64>>,
    /// 每个会话在游标那一秒已经返回过的消息（uubot #132）
    pub boundary: BoundaryMap,
}

/// 解析游标文件。
/// 格式：{ "sessions": { "username": timestamp, ... }, "boundary": { ... } }
/// 旧格式（只有 timestamp 字段）没有 sessions → 首次运行逻辑，重新全量获取；
/// 没有 boundary（旧核心写的文件）或其中某一项不合格式 → 那个会话按旧规则。
pub(crate) fn parse_cursor(text: &str) -> Cursor {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Cursor::default();
    };
    // 空 map 也是合法状态（账号无任何会话），返回 Some(empty) 而非 None
    // 这样不会误触发全量历史拉取
    let sessions = v.get("sessions").and_then(|s| s.as_object()).map(|obj| {
        obj.iter()
            .filter_map(|(k, v)| v.as_i64().map(|ts| (k.clone(), ts)))
            .collect()
    });
    Cursor {
        sessions,
        boundary: parse_boundary(v.get("boundary")),
    }
}

pub(crate) fn render_cursor(sessions: &HashMap<String, i64>, boundary: &BoundaryMap) -> String {
    serde_json::json!({ "sessions": sessions, "boundary": boundary }).to_string()
}

/// daemon 响应里的 new_state / new_boundary。
pub(crate) fn cursor_from_response(data: &serde_json::Value) -> (HashMap<String, i64>, BoundaryMap) {
    let sessions = data
        .get("new_state")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_i64().map(|ts| (k.clone(), ts)))
                .collect()
        })
        .unwrap_or_default();
    (sessions, parse_boundary(data.get("new_boundary")))
}

fn load_state() -> Cursor {
    std::fs::read_to_string(state_file())
        .map(|text| parse_cursor(&text))
        .unwrap_or_default()
}

fn save_state(sessions: &HashMap<String, i64>, boundary: &BoundaryMap) -> Result<()> {
    let path = state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_cursor(sessions, boundary))?;
    Ok(())
}

pub fn cmd_new_messages(limit: usize, opts: OutputOpts) -> Result<()> {
    let cursor = load_state();
    let (with_meta, debug_source) = opts.request_flags();
    let resp = transport::send(Request::NewMessages {
        state: cursor.sessions,
        boundary: Some(cursor.boundary),
        limit,
        with_meta,
        debug_source,
    })?;

    // 保存 daemon 返回的 new_state / new_boundary
    let (sessions, boundary) = cursor_from_response(&resp.data);
    if !sessions.is_empty() {
        let _ = save_state(&sessions, &boundary);
    }

    // 边界只给下一轮用，不进输出：输出的字段和以前一样
    let mut data = resp.data;
    if let Some(obj) = data.as_object_mut() {
        obj.remove("new_boundary");
    }
    emit_warnings(&data);
    print_response(&data, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::new_msg_cursor::Boundary;

    #[test]
    fn old_cursor_files_keep_their_sessions_and_get_no_boundary() {
        let c = parse_cursor(r#"{"sessions":{"g@chatroom":100}}"#);
        assert_eq!(c.sessions, Some(HashMap::from([("g@chatroom".to_string(), 100)])));
        assert!(c.boundary.is_empty());
        assert_eq!(parse_cursor(r#"{"timestamp":5}"#), Cursor::default());
        assert_eq!(parse_cursor("not json"), Cursor::default());
        assert_eq!(parse_cursor(r#"{"sessions":{}}"#).sessions, Some(HashMap::new()));
    }

    #[test]
    fn cursor_round_trips_and_a_broken_boundary_falls_back() {
        let sessions = HashMap::from([("g@chatroom".to_string(), 100)]);
        let mut boundary = BoundaryMap::new();
        boundary.insert(
            "g@chatroom".to_string(),
            Boundary { ts: 100, ids: vec!["message/message_0.db:3".to_string()], left: 7 },
        );
        let text = render_cursor(&sessions, &boundary);
        assert_eq!(parse_cursor(&text), Cursor { sessions: Some(sessions.clone()), boundary });
        let broken = r#"{"sessions":{"g@chatroom":100},"boundary":{"g@chatroom":{"ts":"x"}}}"#;
        assert_eq!(parse_cursor(broken), Cursor { sessions: Some(sessions), boundary: BoundaryMap::new() });
    }

    #[test]
    fn cursor_for_a_thousand_chats_stays_small() {
        // uubot's reader adapter refuses cursors over 1 MiB (MAX_CURSOR_BYTES).
        let mut sessions = HashMap::new();
        let mut boundary = BoundaryMap::new();
        for i in 0..1000 {
            let chat = format!("{:011}@chatroom", 10_000_000_000u64 + i);
            sessions.insert(chat.clone(), 1_794_000_000 + i as i64);
            boundary.insert(
                chat,
                Boundary {
                    ts: 1_794_000_000 + i as i64,
                    ids: vec![format!("message/message_{}.db:{}", i % 4, 100_000 + i)],
                    left: 120,
                },
            );
        }
        let bytes = render_cursor(&sessions, &boundary).len();
        assert!(bytes < 150_000, "cursor for 1000 chats is {} bytes", bytes);
    }
}
