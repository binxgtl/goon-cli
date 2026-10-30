use goon::syntax::{is_jsonc_path, validate_syntax};

fn err(path: &str, content: &str) -> String {
    validate_syntax(path, content).expect_err("expected a syntax error").to_string()
}

#[test]
fn accepts_balanced_typescript_and_javascript() {
    validate_syntax("a.ts", "const obj = { a: [1, 2], b: (x) => ({ ...x }) };\n").unwrap();
    validate_syntax("a.mjs", "export function f(a) { return a.map(x => ({ x })); }\n").unwrap();
    validate_syntax("a.tsx", "type Props = { title: string };\nexport const C = (p: Props) => <div>{p.title}</div>;\n").unwrap();
    validate_syntax("empty.ts", "").unwrap();
}

#[test]
fn rejects_unbalanced_and_mismatched_brackets_with_line_numbers() {
    assert!(err("bad.ts", "function f() {\n").contains("Unclosed '{' at line 1"));
    assert!(err("bad.js", "if (x) { }\n}\n").contains("Unexpected '}' at line 2"));
    assert!(err("bad.ts", "const a = [1, 2;\n").contains("Unclosed '[' at line 1"));
    assert!(err("bad.ts", "const a = { b: 1 );\n").contains("Mismatched ')' at line 1"));
}

#[test]
fn skips_strings_comments_template_literals_and_regular_expressions() {
    let content = [
        "const re = /[{}()\\[\\]]/g;",
        "const withBraces = /}/;",
        "const s = \"}\";",
        "const t = '}';",
        "const u = `{ ${\"}\"} }`;",
        "const nested = `a${ `b${ { x: 1 } }c` }d`;",
        "// } { ( [",
        "/* } { ( [ */",
        "const obj = { a: 1 };",
        "",
    ]
    .join("\n");
    validate_syntax("good.ts", &content).unwrap();
}

#[test]
fn still_sees_brackets_outside_skipped_regions() {
    assert!(err("bad.ts", "const t = `{`;\nfunction f() {\n").contains("Unclosed '{' at line 2"));
    assert!(err("bad.ts", "const s = \"}\";\n[\n").contains("Unclosed '['"));
}

#[test]
fn distinguishes_division_from_a_regex_literal() {
    validate_syntax("division.ts", "const half = total / 2;\nconst third = a / b / c;\n").unwrap();
    validate_syntax("regex.ts", "if (/}/.test(x)) { y(); }\nconst r = /[}]/u;\nreturn /{/.test(x);\n").unwrap();
}

#[test]
fn parses_json_strictly_and_strips_a_bom() {
    validate_syntax("package.json", "{\"a\": [1, 2]}\n").unwrap();
    validate_syntax("bom.json", "\u{feff}{\"a\": 1}").unwrap();
    assert_eq!(err("package.json", "{\"a\": 1,}"), "invalid JSON: Expected double-quoted property name in JSON at position 8 (line 1 column 9)");
    assert_eq!(err("package.json", ""), "invalid JSON: Unexpected end of JSON input");
}

#[test]
fn allows_jsonc_for_config_paths_only() {
    let jsonc = "{ // comment\n \"compilerOptions\": { \"strict\": true, },\n}";
    for path in ["tsconfig.json", "tsconfig.build.json", "jsconfig.json", "settings.jsonc", ".vscode/settings.json", ".vscode/tasks/launch.json", "devcontainer.json"] {
        assert!(is_jsonc_path(path), "{path}");
        validate_syntax(path, jsonc).unwrap();
    }
    assert!(!is_jsonc_path("package.json"));
    assert!(err("settings.json", jsonc).contains("invalid JSON"));
}

#[test]
fn ignores_unknown_extensions() {
    validate_syntax("README.md", "unbalanced {{ [").unwrap();
    validate_syntax("styles.css", "a { b {").unwrap();
    validate_syntax("noext", "{{{{").unwrap();
}
