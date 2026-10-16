# Goon CLI 0.2

Independent implementation informed by ZCode's separation of runtime, provider,
tools and persistence. No ZCode runtime source is copied or loaded.

## Owners and boundaries

- CLI owns terminal input, flags and lifetime. Provider configuration comes
  only from user config, environment and flags. MCP has a separate user-only
  mcp.json. Priority: flags > GOON_* environment > user config > defaults.
  Credentials are resolved at runtime through keyEnv and are never saved by init.
- Harness owns ModelMessage history, token accounting, tool result pairing and
  compaction. A streamed response must complete before any tool executes. Reads
  can run in consecutive batches of two; mutations execute in declaration order.
- SessionStore owns atomic JSON checkpoints and a process lock. A session is
  bound to its real workspace. Loading repairs interrupted tool calls with error
  results; it never replays a side effect. A live lock is never stolen.
- Provider owns HTTP/model protocol handling through AI SDK: OpenAI chat,
  OpenAI Responses, Anthropic Messages. Two retries; configurable request timeout.
- Tools own file/shell I/O. File paths resolve within the real workspace;
  symlink escapes fail. Existing files must be read before overwrite/edit;
  changed revisions are rejected. Shell commands require approval and execute
  as the current user, without an OS sandbox. Read-only mode omits all mutations
  and shell. --yes explicitly authorizes them without prompts.
- Help/init/doctor/sessions perform no model request. No account login, plugin
  marketplace, auto-updater, telemetry, browser, native addons or bundled runtime.
- Cancellation aborts HTTP, approvals and process groups, records terminal tool
  results, and checkpoints before shutdown. Permission denial ends the current
  run rather than inviting repeated requests.
- Context is compacted before the next request when a conservative character
  estimate or measured input exceeds 70% of the declared window. Only complete
  tool-result groups enter a compact summary. Default run token budget: 100,000;
  zero disables it. Output/tool budgets and command deadlines are bounded.
- MCP owns clients and transport cleanup. SDK stdio and Streamable HTTP tools
  are namespaced, schema-validated and bounded. Only user readOnlyTools entries
  bypass approvals; remote annotations do not grant privileges. Stdio inherits
  the SDK default environment plus explicit envFrom mappings. HTTP bearer keys
  come from env variables. No sampling, OAuth, resources, prompts or legacy SSE.
- Subagents own separate transcripts under subagents/PARENT_UUID. Research uses
  at most two concurrent children globally per manager; editing tasks are serial
  mutation barriers. Children share the parent's approval callback and run budget,
  have a 30k local budget, no MCP and no child delegation. Parent cancellation
  drains children; child permission denial stops the parent. Budgets are checked
  between requests and concurrent in-flight requests can exceed the threshold.
- TerminalUI owns the alternate screen, raw input, resize, scrollback and keyboard
  cleanup. It is enabled only for interactive TTYs; --no-tui, TERM=dumb, prompt
  mode and JSON retain plain output. Terminal controls in model/tool data are
  stripped. No native UI dependency is required.
- AnswerProjection owns display compatibility for leading think blocks. It never
  mutates SDK response messages or signed provider replay metadata. Extraction
  is incremental across chunk boundaries, ends once visible answer text starts,
  and can be disabled. Reasoning is opt-in on a separate event/display channel.
  Compaction uses the visible summary and refuses reasoning-only output.
- Every terminal model step without tool calls requires non-whitespace projected
  answer text, independent of observed reasoning events. Earlier tool narration
  and successful child reports cannot stand in for the parent's final answer.
  Missing answers checkpoint usage/history and fail without automatic retries or
  replaying completed tools. Tool-only intermediate steps remain valid; explicit
  permission denial retains its stopped/denied outcome and exit code 2.
- Child start/progress/terminal events form a discriminated union. Only start
  includes task; progress excludes text/reasoning. Child reports select the last
  non-empty visible assistant answer. Terminal publication follows session close,
  and capacity is released on cleanup failures as well as successful runs.
  See REFERENCE_REVIEW.md for exact source references and NDJSON migration.

## Acceptance

Run against a local mock HTTP provider through the actual CLI: streaming tool
calls, read/write/edit, denied approval, cancellation, authentication failure,
resume and crash repair. Unit tests cover config validation, locks, symlink
escapes, stale revisions, output limits and subprocess cleanup. Exact existing
file permissions survive restrictive umasks; new files obey umask. CI runs the
full suite at default, 027 and 007 umasks. MCP tests use real SDK stdio/HTTP
servers; subagent tests use the provider loop and cancellation. A POSIX PTY
test exercises TUI input, resize, approval, terminal restoration and clean exit.
Linux tests do not establish Android device compatibility.

## Automated GitHub releases

Pushes to main and manual workflow dispatch run checks on Node 22.16 and 24.
Pull requests run read-only checks without release permissions. After checks,
the newest main commit is packed and installed in an isolated prefix. The first
release uses package.json's stable version. Later changed commits auto-increment
the latest patch, unless package.json declares a higher stable version. Version
changes update package.json and npm-shrinkwrap.json in a bot commit before the
annotated tag is pushed. CLI version output reads its installed manifest.

Publishing owns tag/release state. Existing tags are never moved. A rerun at the
same tagged commit repairs the same release instead of bumping again. Superseded
source commits are skipped. Release jobs are serialized, use contents:write only
in their publishing job, upload npm archives plus SHA-256 checksums to a draft,
and publish the release only after uploads complete. The repository GITHUB_TOKEN
does not trigger recursive push workflows. No npm registry publishing or
automatic updating of installed clients is enabled by this workflow.
