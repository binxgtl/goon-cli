use crate::common::*;
use goon::session::{repair_interrupted, SessionRoute, SessionStore};

fn route(f: &Fixture) -> SessionRoute {
    SessionRoute { cwd: f.cwd.to_string_lossy().into(), provider: "test".into(), model: "model".into(), base_url: "http://localhost/v1".into() }
}

fn root(f: &Fixture) -> PathBuf {
    f.root.join("sessions")
}

fn open(f: &Fixture, resume: Option<&str>) -> Result<SessionStore> {
    SessionStore::open(&root(f), route(f), resume, None)
}

fn push_user(store: &SessionStore, text: &str) {
    store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": text }));
}

#[test]
fn an_all_digit_eight_char_prefix_resumes_by_prefix_not_as_a_list_index() {
    let f = fixture();
    let seed = open(&f, None).unwrap();
    push_user(&seed, "digit-prefixed session");
    seed.save().unwrap();
    let digit_id = "12345678-aaaa-4bbb-8ccc-dddddddddddd";
    let mut raw: Value = serde_json::from_str(&std::fs::read_to_string(root(&f).join(format!("{}.json", seed.id()))).unwrap()).unwrap();
    raw["id"] = json!(digit_id);
    std::fs::write(root(&f).join(format!("{digit_id}.json")), raw.to_string()).unwrap();
    let seed_id = seed.id();
    seed.close().unwrap();
    let by_digits = open(&f, Some("12345678")).unwrap();
    assert_eq!(by_digits.id(), digit_id);
    by_digits.close().unwrap();
    let by_index = open(&f, Some("1")).unwrap();
    assert!([digit_id.to_string(), seed_id].contains(&by_index.id()));
    by_index.close().unwrap();
}

#[test]
fn the_session_lock_prevents_two_owners_and_resume_retains_history() {
    let f = fixture();
    let store = open(&f, None).unwrap();
    let id = store.id();
    assert_match(&open(&f, Some(&id)).err().unwrap().to_string(), "already open");
    push_user(&store, "remember");
    store.save().unwrap();
    store.close().unwrap();
    let resumed = open(&f, Some("latest")).unwrap();
    assert_eq!(resumed.session.lock().unwrap().messages[0]["content"], "remember");
    assert_eq!(resumed.id(), id);
    resumed.close().unwrap();
}

#[test]
fn crash_repair_preserves_completed_results_and_adds_unknown_outcomes_once() {
    let mut messages = vec![
        json!({ "role": "assistant", "content": [
            { "type": "tool-call", "toolCallId": "one", "toolName": "write_file", "input": {} },
            { "type": "tool-call", "toolCallId": "two", "toolName": "bash", "input": {} },
        ] }),
        json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "one", "toolName": "write_file", "output": { "type": "text", "value": "saved" } }] }),
    ];
    assert!(repair_interrupted(&mut messages));
    let last = messages.last().unwrap();
    assert_eq!(last["content"][0]["toolCallId"], "two");
    assert_eq!(last["content"][0]["output"]["type"], "error-text");
    assert!(!repair_interrupted(&mut messages));
}

#[test]
fn resume_rejects_another_workspace_and_releases_the_acquired_lock() {
    let f = fixture();
    let store = open(&f, None).unwrap();
    let id = store.id();
    store.close().unwrap();
    let mut other = route(&f);
    other.cwd = f.root.to_string_lossy().into();
    assert_match(&SessionStore::open(&root(&f), other, Some(&id), None).err().unwrap().to_string(), "different workspace");
    open(&f, Some(&id)).unwrap().close().unwrap();
}

#[test]
fn a_dead_process_lock_can_be_recovered_and_archives_do_not_duplicate_listings() {
    let f = fixture();
    let store = open(&f, None).unwrap();
    let id = store.id();
    store.close().unwrap();
    std::fs::write(root(&f).join(format!("{id}.lock")), "2147483647").unwrap();
    std::fs::copy(root(&f).join(format!("{id}.json")), root(&f).join(format!("{id}.compact-1.json"))).unwrap();
    let resumed = open(&f, Some(&id)).unwrap();
    assert_eq!(SessionStore::list(&root(&f)).unwrap().len(), 1);
    resumed.close().unwrap();
}

#[test]
fn short_prefix_resume_exclude_id_on_latest_and_fork_to_new_file() {
    let f = fixture();
    let s1 = open(&f, None).unwrap();
    push_user(&s1, "First task in session 1");
    s1.save().unwrap();
    let forked = s1.fork_to_new_file(Some("Forked branch")).unwrap();
    let s1_id = s1.id();
    s1.close().unwrap();
    let by_prefix = open(&f, Some(&forked[..8])).unwrap();
    assert_eq!(by_prefix.id(), forked);
    {
        let s = by_prefix.session.lock().unwrap();
        assert_eq!(s.title.as_deref(), Some("Forked branch"));
        assert_eq!(s.messages.len(), 1);
    }
    let latest_other = SessionStore::open(&root(&f), route(&f), Some("latest"), Some(&forked)).unwrap();
    assert_eq!(latest_other.id(), s1_id);
    by_prefix.close().unwrap();
    latest_other.close().unwrap();
}
