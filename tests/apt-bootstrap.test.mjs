import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFile } from 'node:fs/promises';

test('musl bootstrap authentication and timeout contracts run offline', async () => {
  const { stderr } = await promisify(execFile)(process.platform === 'win32' ? 'python' : 'python3', ['-B', 'tests/fixtures/install-musl-tests.py'], { env: { ...process.env, PYTHONDONTWRITEBYTECODE: '1' } });
  assert.match(stderr, /OK/);
});

test('musl CI restores and saves signed package metadata separately per architecture', async () => {
  const workflow = await readFile('.github/workflows/release.yml', 'utf8');
  assert.match(workflow, /python3 scripts\/install-musl\.py/);
  assert.match(workflow, /actions\/cache\/restore@/);
  assert.match(workflow, /actions\/cache\/save@/);
  assert.match(workflow, /apt-musl-v1-noble-\$\{\{ runner\.arch \}\}/);
  assert.doesNotMatch(workflow, /sudo apt-get update -q/);
});
