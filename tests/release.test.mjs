import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir, mkdtemp, writeFile, copyFile, rm, realpath } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { selectVersion, setManifestVersion, parseVersion, setCargoVersion, cargoVersions } from '../scripts/release-version.mjs';
import { verifyPublishReady } from '../scripts/pack-release.mjs';
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

test('the CI plan entry point runs on its own and picks the next version from the tags', { timeout: 30000 }, async t => {
  const exec = promisify(execFile);
  const root = await realpath(await mkdtemp(join(tmpdir(), 'goon-plan-test-')));
  t.after(() => rm(root, { recursive: true, force: true }));
  const work = join(root, 'work'), remote = join(root, 'remote.git');
  const git = (...args) => exec('git', args, { cwd: work });
  await exec('git', ['init', '--bare', '--initial-branch=main', remote]);
  await mkdir(join(work, 'scripts'), { recursive: true });
  await git('init', '-b', 'main');
  await git('config', 'user.name', 'Plan Test');
  await git('config', 'user.email', 'test@example.invalid');
  await git('remote', 'add', 'origin', remote);
  await writeFile(join(work, 'package.json'), JSON.stringify({ name: 'goon-cli', version: '0.1.0' }));
  for (const file of ['release-version.mjs', 'pack-release.mjs', 'install.mjs']) await copyFile(join('scripts', file), join(work, 'scripts', file));
  await git('add', '.');
  await git('commit', '-m', 'initial');
  await git('tag', 'v0.1.0');
  await writeFile(join(work, 'change.txt'), 'x');
  await git('add', '.');
  await git('commit', '-m', 'change');
  await git('push', 'origin', 'main', '--tags');
  // Run as CI does: a fresh process whose entry module is release-version.mjs (an import cycle
  // with pack-release.mjs once deadlocked here and exited 13).
  const { stdout } = await exec(process.execPath, ['scripts/release-version.mjs', 'plan'], { cwd: work });
  assert.deepEqual(JSON.parse(stdout), { skip: false, version: '0.1.1', reason: '' });
});

test('verifyPublishReady and prepublishOnly block unpacked bare npm publish while packed tarballs pass', { timeout: 30000 }, async t => {
  const exec = promisify(execFile);
  const root = await realpath(await mkdtemp(join(tmpdir(), 'goon-verify-publish-')));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, 'scripts'), { recursive: true });
  await mkdir(join(root, 'bin'), { recursive: true });
  for (const file of ['release-version.mjs', 'pack-release.mjs', 'install.mjs']) {
    await copyFile(join('scripts', file), join(root, 'scripts', file));
  }
  await copyFile(join('bin', 'goon.mjs'), join(root, 'bin', 'goon.mjs'));
  const pkg = {
    name: 'goon-cli',
    version: '0.1.0',
    type: 'module',
    bin: { goon: 'bin/goon.mjs' },
    files: ['bin', 'scripts/install.mjs', 'vendor/manifest.json', 'THIRD-PARTY-NOTICES.txt'],
    scripts: { prepublishOnly: 'node scripts/pack-release.mjs --verify-ready' },
  };
  await writeFile(join(root, 'package.json'), `${JSON.stringify(pkg, null, 2)}\n`);

  // 1. Missing vendor/manifest.json must fail verifyPublishReady and block bare `npm publish --dry-run`.
  await assert.rejects(() => verifyPublishReady(root), /Refusing bare "npm publish"/);
  const emptyNpmrc = join(root, 'empty.npmrc');
  await writeFile(emptyNpmrc, '');
  const npmEnv = { ...process.env, NPM_CONFIG_USERCONFIG: emptyNpmrc, NPM_TOKEN: '', NODE_AUTH_TOKEN: '' };
  await assert.rejects(
    () => exec('npm', ['publish', '--dry-run'], { cwd: root, env: npmEnv, shell: process.platform === 'win32' }),
    error => /Refusing bare "npm publish"/.test(`${error.stdout ?? ''}${error.stderr ?? ''}${error.message}`),
  );

  // 2. Version mismatch, invalid SHA-256, and missing THIRD-PARTY-NOTICES.txt must all be rejected.
  await mkdir(join(root, 'vendor'), { recursive: true });
  const validAssets = Object.fromEntries(TARGETS.map(triple => [triple, { file: assetName(triple), sha256: 'a'.repeat(64), size: 4096 }]));
  await writeFile(join(root, 'vendor', 'manifest.json'), JSON.stringify({ version: '0.9.9', assets: validAssets }));
  await assert.rejects(() => verifyPublishReady(root), /does not match package\.json/);

  await writeFile(join(root, 'vendor', 'manifest.json'), JSON.stringify({ version: '0.1.0', assets: { ...validAssets, [TARGETS[0]]: { file: assetName(TARGETS[0]), sha256: 'short' } } }));
  await assert.rejects(() => verifyPublishReady(root), /missing a valid SHA-256 entry/);

  await writeFile(join(root, 'vendor', 'manifest.json'), JSON.stringify({ version: '0.1.0', assets: validAssets }));
  await assert.rejects(() => verifyPublishReady(root), /THIRD-PARTY-NOTICES\.txt is missing/);

  // 3. Complete valid tree passes verifyPublishReady, and publishing a packed .tgz tarball works directly.
  await writeFile(join(root, 'THIRD-PARTY-NOTICES.txt'), 'Third-party notices\n');
  const verified = await verifyPublishReady(root);
  assert.equal(verified.version, '0.1.0');
  const outDir = join(root, 'packed');
  await mkdir(outDir, { recursive: true });
  await exec('npm', ['pack', '--ignore-scripts', '--pack-destination', outDir], { cwd: root, env: npmEnv, shell: process.platform === 'win32' });
  await rm(join(root, 'vendor'), { recursive: true, force: true });
  await rm(join(root, 'THIRD-PARTY-NOTICES.txt'), { force: true });
  await exec('npm', ['publish', join(outDir, 'goon-cli-0.1.0.tgz'), '--dry-run'], { cwd: root, env: npmEnv, shell: process.platform === 'win32' });
});
