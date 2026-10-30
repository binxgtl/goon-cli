use crate::common::*;
use goon::rag::*;

const MAX_FILES: usize = 2000;
const MAX_BYTES: u64 = 1_048_576;

#[test]
fn fnv1a_and_embeddings_are_deterministic_384_dimensional_and_normalized() {
    assert_eq!(fnv1a("hello"), fnv1a("hello"));
    assert_ne!(fnv1a("hello"), fnv1a("hellp"));
    let va = HashEmbedder::embed_one("function alpha() { return 1; }");
    let vb = HashEmbedder::embed_one("function alpha() { return 1; }");
    assert_eq!(va.len(), EMBEDDING_DIM);
    assert_eq!(va, vb);
    let norm: f64 = va.iter().map(|v| v * v).sum::<f64>().sqrt();
    assert!((norm - 1.0).abs() < 1e-9);
    let v1 = HashEmbedder::embed_one("authentication login password session");
    let v2 = HashEmbedder::embed_one("authentication login password session");
    let v3 = HashEmbedder::embed_one("rendering viewport css layout");
    assert!((cosine_similarity(&v1, &v2) - 1.0).abs() < 1e-9);
    assert!(cosine_similarity(&v1, &v3) < 0.5);
}

#[test]
fn tokenize_lowercases_and_drops_short_tokens() {
    assert_eq!(tokenize("Hello_world a I code9 x _"), vec!["hello_world", "code9"]);
}

#[test]
fn chunk_file_splits_ts_js_go_python_at_declarations_with_one_based_ranges() {
    let content = ["export function alpha() {", "  return 1;", "}", "", "export function beta() {", "  return 2;", "}"].join("\n");
    let chunks = chunk_file("src/a.ts", &content, None);
    assert_eq!(chunks.len(), 2);
    assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 3));
    assert!(chunks[0].content.contains("alpha"));
    assert_eq!((chunks[1].start_line, chunks[1].end_line), (5, 7));
    let py = chunk_file("m.py", "class A:\n    def go(self):\n        pass\n\ndef top():\n    return 3\n", None);
    assert!(py.len() >= 2);
    assert!(py[0].content.contains("class A"));
    assert_eq!(chunk_file("m.go", "func Main() {\n}\n\nfunc (s *S) Aux() {\n}\n", None).len(), 2);
}

#[test]
fn chunk_file_falls_back_to_forty_line_windows_with_eight_line_overlap() {
    let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
    let chunks = chunk_file("notes.txt", &lines.join("\n"), None);
    assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 40));
    assert_eq!(chunks[1].start_line, 33);
    assert_eq!(chunks.last().unwrap().end_line, 100);
}

#[test]
fn chunks_over_the_soft_limit_are_resplit_and_the_hard_cap_bounds_every_piece() {
    let body: Vec<String> = (0..200).map(|i| format!("x = {i}; // {}", "y".repeat(30))).collect();
    let content = format!("export function big() {{\n{}\n}}\n", body.join("\n"));
    let chunks = chunk_file("src/big.ts", &content, None);
    assert!(chunks.len() > 1);
    let total = content.split('\n').count() as u64;
    for chunk in &chunks {
        assert!(chunk.content.chars().count() <= 4000);
        assert!(chunk.start_line >= 1 && chunk.end_line <= total);
    }
}

#[test]
fn a_single_line_over_the_soft_limit_still_produces_exactly_one_chunk() {
    let line = format!("const x = \"{}\";", "A".repeat(1600));
    let chunks = chunk_file("long.ts", &format!("{line}\n"), None);
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 1));
    assert_eq!(chunks[0].content, line);
}

#[test]
fn a_single_line_over_the_hard_cap_is_emitted_once() {
    let line = "X".repeat(6000);
    let chunks = chunk_file("huge.ts", &format!("function f() {{\n{line}\n}}\n"), None);
    assert_eq!(chunks.iter().map(|c| (c.start_line, c.end_line)).collect::<Vec<_>>(), vec![(1, 1), (2, 2), (3, 3)]);
    assert_eq!(chunks.iter().filter(|c| c.content == line).count(), 1);
    let keys: HashSet<String> = chunks.iter().map(|c| format!("{}:{}:{}", c.start_line, c.end_line, c.content)).collect();
    assert_eq!(keys.len(), chunks.len());
}

