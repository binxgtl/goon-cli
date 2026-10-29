import test from 'node:test';
import assert from 'node:assert/strict';
import { validateSyntax, isJsoncPath } from '../dist/syntax.js';

test('validateSyntax accepts balanced TypeScript/JavaScript', () => {
  validateSyntax('a.ts', 'const obj = { a: [1, 2], b: (x) => ({ ...x }) };\n');
  validateSyntax('a.mjs', 'export function f(a) { return a.map(x => ({ x })); }\n');
  validateSyntax('a.tsx', 'type Props = { title: string };\nexport const C = (p: Props) => <div>{p.title}</div>;\n');
  validateSyntax('empty.ts', '');
});

test('validateSyntax rejects unbalanced and mismatched brackets with line numbers', () => {
  assert.throws(() => validateSyntax('bad.ts', 'function f() {\n'), /Unclosed '\{' at line 1/);
  assert.throws(() => validateSyntax('bad.js', 'if (x) { }\n}\n'), /Unexpected '\}' at line 2/);
  assert.throws(() => validateSyntax('bad.ts', 'const a = [1, 2;\n'), /Unclosed '\[' at line 1/);
  assert.throws(() => validateSyntax('bad.ts', 'const a = { b: 1 );\n'), /Mismatched '\)' at line 1/);
});

test('validateSyntax skips strings, comments, template literals and regular expressions', () => {
  const content = [
    'const re = /[{}()\\[\\]]/g;',
    'const withBraces = /}/;',
    'const s = "}";',
    "const t = '}';",
    'const u = `{ ${"}"} }`;',
    'const nested = `a${ `b${ { x: 1 } }c` }d`;',
    '// } { ( [',
    '/* } { ( [ */',
    'const obj = { a: 1 };',
    '',
  ].join('\n');
  validateSyntax('good.ts', content);
});

test('validateSyntax still sees brackets outside skipped regions', () => {
  assert.throws(
    () => validateSyntax('bad.ts', 'const t = `{`;\nfunction f() {\n'),
    /Unclosed '\{' at line 2/,
  );
  assert.throws(() => validateSyntax('bad.ts', 'const s = "}";\n[\n'), /Unclosed '\['/);
});

test('validateSyntax distinguishes division from a regex literal', () => {
  validateSyntax('division.ts', 'const half = total / 2;\nconst third = a / b / c;\n');
  validateSyntax('regex.ts', 'if (/}/.test(x)) { y(); }\nconst r = /[}]/u;\nreturn /{/.test(x);\n');
});

test('validateSyntax parses JSON strictly and strips a BOM', () => {
  validateSyntax('package.json', '{"a": [1, 2]}\n');
  validateSyntax('bom.json', '\uFEFF{"a": 1}');
  assert.throws(() => validateSyntax('package.json', '{"a": 1,}'), /invalid JSON/);
  assert.throws(() => validateSyntax('package.json', ''), /invalid JSON/);
});

test('validateSyntax allows JSONC for config paths only', () => {
  const jsonc = '{ // comment\n "compilerOptions": { "strict": true, },\n}';
  for (const path of [
    'tsconfig.json',
    'tsconfig.build.json',
    'jsconfig.json',
    'settings.jsonc',
    '.vscode/settings.json',
    '.vscode/tasks/launch.json',
    'devcontainer.json',
  ]) {
    assert.equal(isJsoncPath(path), true, path);
    validateSyntax(path, jsonc);
  }
  assert.equal(isJsoncPath('package.json'), false);
  assert.throws(() => validateSyntax('settings.json', jsonc), /invalid JSON/);
});

test('validateSyntax ignores unknown extensions', () => {
  validateSyntax('README.md', 'unbalanced {{ [' );
  validateSyntax('styles.css', 'a { b {');
  validateSyntax('noext', '{{{{');
});
