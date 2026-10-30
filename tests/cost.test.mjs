import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

test('cost: CLI exits 2 when maxCostUsd is set but the model has no price', () => {
  const dir = mkdtempSync(join(tmpdir(), 'goon-cost-'));
  try {
    const cli = fileURLToPath(new URL('../bin/goon.mjs', import.meta.url));
    const env = {
      ...process.env,
      HOME: dir,
      USERPROFILE: dir,
      GOON_API_KEY: 'test-key',
      GOON_MODEL: 'unpriced-model',
      GOON_MAX_COST_USD: '1',
    };
    const result = spawnSync(process.execPath, [cli, 'hello'], { cwd: dir, env, encoding: 'utf8' });
    assert.equal(result.status, 2, result.stderr);
    assert.match(result.stderr + result.stdout, /No price configured/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