#[test]
fn a_declaration_chunk_spanning_more_than_two_windows_covers_every_line() {
    let body: Vec<String> = (0..120).map(|i| format!("  const v{i} = {i} + \"pad-pad-pad\";")).collect();
    let content = format!("function big() {{\n{}\n}}\n", body.join("\n"));
    let chunks = chunk_file("big.ts", &content, None);
    assert!(chunks.len() >= 3);
    let covered: HashSet<u64> = chunks.iter().flat_map(|c| c.start_line..=c.end_line).collect();
    let total = content.split('\n').count() as u64 - 1;
    let missing: Vec<u64> = (1..=total).filter(|l| !covered.contains(l)).collect();
    assert!(missing.is_empty(), "missing {missing:?}");
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks.last().unwrap().end_line, total);
}

#[test]
fn goonignore_subset_supports_comments_negation_dir_only_globs_and_anchors() {
    let rules = parse_goonignore(&["# comment line", "", "node_modules", "build/", "*.log", "!important.log", "data/**/*.tmp", "?temp.txt", "/anchored.txt"].join("\n"));
    let cases = [
        ("node_modules/pkg/index.js", true),
        ("src/build/out.js", true),
        ("debug.log", true),
        ("sub/debug.log", true),
        ("important.log", false),
        ("data/x/y.tmp", true),
        ("data/x.tmp", true),
        ("xtemp.txt", true),
        ("temp.txt", false),
        ("anchored.txt", true),
        ("sub/anchored.txt", false),
        ("src/index.ts", false),
    ];
    for (path, ignored) in cases {
        assert_eq!(is_ignored(path, &rules), ignored, "{path}");
    }
}

#[test]
fn index_repo_skips_sensitive_ignored_binary_and_oversized_files_and_redacts_chunks() {
    let f = fixture();
    f.write("src/auth.ts", "export function login(user: string) {\n  const apiKey = \"sk-live-abcdefgh\";\n  return user;\n}\n");
    f.write(".env", "GOON_API_KEY=supersecret\n");
    f.write(".env.local", "X=1\n");
    f.write("server.pem", "-----BEGIN\n");
    f.write("secrets/key.txt", "hidden\n");
    f.write("node_modules/pkg/index.js", "module.exports = 1;\n");
    f.write("skipme/a.txt", "skipped by goonignore\n");
    std::fs::write(f.cwd.join("binary.bin"), [0u8, 1, 2, 3]).unwrap();
    f.write("huge.txt", &"z".repeat(2048));
    f.write(".goonignore", "skipme/\n.goonignore\n");
    let result = index_repo(&f.cwd, MAX_FILES, 1024, &["sk-live-abcdefgh".into()]);
    assert_eq!(result.snapshot.files.keys().cloned().collect::<Vec<_>>(), vec!["src/auth.ts"]);
    let chunk = &result.snapshot.files["src/auth.ts"].chunks[0];
    assert!(!chunk.content.contains("sk-live-abcdefgh"));
    assert!(chunk.content.contains("[REDACTED]"));
    assert!(result.files == 1 && result.chunks >= 1);
    assert!(!result.truncated);
}

#[cfg(unix)]
#[test]
fn walk_repo_skips_symlinks_and_links_pointing_outside_the_root() {
    let f = fixture();
    let outside = f.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    f.write("src/real.ts", "export const real = 1;\n");
    std::fs::write(outside.join("leak.txt"), "must not be indexed\n").unwrap();
    std::os::unix::fs::symlink(outside.join("leak.txt"), f.cwd.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, f.cwd.join("linkdir")).unwrap();
    let walk = walk_repo(&f.cwd, None, MAX_BYTES, MAX_FILES);
    assert_eq!(walk.files.iter().map(|w| w.path.clone()).collect::<Vec<_>>(), vec!["src/real.ts"]);
}

