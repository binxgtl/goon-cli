import test from 'node:test';
import assert from 'node:assert/strict';
import { writeFile, readFile, symlink, stat, chmod } from 'node:fs/promises';
import { join } from 'node:path';
import { WorkspaceFiles } from '../dist/files.js';
import { runCommand } from '../dist/process.js';
import { fixture } from './helpers.mjs';

test('existing files require a fresh read; edits preserve file mode', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  const path = join(f.cwd, 'script.sh');
  await writeFile(path, 'first\n', { mode: 0o755 });
  await chmod(path, 0o755);
  assert.equal((await stat(path)).mode & 0o777, 0o755);
  await assert.rejects(files.write('script.sh', 'second'), /Read the current file/);
  await files.read('script.sh');
  await writeFile(path, 'changed elsewhere\n');
  await assert.rejects(files.edit('script.sh', 'changed', 'edited'), /changed/);
  await files.read('script.sh');
  await files.edit('script.sh', 'changed elsewhere', 'edited');
  assert.equal(await readFile(path, 'utf8'), 'edited\n');
  assert.equal((await stat(path)).mode & 0o777, 0o755);
});

test('edits preserve exact permissions under restrictive umasks; new files obey umask', async t => {
  const f = await fixture(t);
  for (const mask of [0o027, 0o007, 0o077]) {
    const source = `
      import { writeFile, chmod, stat } from 'node:fs/promises';
      import { WorkspaceFiles } from ${JSON.stringify(new URL('../dist/files.js', import.meta.url).href)};
      process.umask(${mask});
      const files = new WorkspaceFiles(process.cwd());
      for (const mode of [0o755, 0o640, 0o444]) {
        await writeFile('existing', 'before');
        await chmod('existing', mode);
        await files.read('existing');
        await files.edit('existing', 'before', 'after');
        if (((await stat('existing')).mode & 0o777) !== mode) throw Error('mode lost');
        await chmod('existing', 0o600);
      }
      await files.write('new-${mask}', 'new');
      if (((await stat('new-${mask}')).mode & 0o777) !== (0o644 & ~${mask})) throw Error('new file ignores umask');
    `;
    const result = await runCommand(process.execPath, ['--input-type=module', '-e', source], {
      cwd: f.cwd, signal: new AbortController().signal, timeoutMs: 5000,
    });
    assert.equal(result.code, 0, result.output);
  }
});

test('workspace containment rejects ../ and symlink escapes including missing leaves', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await writeFile(join(f.root, 'outside.txt'), 'private');
  await symlink(f.root, join(f.cwd, 'escape'));
  await assert.rejects(files.read('../outside.txt'), /escapes/);
  await assert.rejects(files.read('escape/outside.txt'), /escapes/);
  await assert.rejects(files.write('escape/new/created.txt', 'oops'), /escapes/);
});

test('exact edit rejects ambiguous replacements and supports explicit replaceAll', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await files.write('a.txt', 'one one');
  await assert.rejects(files.edit('a.txt', 'one', 'two'), /found 2/);
  await files.edit('a.txt', 'one', 'two', true);
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'two two');
});

test('portable search is bounded and skips symlinks', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await files.write('a.txt', 'hello needle\nworld');
  await symlink(join(f.root, 'missing'), join(f.cwd, 'link'));
  assert.match(await files.fallbackSearch('needle'), /a.txt:1:hello needle/);
});

test('subprocess output fits or truncates without duplicate middle bytes', async t => {
  const f = await fixture(t);
  for (const size of [20, 40_000, 65_000, 100_000]) {
    const result = await runCommand(process.execPath, ['-e', `process.stdout.write('a'.repeat(${size}))`], {
      cwd: f.cwd, signal: new AbortController().signal, timeoutMs: 3000,
    });
    assert.equal(result.code, 0);
    if (size <= 65_536) assert.equal(result.output.length, size);
    else { assert(result.truncated); assert(result.output.length < 66_000); }
  }
});

test('cancellation kills a child process group and timeout ends runaway commands', async t => {
  const f = await fixture(t), controller = new AbortController();
  const started = Date.now();
  const timer = setTimeout(() => controller.abort(), 150);
  await assert.rejects(runCommand('bash', ['-c', 'sleep 30 & wait'], { cwd: f.cwd, signal: controller.signal, timeoutMs: 5000 }), /cancelled/);
  clearTimeout(timer);
  assert(Date.now() - started < 2500);
  await assert.rejects(runCommand(process.execPath, ['-e', 'setInterval(()=>{},1000)'], {
    cwd: f.cwd, signal: new AbortController().signal, timeoutMs: 100,
  }), /timed out/);
});

