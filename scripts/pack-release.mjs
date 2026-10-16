import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFile, writeFile, copyFile, mkdir, mkdtemp, rm } from 'node:fs/promises';
import { join, resolve, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { parseVersion } from './release-version.mjs';

const exec = promisify(execFile);
export async function command(program, args, cwd) {
  const { stdout } = await exec(program, args, { cwd, maxBuffer: 4 * 1024 * 1024 });
  return stdout.trim();
}

export async function packRelease(root = resolve(dirname(fileURLToPath(import.meta.url)), '..')) {
  const { version } = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'));
  parseVersion(version);
  const output = join(root, 'release-out');
  await mkdir(output, { recursive: true });
  await command('npm', ['run', 'build'], root);
  const packed = JSON.parse(await command('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', output], root));
  const filename = `goon-cli-${version}.tgz`;
  if (packed.length !== 1 || packed[0].filename !== filename) throw new Error('Unexpected npm pack output.');
  const archive = join(output, filename);
  const prefix = await mkdtemp(join(tmpdir(), 'goon-release-'));
  try {
    await command('npm', ['install', '--prefix', prefix, archive, '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'], root);
    const bin = join(prefix, 'node_modules', 'goon-cli', 'bin', 'goon.mjs');
    const reported = await command(process.execPath, [bin, '--version'], root);
    if (reported !== `goon cli ${version}`) throw new Error(`Installed archive reports the wrong version: ${reported}`);
    await command(process.execPath, [bin, '--help'], root);
  } finally { await rm(prefix, { recursive: true, force: true }); }
  await copyFile(archive, join(output, 'goon-cli.tgz'));
  const digest = createHash('sha256').update(await readFile(archive)).digest('hex');
  const checksums = join(output, 'SHA256SUMS.txt');
  await writeFile(checksums, `${digest}  ${filename}\n${digest}  goon-cli.tgz\n`);
  return { version, files: [archive, join(output, 'goon-cli.tgz'), checksums], output };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  console.log(JSON.stringify(await packRelease()));
}
