# Goon CLI 0.1

Independent implementation informed by ZCode's separation of runtime, provider,
tools and persistence. No ZCode runtime source is copied or loaded.

## Owners and boundaries

- CLI owns terminal input, flags and lifetime. User config is the only config
  file read. Priority: flags > GOON_* environment > user config > defaults.
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
  as the current user, without an OS sandbox. Plan mode omits all mutations and
  shell. --yes explicitly authorizes them without prompts.
- Help/init/doctor/sessions perform no model request. No account login, plugin
  marketplace, auto-updater, telemetry, browser, native addons or bundled runtime.
- Cancellation aborts HTTP, approvals and process groups, records terminal tool
  results, and checkpoints before shutdown. Permission denial ends the current
  run rather than inviting repeated requests.
- Context is compacted before the next request when a conservative character
  estimate or measured input exceeds 70% of the declared window. Only complete
  tool-result groups enter a compact summary. Default run token budget: 100,000;
  zero disables it. Output/tool budgets and command deadlines are bounded.

## Acceptance

Run against a local mock HTTP provider through the actual CLI: streaming tool
calls, read/write/edit, denied approval, cancellation, authentication failure,
resume and crash repair. Unit tests cover config validation, locks, symlink
escapes, stale revisions, output limits and subprocess cleanup. Linux tests do
not establish Android device compatibility.
