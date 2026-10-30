// Writes THIRD-PARTY-NOTICES.txt: the license of every crate linked into the goon binary (normal
// dependencies for any target), with the license texts shipped in each crate. Identical texts are
// printed once and list every crate that uses them. Needs cargo; run before packing a release.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense)([-._].*)?$/i;

export function linkedPackages(metadata) {
  const nodes = new Map(metadata.resolve.nodes.map(n => [n.id, n]));
  const root = metadata.resolve.root;
  const seen = new Set([root]);
  const queue = [root];
  while (queue.length) {
    const node = nodes.get(queue.shift());
    for (const dep of node?.deps ?? []) {
      // Dev-dependencies never reach the binary; build-dependencies only run at compile time.
      if (!dep.dep_kinds.some(k => k.kind === null) || seen.has(dep.pkg)) continue;
      seen.add(dep.pkg);
      queue.push(dep.pkg);
    }
  }
  seen.delete(root);
  return metadata.packages.filter(p => seen.has(p.id)).sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
}

export function renderNotices(packages, readLicenseFiles) {
  const texts = new Map();
  const lines = [
    'Third-party software in the goon binary',
    '',
    'goon is MIT licensed (see LICENSE). The native binary statically links the Rust crates below.',
    'Each entry gives the crate, its version, its declared license and the license files it ships.',
    '',
  ];
  for (const p of packages) {
    const files = readLicenseFiles(p);
    const refs = [];
    for (const { name, text } of files) {
      const normalized = text.replace(/\r\n/g, '\n').trim();
      const hash = createHash('sha256').update(normalized).digest('hex').slice(0, 12);
      if (!texts.has(hash)) texts.set(hash, { text: normalized, users: [] });
      texts.get(hash).users.push(`${p.name} ${p.version} (${name})`);
      refs.push(`[${hash}]`);
    }
    const source = p.repository ? `  ${p.repository}` : '';
    lines.push(`- ${p.name} ${p.version}: ${p.license ?? p.license_file ?? 'unknown'}${source}${refs.length ? `\n  license texts: ${refs.join(' ')}` : ''}`);
  }
  lines.push('', '', 'License texts', '=============');
  for (const [hash, { text, users }] of texts) {
    lines.push('', `[${hash}] used by: ${users.join(', ')}`, '-'.repeat(78), text);
  }
  return `${lines.join('\n')}\n`;
}

export function thirdPartyNotices(root) {
  const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--locked'], { cwd: root, maxBuffer: 64 * 1024 * 1024, encoding: 'utf8' }));
  const read = p => {
    const dir = dirname(p.manifest_path);
    return readdirSync(dir).filter(n => LICENSE_FILE.test(n)).sort().map(name => ({ name, text: readFileSync(join(dir, name), 'utf8') }));
  };
  const packages = linkedPackages(metadata);
  const missing = packages.filter(p => read(p).length === 0 && !p.license);
  if (missing.length) throw new Error(`No license information for: ${missing.map(p => `${p.name} ${p.version}`).join(', ')}`);
  return renderNotices(packages, read);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const out = join(root, 'THIRD-PARTY-NOTICES.txt');
  writeFileSync(out, thirdPartyNotices(root));
  console.log(`wrote ${out}`);
}
