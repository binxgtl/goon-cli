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