#[tokio::test]
async fn snapshot_round_trip_recomputes_vectors_on_load() {
    let f = fixture();
    f.write("src/login.ts", "export function login(user: string) {\n  return user;\n}\n");
    let file = rag_snapshot_path(&f.data, "proj1");
    let result = index_repo(&f.cwd, MAX_FILES, MAX_BYTES, &[]);
    save_rag_snapshot(&file, &result.snapshot).unwrap();
    let loaded = load_rag_snapshot(&file).expect("snapshot");
    let mut index = LexicalIndex::new(None, Some(100), None);
    let docs: Vec<AddDocument> = loaded.files.values().flat_map(|r| r.chunks.iter().cloned()).map(|chunk| AddDocument { id: None, chunk }).collect();
    index.add(docs).await.unwrap();
    assert!(!index.is_empty());
    let hits = index.retrieve("login", 3).await.unwrap();
    assert!(!hits.is_empty() && hits[0].score > 0.0);
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(raw["version"], 1);
    for record in raw["files"].as_object().unwrap().values() {
        assert!(record["mtimeMs"].is_number() && record["size"].is_number() && record["chunks"].is_array());
        for chunk in record["chunks"].as_array().unwrap() {
            assert!(chunk.get("vector").is_none());
        }
    }
}

fn doc(path: &str, content: &str) -> AddDocument {
    AddDocument { id: None, chunk: Chunk { path: path.into(), start_line: 1, end_line: 2, content: content.into() } }
}

#[tokio::test]
async fn lexical_index_ranks_matching_docs_first_with_deterministic_ties() {
    let mut index = LexicalIndex::new(None, Some(10), None);
    index
        .add(vec![
            doc("src/render.ts", "rendering viewport css layout engine"),
            doc("src/auth.ts", "authentication login password session tokens"),
            doc("src/auth2.ts", "authentication login password session tokens"),
        ])
        .await
        .unwrap();
    let hits = index.retrieve("authentication password", 2).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].chunk.path, "src/auth.ts");
    assert_eq!(hits[1].chunk.path, "src/auth2.ts");
    assert!(hits[0].score > 0.4);
}

#[tokio::test]
async fn lexical_index_evicts_least_recently_used_over_capacity() {
    let mut index = LexicalIndex::new(None, Some(3), Some(0.4));
    index.add(vec![doc("a.txt", "alphaunique token one"), doc("b.txt", "betaunique token two"), doc("c.txt", "gammaunique token three")]).await.unwrap();
    assert_eq!(index.len(), 3);
    index.add(vec![doc("d.txt", "deltaunique token four")]).await.unwrap();
    assert_eq!(index.len(), 3);
    assert!(index.retrieve("alphaunique", 5).await.unwrap().is_empty());
    assert_eq!(index.retrieve("deltaunique", 5).await.unwrap().len(), 1);
    index.remove_by_path("d.txt");
    assert_eq!(index.len(), 2);
    index.clear();
    assert_eq!(index.len(), 0);
}

#[tokio::test]
async fn threshold_filters_low_score_hits() {
    let mut index = LexicalIndex::new(None, None, Some(0.4));
    index.add(vec![doc("a.txt", "authentication login password session"), doc("b.txt", "rendering viewport css layout")]).await.unwrap();
    assert_eq!(index.retrieve("authentication login", 5).await.unwrap().len(), 1);
    assert!(index.retrieve("completely unrelated quantum banana", 5).await.unwrap().is_empty());
}

#[test]
fn looks_binary_detects_nul_bytes() {
    assert!(!looks_binary(b"plain text file"));
    assert!(looks_binary(&[0x70, 0x00, 0x71]));
}

#[test]
fn project_id_is_a_stable_sixteen_hex_workspace_id() {
    let id = project_id_for(Path::new("C:/some/workspace"));
    assert_match(&id, "^[0-9a-f]{16}$");
    assert_eq!(id, project_id_for(Path::new("C:/some/workspace")));
    assert_ne!(id, project_id_for(Path::new("C:/other/workspace")));
}

#[test]
fn walk_repo_caps_files_and_reports_truncation() {
    let f = fixture();
    for i in 0..5 {
        f.write(&format!("d/f{i}.txt"), "x\n");
    }
    let walk = walk_repo(&f.cwd, None, MAX_BYTES, 3);
    assert_eq!(walk.files.len(), 3);
    assert!(walk.truncated);
}
