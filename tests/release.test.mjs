import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { selectVersion, setManifestVersion, parseVersion, setCargoVersion, cargoVersions } from '../scripts/release-version.mjs';
import { TARGETS, targetTriple, assetName } from '../scripts/install.mjs';

test('first release, automatic patches and numeric ordering use distinct immutable tags', () => {
  assert.equal(selectVersion('0.1.0', [], 'A'), '0.1.0');
  assert.equal(selectVersion('0.1.0', [{ name: 'v0.1.0', sha: 'A' }], 'B'), '0.1.1');
  assert.equal(selectVersion('0.1.9', [{ name: 'v0.1.9', sha: 'A' }, { name: 'v0.1.10', sha: 'B' }], 'C'), '0.1.11');
  assert.equal(selectVersion('0.1.0', [{ name: 'v0.4.9', sha: 'A' }], 'B'), '0.4.10');
});

test('reruns reuse the same tagged commit and explicit higher versions are respected', () => {
  const tags = [{ name: 'v0.1.0', sha: 'A' }, { name: 'v0.1.1', sha: 'B' }, { name: 'unrelated', sha: 'C' }];
  assert.equal(selectVersion('0.1.1', tags, 'B'), '0.1.1');
  assert.equal(selectVersion('0.2.0', tags, 'C'), '0.2.0');
  assert.equal(selectVersion('0.1.1', tags, 'C'), '0.1.2');
  assert.throws(() => parseVersion('1.0.0-beta'), /stable/);
  assert.throws(() => parseVersion('01.0.0'), /stable/);
});

test('version synchronization preserves dependencies and rejects a divergent lockfile', () => {
  const manifest = { name: 'goon-cli', version: '0.1.0', dependencies: { ai: '6.0.193' } };
  const lock = { version: '0.1.0', packages: { '': { version: '0.1.0', name: 'goon-cli' }, 'node_modules/ai': { version: '6.0.193' } } };
  const next = setManifestVersion(manifest, lock, '0.1.1');
  assert.equal(next.manifest.version, '0.1.1');
  assert.equal(next.lock.version, '0.1.1');
  assert.equal(next.lock.packages[''].version, '0.1.1');
  assert.deepEqual(next.manifest.dependencies, manifest.dependencies);
  assert.deepEqual(next.lock.packages['node_modules/ai'], lock.packages['node_modules/ai']);
  assert.equal(manifest.version, '0.1.0');
  assert.throws(() => setManifestVersion(manifest, { ...lock, version: '0.2.0' }, '0.1.1'), /disagree/);
});

test('Cargo version bumps touch only the package version and reject divergence', () => {
  const toml = '[package]\r\nname = "goon-cli"\r\nversion = "0.1.0"\r\n\r\n[dependencies]\r\nfoo = { version = "0.1.0" }\r\n';
  const lock = 'version = 4\n\n[[package]]\nname = "foo"\nversion = "0.1.0"\n\n[[package]]\nname = "goon-cli"\nversion = "0.1.0"\n';
  const next = setCargoVersion(toml, lock, '0.1.0', '0.2.0');
  assert.equal(next.cargoToml, toml.replace('version = "0.1.0"\r\n\r\n', 'version = "0.2.0"\r\n\r\n'));
  assert.deepEqual(cargoVersions(next.cargoToml, next.cargoLock), { manifest: '0.2.0', lock: '0.2.0' });
  assert.match(next.cargoLock, /name = "foo"\nversion = "0\.1\.0"/);
  assert.throws(() => setCargoVersion(toml, lock, '0.0.9', '0.2.0'), /disagree/);
  assert.throws(() => setCargoVersion('[package]\nname = "x"\n', lock, '0.1.0', '0.2.0'), /Cannot find/);
});

test('every supported platform maps to a shipped target', () => {
  assert.equal(targetTriple('android', 'arm64'), 'aarch64-linux-android');
  assert.equal(targetTriple('linux', 'x64'), 'x86_64-unknown-linux-musl');
  assert.equal(targetTriple('win32', 'arm64'), 'x86_64-pc-windows-msvc');
  assert.equal(targetTriple('sunos', 'x64'), undefined);
  assert.equal(TARGETS.length, 8);
  assert.equal(assetName('aarch64-apple-darwin'), 'goon-aarch64-apple-darwin.gz');
});

test('CLI version comes from the installed package manifest', async () => {
  const manifest = JSON.parse(await readFile('package.json', 'utf8'));
  const lock = JSON.parse(await readFile('npm-shrinkwrap.json', 'utf8'));
  assert.equal(lock.version, manifest.version, 'npm-shrinkwrap.json version must match package.json');
  assert.equal(lock.packages?.['']?.version, manifest.version, 'npm-shrinkwrap.json root package version must match package.json');
  assert.deepEqual(cargoVersions(await readFile('Cargo.toml', 'utf8'), await readFile('Cargo.lock', 'utf8')), { manifest: manifest.version, lock: manifest.version }, 'Cargo.toml and Cargo.lock must match package.json');
  const { stdout } = await promisify(execFile)(process.execPath, ['bin/goon.mjs', '--version']);
  assert.equal(stdout.trim(), `goon cli ${manifest.version}`);
});