test('edit normalizes CRLF/LF line endings, supports line-trimmed fallback, and generates unified diff previews', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  const crlfFile = join(f.cwd, 'win.ts');
  await writeFile(crlfFile, 'const a = 1;\r\nconst b = 2;\r\nconst c = 3;\r\n', 'utf8');
  await files.read('win.ts');

  const diffPreview = await files.previewDiff('edit_file', {
    path: 'win.ts',
    oldText: 'const b = 2;\n',
    newText: 'const b = 42;\n',
  });
  assert.match(diffPreview, /--- a\/win\.ts/);
  assert.match(diffPreview, /-const b = 2;/);
  assert.match(diffPreview, /\+const b = 42;/);

  await files.edit('win.ts', 'const b = 2;\n', 'const b = 42;\n');
  const afterCrlf = await readFile(crlfFile, 'utf8');
  assert.equal(afterCrlf, 'const a = 1;\r\nconst b = 42;\r\nconst c = 3;\r\n');

  // Line-trimmed fallback when indentation drifts slightly
  await files.read('win.ts');
  await files.edit('win.ts', '  const a = 1;\n  const b = 42;', 'const a = 10;\nconst b = 20;');
  assert.equal(await readFile(crlfFile, 'utf8'), 'const a = 10;\r\nconst b = 20;\r\nconst c = 3;\r\n');
});

test('applyPatch supports Add/Update/Delete and restoreBackups reverts modified, added, and deleted files', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await writeFile(join(f.cwd, 'keep.txt'), 'line1\nline2\nline3\n', 'utf8');
  await writeFile(join(f.cwd, 'remove-me.txt'), 'goodbye\n', 'utf8');

  files.beginTurn();
  const patch = [
    '*** Begin Patch',
    '*** Update File: keep.txt',
    '@@',
    ' line1',
    '-line2',
    '+line2_patched',
    ' line3',
    '*** Add File: added.txt',
    '+hello world',
    '*** Delete File: remove-me.txt',
    '*** End Patch',
  ].join('\n');

  const preview = await files.previewDiff('apply_patch', { patch });
  assert.match(preview, /\+line2_patched/);
  assert.match(preview, /\+hello world/);

  await files.applyPatch(patch);
  assert.equal(await readFile(join(f.cwd, 'keep.txt'), 'utf8'), 'line1\nline2_patched\nline3\n');
  assert.equal(await readFile(join(f.cwd, 'added.txt'), 'utf8'), 'hello world');
  await assert.rejects(readFile(join(f.cwd, 'remove-me.txt'), 'utf8'));

  const backups = files.takeTurnBackups();
  assert.equal(backups.length, 3);

  const restored = await files.restoreBackups(backups);
  assert.deepEqual(restored.restored.sort(), ['keep.txt', 'remove-me.txt']);
  assert.deepEqual(restored.deleted, ['added.txt']);

  assert.equal(await readFile(join(f.cwd, 'keep.txt'), 'utf8'), 'line1\nline2\nline3\n');
  assert.equal(await readFile(join(f.cwd, 'remove-me.txt'), 'utf8'), 'goodbye\n');
  await assert.rejects(readFile(join(f.cwd, 'added.txt'), 'utf8'));

  // Subagent backups merge cleanly and preserve earliest pre-turn state
  files.beginTurn();
  await files.read('keep.txt');
  await files.edit('keep.txt', 'line2', 'parent_edit');
  files.mergeBackups([
    { path: 'keep.txt', existed: true, content: 'child_saw_parent_edit\n', mode: 0o644 },
    { path: 'child-added.txt', existed: false },
  ]);
  await writeFile(join(f.cwd, 'child-added.txt'), 'from child\n', 'utf8');
  const merged = files.takeTurnBackups();
  assert.equal(merged.length, 2);
  const restoredMerged = await files.restoreBackups(merged);
  assert.deepEqual(restoredMerged.restored, ['keep.txt']);
  assert.deepEqual(restoredMerged.deleted, ['child-added.txt']);
  assert.equal(await readFile(join(f.cwd, 'keep.txt'), 'utf8'), 'line1\nline2\nline3\n');
});

