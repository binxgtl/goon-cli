import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, mkdtemp, writeFile, readFile, copyFile, rm, realpath } from 'node:fs/promises';
import { join, delimiter } from 'node:path';
import { tmpdir } from 'node:os';
import { pathToFileURL } from 'node:url';
import { gzipSync } from 'node:zlib';
import { randomBytes } from 'node:crypto';
import { TARGETS, assetName } from '../scripts/install.mjs';

const exec = promisify(execFile);
test('publishing creates the first tag, bumps every version file on update and repairs a rerun without another bump', { timeout: 60000 }, async t => {
  if (process.platform === 'win32') { t.skip('the fake gh executable shim is POSIX-only'); return; }
  const root = await realpath(await mkdtemp(join(tmpdir(), 'goon-publish-test-')));
  t.after(() => rm(root, { recursive: true, force: true }));
  const work = join(root, 'work'), remote = join(root, 'remote.git'), bin = join(root, 'fake-bin');
  await mkdir(join(work, 'scripts'), { recursive: true });
  await mkdir(join(work, 'bin'));
  await mkdir(bin);
  const git = async (...args) => (await exec('git', args, { cwd: work })).stdout.trim();
  await exec('git', ['init', '--bare', '--initial-branch=main', remote]);
  await git('init', '-b', 'main');
  await git('config', 'user.name', 'Release Test');
  await git('config', 'user.email', 'test@example.invalid');
  await git('remote', 'add', 'origin', remote);
  const manifest = { name: 'goon-cli', version: '0.1.0', private: true, type: 'module', bin: { goon: 'bin/goon.mjs' }, files: ['bin', 'vendor/manifest.json', 'THIRD-PARTY-NOTICES.txt'] };
  await writeFile(join(work, 'package.json'), JSON.stringify(manifest));
  await writeFile(join(work, 'npm-shrinkwrap.json'), JSON.stringify({ name: 'goon-cli', version: '0.1.0', lockfileVersion: 3, requires: true, packages: { '': { name: 'goon-cli', version: '0.1.0' } } }));
  await writeFile(join(work, 'Cargo.toml'), '[package]\nname = "goon-cli"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nserde = { version = "1" }\n');
  await writeFile(join(work, 'Cargo.lock'), '# generated\nversion = 4\n\n[[package]]\nname = "goon-cli"\nversion = "0.1.0"\n\n[[package]]\nname = "serde"\nversion = "1.0.0"\n');
  await writeFile(join(work, '.gitignore'), 'node_modules/\nrelease-out/\nrelease-bin/\nvendor/\nTHIRD-PARTY-NOTICES.txt\n');
  await writeFile(join(work, 'THIRD-PARTY-NOTICES.txt'), 'Third-party software in the goon binary\n');
  await mkdir(join(work, 'release-bin'));
  for (const triple of TARGETS) await writeFile(join(work, 'release-bin', assetName(triple)), gzipSync(randomBytes(4096)));
  await writeFile(join(work, 'bin/goon.mjs'), "#!/usr/bin/env node\nimport {readFileSync} from 'node:fs'; const p=JSON.parse(readFileSync(new URL('../package.json',import.meta.url),'utf8')); console.log('goon cli '+p.version);\n");
  for (const file of ['pack-release.mjs', 'publish-release.mjs', 'release-version.mjs', 'install.mjs']) await copyFile(join('scripts', file), join(work, 'scripts', file));
  const state = join(root, 'releases.json'), log = join(root, 'gh.log');
  await writeFile(state, '{}');
  await writeFile(join(bin, 'gh'), `#!/usr/bin/env node
const fs=require('node:fs');
const args=process.argv.slice(2), [resource,command,tag]=args;
if(resource!=='release') throw Error('Unexpected gh command');
const state=JSON.parse(fs.readFileSync(process.env.GH_TEST_STATE,'utf8'));
fs.appendFileSync(process.env.GH_TEST_LOG,JSON.stringify(args)+'\\n');
if(command==='create'){if(state[tag])throw Error('Duplicate release');state[tag]={draft:true};}
if(command==='upload'){for(const f of args.slice(3)){if(f.startsWith('--'))break;if(!fs.existsSync(f))throw Error('Missing asset '+f);}}
if(command==='edit')state[tag].draft=false;
fs.writeFileSync(process.env.GH_TEST_STATE,JSON.stringify(state));
`, { mode: 0o755 });
  const preload = join(root, 'mock-fetch.mjs');
  await writeFile(preload, `import {readFileSync} from 'node:fs';
globalThis.fetch=async url=>{
 if(!String(url).startsWith('https://api.github.com/repos/test/goon-cli/releases/tags/'))throw Error('Unexpected request '+url);
 const tag=String(url).split('/').at(-1), state=JSON.parse(readFileSync(process.env.GH_TEST_STATE,'utf8'));
 return new Response(JSON.stringify(state[tag]??{}),{status:state[tag]?200:404,headers:{'content-type':'application/json'}});
};\n`);
  await git('add', '.');
  await git('commit', '-m', 'initial source');
  await git('push', 'origin', 'main');
  const first = await git('rev-parse', 'HEAD');
  const publish = async source => exec(process.execPath, ['scripts/publish-release.mjs'], {
    cwd: work, timeout: 20000, env: { ...process.env, PATH: bin + delimiter + process.env.PATH,
      GH_TOKEN: 'fake-local-test-token', GITHUB_REPOSITORY: 'test/goon-cli', SOURCE_SHA: source,
      GH_TEST_STATE: state, GH_TEST_LOG: log, NODE_OPTIONS: '--import=' + pathToFileURL(preload).href },
  });
  await publish(first);
  assert.equal(await git('rev-parse', 'v0.1.0^{commit}'), first);
  assert.equal(JSON.parse(await readFile(state, 'utf8'))['v0.1.0'].draft, false);
  await writeFile(join(work, 'change.txt'), 'new source');
  await git('add', 'change.txt');
  await git('commit', '-m', 'source update');
  await git('push', 'origin', 'main');
  const update = await git('rev-parse', 'HEAD');
  await publish(update);
  const released = await git('rev-parse', 'HEAD');
  assert.notEqual(released, update);
  assert.equal(await git('rev-parse', 'v0.1.1^{commit}'), released);
  assert.equal((await exec('git', ['--git-dir', remote, 'rev-parse', 'refs/heads/main'])).stdout.trim(), released);
  assert.equal(JSON.parse(await readFile(join(work, 'package.json'), 'utf8')).version, '0.1.1');
  assert.equal(JSON.parse(await readFile(join(work, 'npm-shrinkwrap.json'), 'utf8')).packages[''].version, '0.1.1');
  assert.match(await readFile(join(work, 'Cargo.toml'), 'utf8'), /^\[package\]\nname = "goon-cli"\nversion = "0\.1\.1"\n/);
  const cargoLock = await readFile(join(work, 'Cargo.lock'), 'utf8');
  assert.match(cargoLock, /name = "goon-cli"\nversion = "0\.1\.1"/);
  assert.match(cargoLock, /name = "serde"\nversion = "1\.0\.0"/, 'other lock entries are untouched');
  assert.deepEqual((await git('show', '--name-only', '--format=', 'HEAD')).split('\n').sort(), ['Cargo.lock', 'Cargo.toml', 'npm-shrinkwrap.json', 'package.json']);
  const vendored = JSON.parse(await readFile(join(work, 'vendor', 'manifest.json'), 'utf8'));
  assert.equal(vendored.version, '0.1.1');
  assert.deepEqual(Object.keys(vendored.assets).sort(), [...TARGETS].sort());
  await git('checkout', '--detach', update);
  await publish(update);
  assert.equal(await git('rev-parse', 'HEAD'), released);
  assert.deepEqual((await git('tag', '--list')).split('\n'), ['v0.1.0', 'v0.1.1']);
  const calls = (await readFile(log, 'utf8')).trim().split('\n').map(JSON.parse);
  assert.equal(calls.filter(args => args[1] === 'create').length, 2);
  const uploads = calls.filter(args => args[1] === 'upload');
  assert.equal(uploads.length, 3);
  const uploaded = uploads[0].slice(3, uploads[0].indexOf('--repo')).map(f => f.split(/[\\/]/).at(-1));
  assert.deepEqual(uploaded.sort(), ['SHA256SUMS.txt', 'THIRD-PARTY-NOTICES.txt', 'goon-cli-0.1.0.tgz', 'goon-cli.tgz', ...TARGETS.map(assetName)].sort());
});
