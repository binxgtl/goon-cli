// Tags and publishes a GitHub release from CI. The plan job already chose the version (see
// release-version.mjs) and the build jobs stamped it into the binaries in release-bin/.
import { readFile, writeFile } from 'node:fs/promises';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { planRelease, setManifestVersion, setCargoVersion, VERSION_FILES } from './release-version.mjs';
import { packRelease, command } from './pack-release.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const git = (...args) => command('git', args, root);
const repository = process.env.GITHUB_REPOSITORY;
const token = process.env.GH_TOKEN;
const source = process.env.SOURCE_SHA;
const planned = process.env.RELEASE_VERSION;
if (!repository || !/^[\w.-]+\/[\w.-]+$/.test(repository) || !token || !/^[a-f0-9]{40}$/.test(source ?? '')) {
  throw new Error('Publishing requires GITHUB_REPOSITORY, GH_TOKEN and a full SOURCE_SHA.');
}

const plan = await planRelease(root, git);
if (plan.skip) { console.log(`${plan.reason} Release skipped.`); process.exit(0); }
if (plan.checkout) await git('checkout', '--detach', plan.checkout);
let { head } = plan;
const { remote, tags, version } = plan;
if (planned && planned !== version) throw new Error(`The binaries were built as ${planned} but this release is ${version}; rerun the workflow.`);
if (await git('rev-parse', `${source}^{commit}`) !== source) throw new Error('Invalid source commit.');

const read = name => readFile(join(root, name), 'utf8');
const manifest = JSON.parse(await read('package.json'));
const changed = manifest.version !== version;
if (changed) {
  const synchronized = setManifestVersion(manifest, JSON.parse(await read('npm-shrinkwrap.json')), version);
  await writeFile(join(root, 'package.json'), `${JSON.stringify(synchronized.manifest, null, 2)}\n`);
  await writeFile(join(root, 'npm-shrinkwrap.json'), `${JSON.stringify(synchronized.lock, null, 2)}\n`);
  const cargo = setCargoVersion(await read('Cargo.toml'), await read('Cargo.lock'), manifest.version, version);
  await writeFile(join(root, 'Cargo.toml'), cargo.cargoToml);
  await writeFile(join(root, 'Cargo.lock'), cargo.cargoLock);
}
const packed = await packRelease(root, { repository });
const tag = `v${version}`;
await git('config', 'user.name', 'github-actions[bot]');
await git('config', 'user.email', '41898282+github-actions[bot]@users.noreply.github.com');
if (changed) {
  await git('add', ...VERSION_FILES);
  await git('commit', '-m', `chore: release ${tag} [skip ci]`);
  head = await git('rev-parse', 'HEAD');
}
const existing = tags.find(t => t.name === tag);
if (existing && existing.sha !== head) throw new Error(`Refusing to move existing tag ${tag}.`);
const liveHead = (await git('ls-remote', 'origin', 'refs/heads/main')).split(/\s/)[0];
if (liveHead !== remote) { console.log('Main advanced during packaging; release skipped.'); process.exit(0); }
if (!existing) await git('tag', '-a', tag, '-m', `Goon CLI ${tag}`, head);
const refs = changed ? ['HEAD:refs/heads/main', `refs/tags/${tag}`] : [`refs/tags/${tag}`];
await git('push', '--atomic', 'origin', ...refs);

const releaseResponse = await fetch(`https://api.github.com/repos/${repository}/releases/tags/${tag}`, {
  headers: { authorization: `Bearer ${token}`, accept: 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28' },
});
if (releaseResponse.status !== 404 && !releaseResponse.ok) throw new Error(`Cannot inspect existing release: HTTP ${releaseResponse.status}`);
const release = releaseResponse.ok ? await releaseResponse.json() : null;
const targets = Object.keys(packed.manifest.assets).map(t => `- \`${packed.manifest.assets[t].file}\` (${t})`).join('\n');
const notes = join(packed.output, 'release-notes.md');
await writeFile(notes, `Goon CLI ${tag}

Independent coding harness for Termux and external APIs — a native Rust binary with a small npm wrapper.

Install or update (Node >=22.16.0; the matching binary is downloaded and SHA-256 verified on install):

\`\`\`bash
npm install -g https://github.com/${repository}/releases/latest/download/goon-cli.tgz --omit=dev
\`\`\`

Or download a binary directly and \`gunzip\` it:

${targets}

Checks: cargo fmt, clippy and tests plus the end-to-end Node tests on Linux, macOS and Windows; the packed archive is installed and smoke-tested. No paid API or Android-device verification is performed by CI.

Source commit: ${head}
`);
if (!release) await command('gh', ['release', 'create', tag, '--repo', repository, '--verify-tag', '--draft', '--title', `Goon CLI ${tag}`, '--notes-file', notes], root);
await command('gh', ['release', 'upload', tag, ...packed.files, '--repo', repository, '--clobber'], root);
await command('gh', ['release', 'edit', tag, '--repo', repository, '--draft=false', '--latest'], root);
console.log(`Published https://github.com/${repository}/releases/tag/${tag}`);
