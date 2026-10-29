use crate::common::*;
use goon::eventlog::*;

fn sessions(f: &Fixture) -> PathBuf {
    let root = f.root.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn writer(path: &Path, id: &str, secrets: &[&str]) -> EventLogWriter {
    let secrets: Vec<String> = secrets.iter().map(|s| s.to_string()).collect();
    EventLogWriter::open(path, Some(id), &secrets, Box::new(|e| panic!("event log error: {e}")))
}

fn seqs(path: &Path) -> Vec<u64> {
    read_event_log(path).unwrap().envelopes.iter().map(|e| e.seq).collect()
}

fn types(path: &Path) -> Vec<String> {
    read_event_log(path).unwrap().envelopes.iter().map(|e| e.kind.clone()).collect()
}

#[test]
fn seq_sidecar_and_reopen() {
    let f = fixture();
    let path = sessions(&f).join("s1.events.jsonl");
    let mut w = writer(&path, "s1", &[]);
    w.append(&json!({ "type": "session", "id": "s1" }));
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    let first = read_event_log(&path).unwrap();
    assert!(!first.truncated);
    assert_eq!(seqs(&path), vec![1, 2]);
    assert_eq!(read_last_seq(&path), 2);
    assert!(event_log_sidecar_path(&path).exists());
    let mut again = writer(&path, "s1", &[]);
    again.append(&json!({ "type": "todo", "todos": [] }));
    again.close();
    assert_eq!(seqs(&path), vec![1, 2, 3]);
}

#[test]
fn env_credentials_are_redacted_without_explicit_secrets() {
    let f = fixture();
    let path = sessions(&f).join("envsecret.events.jsonl");
    let secret = "sk-env-secret-1234567890";
    {
        let _env = env(&[("GOON_API_KEY", Some(secret))]);
        let mut w = writer(&path, "envsecret", &[]);
        w.append(&json!({ "type": "text", "text": format!("key {secret}") }));
        w.append(&json!({ "type": "done", "text": "done" }));
        w.close();
    }
    assert!(!std::fs::read_to_string(&path).unwrap().contains(secret));
}

#[test]
fn reopen_repairs_a_torn_tail_before_appending() {
    let f = fixture();
    let path = sessions(&f).join("torn.events.jsonl");
    let mut w = writer(&path, "torn", &[]);
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    let body = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{body}{{\"v\":1,\"seq\":2,\"at\":\"x\",\"sessionId\":\"torn\",\"type\":\"text\",\"event\":{{\"type\":\"text\",\"text\":\"half")).unwrap();
    let mut reopened = writer(&path, "torn", &[]);
    reopened.append(&json!({ "type": "todo", "todos": [] }));
    reopened.append(&json!({ "type": "session", "id": "torn" }));
    reopened.close();
    let result = read_event_log(&path).unwrap();
    assert!(!result.truncated);
    assert_eq!(seqs(&path), vec![1, 2, 3]);
    assert_eq!(types(&path), vec!["todo", "todo", "session"]);
}

#[test]
fn complete_final_line_without_newline_gets_a_separator() {
    let f = fixture();
    let path = sessions(&f).join("nonewline.events.jsonl");
    let mut w = writer(&path, "nonewline", &[]);
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    let body = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, body.trim_end_matches('\n')).unwrap();
    let mut reopened = writer(&path, "nonewline", &[]);
    reopened.append(&json!({ "type": "session", "id": "nonewline" }));
    reopened.close();
    assert!(!read_event_log(&path).unwrap().truncated);
    assert_eq!(seqs(&path), vec![1, 2]);
}

