import { readFile, writeFile } from 'node:fs/promises';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { selectVersion, setManifestVersion } from './release-version.mjs';
import { packRelease, command } from './pack-release.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const git = (...args) => command('git', args, root);
const repository = process.env.GITHUB_REPOSITORY;
const token = process.env.GH_TOKEN;
const source = process.env.SOURCE_SHA;
if (!repository || !/^[\w.-]+\/[\w.-]+$/.test(repository) || !token || !/^[a-f0-9]{40}$/.test(source ?? '')) {
  throw new Error('Publishing requires GITHUB_REPOSITORY, GH_TOKEN and a full SOURCE_SHA.');
}

await git('fetch', 'origin', 'main', '--tags');
let head = await git('rev-parse', 'HEAD');
const remote = await git('rev-parse', 'origin/main');
if (remote !== head) {
  // A retry may start at the source commit while main already contains this
  // workflow's version-only child commit. Any unrelated newer work wins.
  const parent = await git('rev-parse', `${remote}^`);
  const subject = await git('log', '-1', '--format=%s', remote);
  const files = (await git('diff', '--name-only', head, remote)).split('\n').filter(Boolean);
  const releaseChild = parent === head && /^chore: release v\d+\.\d+\.\d+ \[skip ci\]$/.test(subject) &&
    files.length > 0 && files.every(p => ['package.json', 'npm-shrinkwrap.json'].includes(p));
  if (!releaseChild) { console.log('A newer main commit superseded this run; release skipped.'); process.exit(0); }
  await git('checkout', '--detach', remote);
  head = remote;
}
if (await git('rev-parse', `${source}^{commit}`) !== source) throw new Error('Invalid source commit.');
const tagNames = (await git('tag', '--list', 'v*')).split('\n').filter(Boolean);
const tags = [];
for (const name of tagNames) tags.push({ name, sha: await git('rev-parse', `${name}^{commit}`) });
const manifestPath = join(root, 'package.json'), lockPath = join(root, 'npm-shrinkwrap.json');
const manifest = JSON.parse(await readFile(manifestPath, 'utf8'));
const lock = JSON.parse(await readFile(lockPath, 'utf8'));
const version = selectVersion(manifest.version, tags, head);
const synchronized = setManifestVersion(manifest, lock, version);
const changed = manifest.version !== version;
if (changed) {
  await writeFile(manifestPath, `${JSON.stringify(synchronized.manifest, null, 2)}\n`);
  await writeFile(lockPath, `${JSON.stringify(synchronized.lock, null, 2)}\n`);
}
const packed = await packRelease(root);
const tag = `v${version}`;
await git('config', 'user.name', 'github-actions[bot]');
await git('config', 'user.email', '41898282+github-actions[bot]@users.noreply.github.com');
if (changed) {
  await git('add', 'package.json', 'npm-shrinkwrap.json');
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
const notes = join(packed.output, 'release-notes.md');
await writeFile(notes, `Goon CLI ${tag}\n\nIndependent coding harness for Termux and external APIs.\n\nChecks: Node 22.16 and 24 tests, TypeScript and lint; packed install/version smoke test.\n\nInstall or update:\n\n\`\`\`bash\nnpm install -g https://github.com/${repository}/releases/latest/download/goon-cli.tgz --omit=dev\n\`\`\`\n\nAssets include the versioned npm package, a stable goon-cli.tgz alias and SHA-256 checksums. Requires Node >=22.16.0. No paid API or Android-device verification is performed by CI.\n\nSource commit: ${head}\n`);
if (!release) await command('gh', ['release', 'create', tag, '--repo', repository, '--verify-tag', '--draft', '--title', `Goon CLI ${tag}`, '--notes-file', notes], root);
await command('gh', ['release', 'upload', tag, ...packed.files, '--repo', repository, '--clobber'], root);
await command('gh', ['release', 'edit', tag, '--repo', repository, '--draft=false', '--latest'], root);
console.log(`Published https://github.com/${repository}/releases/tag/${tag}`);
