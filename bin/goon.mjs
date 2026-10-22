#!/usr/bin/env node
// Fast path: answer --version / --help without loading the AI SDK, MCP SDK and TUI (~10x faster).
const args = process.argv.slice(2);
if (args.length === 1 && (args[0] === '--version' || args[0] === '-v')) {
  const { VERSION } = await import('../dist/version.js');
  process.stdout.write(`goon cli ${VERSION}\n`);
} else if (args.length === 1 && (args[0] === '--help' || args[0] === '-h')) {
  const { HELP } = await import('../dist/help.js');
  process.stdout.write(HELP);
} else {
  const { main } = await import('../dist/cli.js');
  await main();
}