#[test]
fn sensitive_tool_content_is_dropped_but_path_and_counts_stay() {
    let f = fixture();
    let path = sessions(&f).join("sensitive.events.jsonl");
    let mut w = writer(&path, "sensitive", &[]);
    w.append(&json!({ "type": "tool", "name": "read_file", "state": "done", "meta": { "kind": "read", "summary": ".env", "outputPreview": "TOP_SECRET=1", "diff": "+PRIVATE_KEY=1", "omittedLines": 3, "added": 1, "removed": 0 } }));
    w.append(&json!({ "type": "tool", "name": "write_file", "state": "done", "meta": { "kind": "edit", "summary": "certs/server.pem", "diff": "-----BEGIN PRIVATE KEY-----\nsecret\n", "added": 2, "removed": 0 } }));
    w.append(&json!({ "type": "tool_stream", "name": "write_file", "state": "delta", "preview": "{\"path\":\"certs/server.pem\",\"content\":\"secret\"}" }));
    w.append(&json!({ "type": "tool_stream", "name": "bash", "state": "delta", "preview": "{\"command\":\"cat .env\"}" }));
    w.append(&json!({ "type": "tool", "name": "bash", "state": "done", "meta": { "kind": "bash", "summary": "cat .env", "outputPreview": "SECRET=1" } }));
    w.append(&json!({ "type": "tool", "name": "read_file", "state": "done", "meta": { "kind": "read", "summary": "notes.txt", "outputPreview": "note body" } }));
    w.close();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(!body.contains("TOP_SECRET") && !body.contains("PRIVATE KEY") && !body.contains("SECRET=1"));
    let envelopes = read_event_log(&path).unwrap().envelopes;
    let first = &envelopes[0].event["meta"];
    assert_eq!(first["summary"], ".env");
    assert!(first.get("outputPreview").is_none() && first.get("diff").is_none() && first.get("omittedLines").is_none());
    assert_eq!(envelopes[5].event["meta"]["outputPreview"], "note body");
    assert_eq!(envelopes[2].event["preview"], "");
    assert_eq!(envelopes[3].event["preview"], "");
}

#[test]
fn switch_session_starts_a_new_file_and_the_caller_writes_its_session_envelope() {
    let f = fixture();
    let root = sessions(&f);
    let first = root.join("first.events.jsonl");
    let mut w = writer(&first, "first", &[]);
    let cwd = f.cwd.to_string_lossy().to_string();
    w.append(&json!({ "type": "session", "id": "first", "cwd": cwd }));
    w.append(&json!({ "type": "user", "text": "old turn" }));
    w.switch_session("second", Some(root.join("second.events.jsonl")));
    w.append(&json!({ "type": "session", "id": "second", "cwd": cwd }));
    w.append(&json!({ "type": "user", "text": "hello" }));
    w.append(&json!({ "type": "done", "text": "ok", "denied": false, "sessionId": "second" }));
    w.close();
    let second = read_event_log(&root.join("second.events.jsonl")).unwrap();
    assert!(!second.truncated);
    assert_eq!(second.envelopes.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["session", "user", "done"]);
    assert!(second.envelopes.iter().all(|e| e.session_id == "second"));
    assert_eq!(second.envelopes[0].event["id"], "second");
    assert_eq!(second.envelopes[0].event["cwd"], cwd.as_str());
    assert_eq!(types(&first), vec!["session", "user"]);
}

#[test]
fn sidecar_ahead_wins_and_a_stale_sidecar_falls_back_to_the_tail() {
    let f = fixture();
    let path = sessions(&f).join("sidecar.events.jsonl");
    let mut w = writer(&path, "sidecar", &[]);
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    let sidecar = event_log_sidecar_path(&path);
    std::fs::write(&sidecar, "5\n").unwrap();
    assert_eq!(read_last_seq(&path), 5);
    std::fs::write(&sidecar, "0\n").unwrap();
    assert_eq!(read_last_seq(&path), 1);
    std::fs::write(&sidecar, "garbage").unwrap();
    assert_eq!(read_last_seq(&path), 1);
}

#[test]
fn a_torn_tail_is_reported_and_not_parsed() {
    let f = fixture();
    let path = sessions(&f).join("s2.events.jsonl");
    let mut w = writer(&path, "s2", &[]);
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    let body = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{body}{{\"v\":1,\"seq\":99")).unwrap();
    let result = read_event_log(&path).unwrap();
    assert!(result.truncated);
    assert_eq!(result.envelopes.len(), 1);
}

