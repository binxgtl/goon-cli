// Turns the interesting lines of failed CI logs into GitHub annotations, so a failure can be read
// from the run summary (and the checks API) without downloading the raw job log.
import { existsSync, readFileSync } from 'node:fs';

const INTERESTING = /panicked at|^test .* FAILED$|^---- .* ----$|assertion|left:|right:|^not ok|^\s+not ok|error\[|^error:|Error:|expected|actual|failed/i;
const escape = text => text.replace(/%/g, '%25').replace(/\r/g, '').replace(/\n/g, '%0A');
let emitted = 0;
for (const file of process.argv.slice(2)) {
  if (!existsSync(file)) continue;
  const lines = readFileSync(file, 'utf8').split('\n');
  const picked = [];
  lines.forEach((line, i) => {
    if (INTERESTING.test(line)) for (let j = Math.max(0, i - 1); j <= Math.min(lines.length - 1, i + 3); j++) picked.push(j);
  });
  const unique = [...new Set(picked)].sort((a, b) => a - b);
  const chunks = [];
  for (let i = 0; i < unique.length && chunks.length < 8; i += 40) chunks.push(unique.slice(i, i + 40).map(n => lines[n].slice(0, 300)).join('\n'));
  if (!chunks.length) chunks.push(lines.slice(-60).join('\n'));
  for (const [n, chunk] of chunks.entries()) {
    console.log(`::error title=${file} (${n + 1}/${chunks.length})::${escape(chunk)}`);
    emitted++;
  }
}
if (!emitted) console.log('::notice::No failure logs were captured.');
