use crate::common::*;
use goon::files::{FileBackup, WorkspaceFiles};
use goon::process::{run_command, CommandError, RunOptions};

fn ws(f: &Fixture) -> WorkspaceFiles {
    WorkspaceFiles::new(&f.cwd)
}

fn err<T>(r: Result<T>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.to_string(),
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn existing_files_require_a_fresh_read_and_edits_preserve_file_mode() {
    let f = fixture();
    let files = ws(&f);
    let path = f.write("script.sh", "first\n");
    chmod(&path, 0o755);
    assert_match(&err(files.write("script.sh", "second")), "Read the current file");
    files.read("script.sh", 1, 200).unwrap();
    std::fs::write(&path, "changed elsewhere\n").unwrap();
    assert_match(&err(files.edit("script.sh", "changed", "edited", false)), "changed");
    files.read("script.sh", 1, 200).unwrap();
    files.edit("script.sh", "changed elsewhere", "edited", false).unwrap();
    assert_eq!(f.read("script.sh"), "edited\n");
    assert_eq!(mode(&path), 0o755);
}

#[cfg(unix)]
#[test]
fn edits_preserve_exact_permissions_under_restrictive_umasks_and_new_files_obey_umask() {
    // umask is process-wide: hold the env lock so no other test creates files meanwhile.
    let _guard = env(&[]);
    let f = fixture();
    let files = ws(&f);
    for mask in [0o027u32, 0o007, 0o077] {
        let previous = unsafe { libc::umask(mask as libc::mode_t) };
        for m in [0o755, 0o640, 0o444] {
            let path = f.cwd.join("existing");
            if path.exists() {
                chmod(&path, 0o600);
            }
            std::fs::write(&path, "before").unwrap();
            chmod(&path, m);
            files.read("existing", 1, 200).unwrap();
            files.edit("existing", "before", "after", false).unwrap();
            assert_eq!(mode(&path), m, "mode lost under umask {mask:o}");
            chmod(&path, 0o600);
        }
        let name = format!("new-{mask:o}");
        files.write(&name, "new").unwrap();
        let created = mode(&f.cwd.join(&name));
        unsafe { libc::umask(previous) };
        assert_eq!(created, 0o644 & !mask, "new file ignores umask {mask:o}");
    }
}

#[cfg(unix)]
#[test]
fn workspace_containment_rejects_dotdot_and_symlink_escapes_including_missing_leaves() {
    let f = fixture();
    let files = ws(&f);
    std::fs::write(f.root.join("outside.txt"), "private").unwrap();
    std::os::unix::fs::symlink(&f.root, f.cwd.join("escape")).unwrap();
    assert_match(&err(files.read("../outside.txt", 1, 200)), "escapes");
    assert_match(&err(files.read("escape/outside.txt", 1, 200)), "escapes");
    assert_match(&err(files.write("escape/new/created.txt", "oops")), "escapes");
}

#[test]
fn exact_edit_rejects_ambiguous_replacements_and_supports_replace_all() {
    let f = fixture();
    let files = ws(&f);
    files.write("a.txt", "one one").unwrap();
    assert_match(&err(files.edit("a.txt", "one", "two", false)), "found 2");
    files.edit("a.txt", "one", "two", true).unwrap();
    assert_eq!(f.read("a.txt"), "two two");
}

#[cfg(unix)]
#[test]
fn portable_search_is_bounded_and_skips_symlinks() {
    let f = fixture();
    let files = ws(&f);
    files.write("a.txt", "hello needle\nworld").unwrap();
    std::os::unix::fs::symlink(f.root.join("missing"), f.cwd.join("link")).unwrap();
    assert_match(&files.fallback_search("needle", "", &token()).unwrap(), "a.txt:1:hello needle");
}

fn opts<'a>(cwd: &'a Path, cancel: &'a CancellationToken, timeout_ms: u64) -> RunOptions<'a> {
    RunOptions { cwd, cancel, timeout_ms, env: None, output_limit_bytes: None, hard_output_bytes: None }
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_output_fits_or_truncates_without_duplicate_middle_bytes() {
    let f = fixture();
    let cancel = token();
    for size in [20usize, 40_000, 65_000, 100_000] {
        let args = vec!["-c".to_string(), format!("head -c {size} /dev/zero | tr '\\0' a")];
        let result = run_command("sh", &args, opts(&f.cwd, &cancel, 3000)).await.unwrap();
        assert_eq!(result.code, 0);
        if size <= 65_536 {
            assert_eq!(result.output.len(), size);
            assert!(result.output.bytes().all(|b| b == b'a'));
        } else {
            assert!(result.truncated);
            assert!(result.output.len() < 66_000);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_a_child_process_group_and_timeout_ends_runaway_commands() {
    let f = fixture();
    let cancel = token();
    let started = std::time::Instant::now();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        c.cancel();
    });
    let args = vec!["-c".to_string(), "sleep 30 & wait".to_string()];
    let result = run_command("bash", &args, opts(&f.cwd, &cancel, 5000)).await;
    assert!(matches!(result, Err(CommandError::Cancelled)));
    assert!(started.elapsed().as_millis() < 2500);
    let fresh = token();
    let result = run_command("sh", &["-c".to_string(), "while :; do sleep 1; done".to_string()], opts(&f.cwd, &fresh, 100)).await;
    assert_match(&result.err().unwrap().to_string(), "timed out");
}

#[test]
fn edit_normalizes_line_endings_supports_line_trimmed_fallback_and_previews_diffs() {
    let f = fixture();
    let files = ws(&f);
    f.write("win.ts", "const a = 1;\r\nconst b = 2;\r\nconst c = 3;\r\n");
    files.read("win.ts", 1, 200).unwrap();
    let preview = files.preview_diff("edit_file", &json!({ "path": "win.ts", "oldText": "const b = 2;\n", "newText": "const b = 42;\n" })).unwrap();
    assert_match(&preview, r"--- a/win\.ts");
    assert_match(&preview, "-const b = 2;");
    assert_match(&preview, r"\+const b = 42;");
    files.edit("win.ts", "const b = 2;\n", "const b = 42;\n", false).unwrap();
    assert_eq!(f.read("win.ts"), "const a = 1;\r\nconst b = 42;\r\nconst c = 3;\r\n");
    files.read("win.ts", 1, 200).unwrap();
    files.edit("win.ts", "  const a = 1;\n  const b = 42;", "const a = 10;\nconst b = 20;", false).unwrap();
    assert_eq!(f.read("win.ts"), "const a = 10;\r\nconst b = 20;\r\nconst c = 3;\r\n");
}

#[test]
fn apply_patch_supports_add_update_delete_and_restore_backups_reverts_them() {
    let f = fixture();
    let files = ws(&f);
    f.write("keep.txt", "line1\nline2\nline3\n");
    f.write("remove-me.txt", "goodbye\n");
    files.begin_turn();
    let patch = ["*** Begin Patch", "*** Update File: keep.txt", "@@", " line1", "-line2", "+line2_patched", " line3", "*** Add File: added.txt", "+hello world", "*** Delete File: remove-me.txt", "*** End Patch"].join("\n");
    let preview = files.preview_diff("apply_patch", &json!({ "patch": patch })).unwrap();
    assert_match(&preview, r"\+line2_patched");
    assert_match(&preview, r"\+hello world");
    files.read("keep.txt", 1, 200).unwrap();
    files.read("remove-me.txt", 1, 200).unwrap();
    files.apply_patch(&patch).unwrap();
    assert_eq!(f.read("keep.txt"), "line1\nline2_patched\nline3\n");
    assert_eq!(f.read("added.txt"), "hello world\n");
    assert!(!f.exists("remove-me.txt"));

    let backups = files.take_turn_backups();
    assert_eq!(backups.len(), 3);
    let restored = files.restore_backups(&backups, false).unwrap();
    let mut names = restored.restored.clone();
    names.sort();
    assert_eq!(names, vec!["keep.txt", "remove-me.txt"]);
    assert_eq!(restored.deleted, vec!["added.txt"]);
    assert_eq!(f.read("keep.txt"), "line1\nline2\nline3\n");
    assert_eq!(f.read("remove-me.txt"), "goodbye\n");
    assert!(!f.exists("added.txt"));

    // Subagent backups merge cleanly and preserve the earliest pre-turn state.
    files.begin_turn();
    files.read("keep.txt", 1, 200).unwrap();
    files.edit("keep.txt", "line2", "parent_edit", false).unwrap();
    files.merge_backups(vec![
        FileBackup { path: "keep.txt".into(), existed: true, content: Some("child_saw_parent_edit\n".into()), mode: Some(0o644), ..Default::default() },
        FileBackup { path: "child-added.txt".into(), existed: false, ..Default::default() },
    ]);
    f.write("child-added.txt", "from child\n");
    let merged = files.take_turn_backups();
    assert_eq!(merged.len(), 2);
    let restored = files.restore_backups(&merged, false).unwrap();
    assert_eq!(restored.restored, vec!["keep.txt"]);
    assert_eq!(restored.deleted, vec!["child-added.txt"]);
    assert_eq!(f.read("keep.txt"), "line1\nline2\nline3\n");
}

#[test]
fn apply_patch_rejects_unread_or_stale_files_before_planning_any_write() {
    let f = fixture();
    let files = ws(&f);
    f.write("a.txt", "one\n");
    let update = ["*** Begin Patch", "*** Update File: a.txt", "@@", "-one", "+two", "*** End Patch"].join("\n");
    assert_match(&files.preview_diff("apply_patch", &json!({ "patch": update })).unwrap(), r"\+two");
    let stale = r"Read the current file before patching; it is unread or has changed\.";
    assert_match(&err(files.apply_patch(&update)), stale);
    files.read("a.txt", 1, 200).unwrap();
    f.write("a.txt", "changed outside\n");
    assert_match(&err(files.apply_patch(&update)), stale);
    assert_eq!(f.read("a.txt"), "changed outside\n");
    assert_match(&err(files.apply_patch("*** Begin Patch\n*** Delete File: a.txt\n*** End Patch")), "Read the current file before patching");
}

#[test]
fn apply_patch_rejects_ambiguous_hunks_and_applies_insertion_only_hunks_at_the_cursor() {
    let f = fixture();
    let files = ws(&f);
    f.write("dup.txt", "x\nx\nx\n");
    files.read("dup.txt", 1, 200).unwrap();
    let ambiguous = ["*** Begin Patch", "*** Update File: dup.txt", "@@", "-x", "+y", "*** End Patch"].join("\n");
    assert_match(&err(files.apply_patch(&ambiguous)), r"Patch hunk is ambiguous in dup\.txt \(matched 3 times\); add more surrounding context\.");
    assert_eq!(f.read("dup.txt"), "x\nx\nx\n");
    f.write("insert.txt", "a\nb\n");
    files.read("insert.txt", 1, 200).unwrap();
    files.apply_patch(&["*** Begin Patch", "*** Update File: insert.txt", "@@", "+before", "*** End Patch"].join("\n")).unwrap();
    assert_eq!(f.read("insert.txt"), "before\na\nb\n");
}

#[test]
fn apply_patch_validates_resulting_syntax_before_writing_anything() {
    let f = fixture();
    let files = ws(&f);
    assert_match(&err(files.apply_patch(&["*** Begin Patch", "*** Add File: bad.ts", "+export function f() {", "*** End Patch"].join("\n"))), r"Syntax validation failed for bad\.ts: Unclosed");
    assert!(!f.exists("bad.ts"));
    f.write("ok.txt", "ok\n");
    files.read("ok.txt", 1, 200).unwrap();
    let patch = ["*** Begin Patch", "*** Update File: ok.txt", "@@", "-ok", "+ok2", "*** Add File: bad.ts", "+export function g() {", "*** End Patch"].join("\n");
    assert_match(&err(files.apply_patch(&patch)), "Syntax validation failed");
    assert_eq!(f.read("ok.txt"), "ok\n");
    assert!(!files.has_turn_changes());
}

#[test]
fn apply_patch_accepts_valid_syntax_and_ignores_braces_inside_strings_regex_and_templates() {
    let f = fixture();
    let files = ws(&f);
    let ts = ["const re = /[{}()]/g;", "const msg = \"}\";", "const tpl = `{ ${\"}\"} }`;", "export const obj = { ok: true };"];
    let mut lines = vec!["*** Begin Patch".to_string(), "*** Add File: good.ts".to_string()];
    lines.extend(ts.iter().map(|l| format!("+{l}")));
    lines.push("*** End Patch".into());
    files.apply_patch(&lines.join("\n")).unwrap();
    assert_eq!(f.read("good.ts"), format!("{}\n", ts.join("\n")));
}

#[test]
fn apply_patch_validates_plain_json_but_accepts_jsonc_config_paths() {
    let f = fixture();
    let files = ws(&f);
    let jsonc = ["{", "  // comment", "  \"compilerOptions\": { \"strict\": true, },", "}"];
    let mut lines = vec!["*** Begin Patch".to_string(), "*** Add File: tsconfig.json".to_string()];
    lines.extend(jsonc.iter().map(|l| format!("+{l}")));
    lines.push("*** End Patch".into());
    files.apply_patch(&lines.join("\n")).unwrap();
    assert_match(&err(files.apply_patch(&["*** Begin Patch", "*** Add File: package.json", "+{ \"a\": 1, }", "*** End Patch"].join("\n"))), r"Syntax validation failed for package\.json: invalid JSON");
}

#[test]
fn deletions_are_tombstoned_rollback_restores_originals_and_keeps_user_recreations() {
    let f = fixture();
    let files = ws(&f);
    f.write("gone.txt", "original\n");
    files.begin_turn();
    files.read("gone.txt", 1, 200).unwrap();
    files.apply_patch("*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch").unwrap();
    let backups = files.peek_turn_backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].after_deleted, Some(true));
    assert_eq!(backups[0].after_hash, None);
    assert_eq!(backups[0].content.as_deref(), Some("original\n"));

    f.write("gone.txt", "user recreated\n");
    let conflicted = files.restore_backups(&backups, false).unwrap();
    assert_eq!(conflicted.conflicts, vec!["gone.txt"]);
    assert_eq!(f.read("gone.txt"), "user recreated\n");

    std::fs::remove_file(f.cwd.join("gone.txt")).unwrap();
    let restored = files.restore_backups(&backups, false).unwrap();
    assert_eq!(restored.restored, vec!["gone.txt"]);
    assert_eq!(f.read("gone.txt"), "original\n");

    f.write("gone.txt", "user recreated\n");
    let forced = files.restore_backups(&backups, true).unwrap();
    assert_eq!(forced.restored, vec!["gone.txt"]);
    assert_eq!(f.read("gone.txt"), "original\n");
}

#[test]
fn turn_backup_apis_do_not_consume_state() {
    let f = fixture();
    let files = ws(&f);
    f.write("a.txt", "one\n");
    files.begin_turn();
    files.read("a.txt", 1, 200).unwrap();
    files.write("a.txt", "two\n").unwrap();
    assert!(files.has_turn_changes());
    assert_eq!(files.turn_changed_files(), vec!["a.txt"]);
    let peeked = files.peek_turn_backups();
    assert_eq!(peeked.len(), 1);
    assert_eq!(files.peek_turn_backups().len(), 1);
    files.begin_turn();
    assert!(!files.has_turn_changes());
    files.replace_turn_backups(peeked);
    assert!(files.has_turn_changes());
    assert_eq!(files.take_turn_backups().len(), 1);
    assert!(!files.has_turn_changes());
}

#[test]
fn merge_backups_propagates_after_deleted_and_fills_missing_pre_turn_fields() {
    let f = fixture();
    let files = ws(&f);
    f.write("m.txt", "base\n");
    files.begin_turn();
    files.read("m.txt", 1, 200).unwrap();
    files.write("m.txt", "parent\n").unwrap();
    files.merge_backups(vec![FileBackup { path: "m.txt".into(), existed: true, content: Some("base\n".into()), mode: Some(0o644), after_deleted: Some(true), ..Default::default() }]);
    let merged = files.peek_turn_backups().remove(0);
    assert_eq!(merged.after_deleted, Some(true));
    assert_eq!(merged.after_hash, None);
    assert_eq!(merged.content.as_deref(), Some("base\n"));

    files.merge_backups(vec![FileBackup { path: "m.txt".into(), existed: true, content: Some("base\n".into()), mode: Some(0o644), after_hash: Some("deadbeef".into()), ..Default::default() }]);
    let merged = files.peek_turn_backups().remove(0);
    assert_eq!(merged.after_deleted, None);
    assert_eq!(merged.after_hash.as_deref(), Some("deadbeef"));

    files.begin_turn();
    files.merge_backups(vec![FileBackup { path: "x.txt".into(), existed: true, ..Default::default() }]);
    files.merge_backups(vec![FileBackup { path: "x.txt".into(), existed: true, content: Some("content".into()), mode: Some(0o755), ..Default::default() }]);
    let filled = files.peek_turn_backups().remove(0);
    assert_eq!(filled.content.as_deref(), Some("content"));
    assert_eq!(filled.mode, Some(0o755));
}
