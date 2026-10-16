# Validation — 0.1.0

Executed on Linux x64, Node.js 24.19.0.

- `npm test`: **24 passed, 0 failed**. Tests use local mock HTTP servers and
  the real CLI, AI SDK adapters, filesystem, process scheduler and session store.
- `npm run typecheck`: passed, TypeScript strict mode.
- `npm run lint`: passed without warnings.
- Installed the packed npm archive in a separate prefix with development
  dependencies omitted and lifecycle scripts disabled. Its executable, help,
  version and a real harness → mock HTTP → write_file → final response cycle
  passed. Installed runtime tree contained **zero `.node` native addons**.
- One local `goon --help` sample: 0.124 seconds and 51,864 KiB peak child RSS.
  This is a Linux development-machine observation, not a Termux benchmark or a
  measurement of memory use during a long agent run.

Coverage includes OpenAI-compatible streamed tool arguments and reasoning
roundtrip, Anthropic Messages streaming, OpenAI Responses streaming,
write/read/edit across a resumed session, offline init/doctor, invalid config,
credential redaction, denied mutations, read-only tool surfaces, SIGINT during
HTTP, incomplete streams, bounded parallel reads and mutation barriers,
cancellation result pairing, compaction/archive, oversized input, stale file
revisions, exact edits, symlink/path escapes, fallback search, output truncation,
process-group cancellation, command timeouts, exclusive session locks, stale
owner recovery and workspace-bound resume.

No paid API request was made. No Android/Termux device, ARM64, macOS or Windows
was available for execution. Termux support is an implementation target, not a
device-tested claim. OS-level shell sandboxing is not implemented. File path
validation is intended to prevent ordinary traversal/symlink mistakes; it is
not a security boundary against another hostile process sharing the OS account.

Runtime package tree contains JavaScript dependencies; platform-specific lint
binaries are development dependencies only. The installable npm archive
includes compiled `dist/`, so Termux installation does not need TypeScript or
native builds.