#[test]
fn very_long_lines_keep_their_sequence() {
    let f = fixture();
    let root = sessions(&f);
    for (id, size) in [("s3", 100 * 1024), ("s4", 1_100_000)] {
        let path = root.join(format!("{id}.events.jsonl"));
        let mut w = writer(&path, id, &[]);
        w.append(&json!({ "type": "tool", "payload": "x".repeat(size) }));
        w.close();
        std::fs::remove_file(event_log_sidecar_path(&path)).unwrap();
        let mut reopened = writer(&path, id, &[]);
        reopened.append(&json!({ "type": "todo", "todos": [] }));
        reopened.close();
        assert_eq!(read_last_seq(&path), 2, "{id}");
    }
}

#[test]
fn redacts_literals_split_across_stream_deltas() {
    let f = fixture();
    let path = sessions(&f).join("s5.events.jsonl");
    let secret = "sk-live-abcdef1234567890";
    let mut w = writer(&path, "s5", &[secret]);
    w.append(&json!({ "type": "text", "text": format!("prefix {}", &secret[..10]) }));
    w.append(&json!({ "type": "text", "text": format!("{} suffix", &secret[10..]) }));
    w.append(&json!({ "type": "tool", "meta": { "apiKey": secret, "note": "Bearer abcdefghijklmnopqrstuvwxyz" } }));
    w.append(&json!({ "type": "tool", "meta": { "json": format!("{{\"apiKey\":\"{secret}\"}}") } }));
    w.append(&json!({ "type": "done", "text": "done" }));
    w.close();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(!body.contains(secret) && !body.contains("abcdefghijklmnop"));
    let text: String = read_event_log(&path).unwrap().envelopes.iter().filter(|e| e.kind == "text").map(|e| e.event["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(text, "prefix [REDACTED] suffix");
}

#[test]
fn stream_redactor_keeps_a_tail() {
    let secret = "sk-live-abcdef1234567890";
    let mut r = StreamRedactor::new(&[secret.to_string()]);
    let combined = format!("{}{}{}", r.push(&format!("hello {}", &secret[..4])), r.push(&format!("{} bye", &secret[4..])), r.flush_all());
    assert!(!combined.contains(secret));
    assert!(combined.contains("[REDACTED]"));
}

#[test]
fn normalize_event_envelope_rejects_unsupported_versions() {
    assert!(normalize_event_envelope(&json!({ "v": 2, "seq": 1, "at": "x", "sessionId": "s", "type": "todo", "event": {} }), None, None).is_err());
}

#[test]
fn delete_session_removes_log_and_sidecar() {
    let f = fixture();
    let root = sessions(&f);
    let store = session::SessionStore::open(&root, session::SessionRoute { cwd: f.cwd.to_string_lossy().into(), provider: "openai-compatible".into(), model: "m".into(), base_url: "http://127.0.0.1:1/v1".into() }, None, None).unwrap();
    store.save().unwrap();
    let id = store.id();
    store.close().unwrap();
    let path = root.join(format!("{id}.events.jsonl"));
    let mut w = writer(&path, &id, &[]);
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    assert!(path.exists() && event_log_sidecar_path(&path).exists());
    session::SessionStore::delete_session(&root, &id).unwrap();
    assert!(!root.join(format!("{id}.json")).exists());
    assert!(!path.exists() && !event_log_sidecar_path(&path).exists());
}

#[test]
fn is_sensitive_path_flags_common_secrets() {
    for p in ["/tmp/project/.env", "/tmp/project/.env.example", "/tmp/project/server.pem", "/tmp/project/secrets/token.txt"] {
        assert!(redact::is_sensitive_path(p), "{p}");
    }
    assert!(!redact::is_sensitive_path("/tmp/project/src/index.ts"));
}

#[cfg(unix)]
#[test]
fn log_files_are_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let path = sessions(&f).join("mode.events.jsonl");
    let mut w = writer(&path, "mode", &[]);
    w.append(&json!({ "type": "todo", "todos": [] }));
    w.close();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
