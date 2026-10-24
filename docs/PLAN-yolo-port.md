# Kế hoạch port ý tưởng yolo-code sang goon-cli

> Tài liệu này là kế hoạch duy nhất cho AI agent chuyên triển khai. Nó hợp nhất bốn draft trong `.zcode/plans/goon-yolo-port/drafts/` (verification, cost-risk, events-tests, memory-agents), giải quyết xung đột, loại trùng lặp, thống nhất tên module/config/event, và đặt thứ tự triển khai hợp lý. Chỉ viết tài liệu; **không sửa source code** trong repo.

## 0. Cách dùng tài liệu này (cho AI agent triển khai)

1. **Đọc toàn bộ plan trước khi viết code.** Không bắt đầu từ giữa file. Nếu có mâu thuẫn với source thực tế tại thời điểm triển khai, dừng và báo cáo, không tự ý "sửa cho khớp".
2. **Làm từng PR** theo §9. Mỗi PR nhỏ, có Definition of Done (DoD). Không gộp nhiều PR thành một commit lớn. Không refactor ngoài phạm vi PR.
3. **Chạy lệnh nghiệm thu** §10 cho mỗi PR và báo cáo output thật (stdout/stderr/exit code). Không bịa kết quả. Nếu lệnh fail vì môi trường (Windows mode bits, shim `gh`), ghi rõ là môi trường, không coi là regression logic.
4. **Không thêm native dependency.** Chỉ module thuần JS/TS. Nếu cần parser/AST, dùng heuristic thuần TS hoặc package JS đã có.
5. **Giữ backward compatibility.** Config cũ phải parse được nhờ defaults; session schema v1 không đổi; HarnessEvent chỉ thêm field optional; CLI flag mặc định không bật tính năng mới (event log mặc định tắt, rag mặc định tắt; `verify.enabled` đề xuất true nhưng chỉ chạy sau approval — Q1 §11).
6. **Không phá invariant bảo mật.** API key không vào env con; approval không phải sandbox; không gửi secret ra ngoài; redact trước khi log/persist. Risk class chỉ gate, không thay filesystem confinement.
7. **Cập nhật README/CHANGELOG** khi thêm lệnh/config/event mới (ít nhất CHANGELOG per version).
8. **Dừng và hỏi** (escalate) khi gặp quyết định sản phẩm mơ hồ (default on/off, scope, semantics). Các câu hỏi mở đã liệt kê §11; nếu triển khai gặp thêm, escalate thay vì đoán.
9. **Báo cáo output thật** cho mỗi PR: files changed, test count, exit codes, warnings.
10. **Baseline hiện tại:** `typecheck=0`, `lint=0`, `build=0`, `tests=1` (exit code 1; thực tế `138 tests / 133 pass / 3 fail / 2 skipped` trên Windows, 3 failure pre-existing: `tests/files.test.mjs:9`, `tests/files.test.mjs:25`, `tests/publish.test.mjs:11`; full suite xanh trên Termux/Linux). Không được thêm failure mới.

## 1. Bối cảnh và mục tiêu

- **Repo đích:** `goon-cli` — TypeScript, Node ≥22.16, ưu tiên Termux, test `node:test`, build `tsc`. Phiên bản hiện tại 0.3.1 (`package.json`).
- **Repo tham chiếu:** `C:/Users/Admin/AppData/Local/Temp/yolo-code` (Go). Chỉ đọc tài liệu (`05-Event_Bus.md`, `08-Execution_Engine.md`, `09-Verification_Engine.md`, `10-Patch_Engine.md`, `11-Memory_System.md`, `12-Coordination_Layer.md`, `13-Infrastructure.md`, `docs/security/sandbox-redteam.md`). **Không copy code Go.**
- **Mục tiêu:** port *ý tưởng* (không code) thành 4 lớp năng lực, chia 4 phiên bản:
  - **v0.4 Trust & Verify:** verification gate + auto-rollback, patch safety, cost ledger + caps, risk class + critical deny + rejection loop.
  - **v0.5 Observability & vận hành:** event log/replay, stub provider + golden JSONL, execution policy (network/wrapper), CI đa nền tảng.
  - **v0.6 Hiểu repo & multi-agent:** memory/RAG lexical, context budget, multi-agent DAG.
  - **v0.7 Khác biệt dài hơi:** dynamic workflow classifier, scope loop, multi-candidate reflection + cost-degrade (và OS sandbox, refreshable price table, full session restore, real embedder seam).
- **Tiêu chí thành công:** verify fail → model thấy lỗi → sửa → pass; rollback đúng byte; patch stale/ambiguous bị chặn; cost/time cap dừng run với lý do rõ; critical không chạy kể cả auto/session allowlist; event stream deterministic với stub; DAG đúng thứ tự; không thêm native dep; CI xanh 3 OS.

## 2. Ràng buộc và nguyên tắc

1. **Không thêm native dependency.** `package.json` dependencies giữ nguyên; chỉ stdlib + package JS đã có (`ai`, `zod`, `ajv`, MCP SDK).
2. **Backward compat.**
   - `configSchema` là `.strict()` nhưng field mới có `.default(...)`, config cũ parse thành công, không cần migration.
   - `sessionSchema` version 1 không đổi; field mới optional/default.
   - `HarnessEvent` chỉ thêm variant; consumer cũ bỏ qua unknown type.
   - `Harness` constructor thêm param cuối optional; callers cũ compile.
   - CLI flag mặc định không bật tính năng mới (event log tắt; rag tắt; DAG manual). Ngoại lệ có chủ đích: `verify.enabled` đề xuất `true` nhưng chỉ chạy sau khi user approve; đây là quyết định sản phẩm ở Q1 §11, có thể đổi default trước khi merge.
3. **Invariant bảo mật (không phá).**
   - API key không vào env con: `protectSecretEnv` + `childEnv` (`src/process.ts:13-20`).
   - Approval không phải sandbox: risk class chỉ gate; không claim confinement.
   - Không gửi secret ra ngoài: redact trước log/persist/index.
   - MCP trust không set `readOnly`, không bypass critical.
4. **Approval gate là lớp bảo vệ duy nhất.** Verify commands, bash, MCP đều đi qua `tools.approve` hoặc policy trước khi chạy.
5. **Không copy code Go** từ repo tham chiếu.
6. **Mọi thay đổi có test.** Không merge PR nếu test mới thiếu.
7. **Không track bash/MCP writes ở v0.4** (giới hạn đã biết, ghi rõ docs/tests).

## 3. Bản đồ kiến trúc hiện tại (file:line)

### Harness / turn lifecycle
- `src/harness.ts:22-33`: `HarnessEvent` union hiện có (`text`, `reasoning`, `tool_stream`, `tool`, `usage`, `compaction`, `todo`, `goal`, `plan_exit`, `retry`, `stopped`).
- `src/harness.ts:261`: `running` private; `src/harness.ts:300-303`: `busy` getter.
- `src/harness.ts:374-437`: `revert(...)` từ chối khi đang chạy (`src/harness.ts:375`).
- `src/harness.ts:438-668`: `run(prompt, signal, sharedBudget)`. `messageIndexBeforeTurn` `:449`, `tokensBeforeTurn` `:450`, `turnStartedAt` `:451`.
- `src/harness.ts:455-456`: `turnHadSuccessfulTool`, `turnHadFailedExec` reset đầu turn.
- `src/harness.ts:476`: `this.tools.files.beginTurn()`.
- `src/harness.ts:609-610`: `executeBatch` + denial check.
- `src/harness.ts:651-662`: `finally` tạo checkpoint từ `takeTurnBackups()` `:651`.
- `src/harness.ts:670-758`: `executeBatch`; kết quả tool `:732-735`, `meta.kind` edit `:682-691`.
- `src/harness.ts:270-272`: constructor `(config, store, tools, emit, model?)`.

### WorkspaceFiles / patch
- `src/files.ts:292`: class `WorkspaceFiles`; `:293` `revisions` Map; `:294` `activeBackups` Map.
- `src/files.ts:303-321`: `beginTurn`, `takeTurnBackups`, `mergeBackups`.
- `src/files.ts:322-335`: `recordBackup`.
- `src/files.ts:378-384`: `read` ghi `revisions` `:381`.
- `src/files.ts:406-425`: `write` stale check `:415`, record `:422`, writeRaw `:423`.
- `src/files.ts:426-453`: `edit` stale `:429`, exact+CRLF+trimmed fallback `:434-450`, ambiguous guard `:451`.
- `src/files.ts:459-537`: `applyPatch` planning slots `:461-523`, commit loop `:525-535`. Chưa check revisions/uniqueness/AST.
- `src/files.ts:543-585`: `restoreBackups` conflict via `afterHash` `:566-571`.
- `src/files.ts:255-290`: `seekSubsequence` fuzzy; `applyHunksToContent` `:277-290`.

### Tools / config / CLI / TUI
- `src/tools.ts:20-21`: `readOnly`; `:33-37`: `RegisteredTool`; `:606-622`: register `write_file`, `edit_file`, `apply_patch`; `:681`: `isExposed`; `:693-710`: `isReadOnly`; `:711-732`: `execute`; `:723`: approval gate.
- `src/config.ts:11-25`: `configSchema` strict; `:81-117`: `loadConfig`; `:119-126`: `resolveRuntime`.
- `src/cli.ts:319-332`: parseArgs flags; `:663-695`: `emit`; `:894`: subagent `Harness.run`; `:919`: `new Harness`; `:1107`: `harness.run`; `:2496-2555`: `/revert`.
- `src/tui.ts:1032-1103`: `event(...)` dispatch.
- `src/session.ts:8-27`: `fileBackupSchema`/`turnCheckpointSchema`; `:206-215`: `sessionSchema`; `:565-566`: `MAX_CHECKPOINT_BACKUP_BYTES`; `:574-592`: `pruneCheckpointBackups`.
- `src/process.ts:13-20`: `protectSecretEnv`/`childEnv`; `:21-22`: output limits; `:38-103`: `runCommand`.
- `src/provider.ts:125-140`: `createModel`.
- `src/context.ts:18-51`: `MemoryEntry`/`loadMemories`/`saveMemories`; `:189`: `loadProjectInstructions`; `:271-344`: `expandFileMentions`; `:480`: `SystemPromptCache`; `:485`: `systemPrompt`; `:572`: `estimatedTokens`; `:622`: `compactIfNeeded`; `:660`: `compactNow`; `:705`: `recapSession`.
- `src/subagents.ts:32`: `AgentRoleType`; `:172`: `Subagents`; `:191`: `subagentsRoot`; `:232`: `register`; `:385`: `spawnAgentTool`; `:474`: `executeManagedTurn` (merge `:526-531`); `:809`: `child` (merge `:852-858`); `:871`: `run`.
- `src/network.ts:171`: `AgentNetworkBus`; `:492`: `DEFAULT_NETWORK_TOKEN_BUDGET`; `:498`: `AgentNetwork`; `:510`: `brainstorm`; `:732`: `formatBrainstormReport`.
- `src/workflow.ts:11`: `WorkflowStep`; `:37`: `WorkflowStepResult`; `:92`: `BUILTIN_WORKFLOWS`; `:552`: `WorkflowExecutionHooks`; `:569`: `executeWorkflow`.
- `src/mcp.ts:603-611`: `McpToolSpec`; `:821-823`: `readOnly = config.trust || ...` (P13 bỏ trust khỏi readOnly); `:889-895`: `attach`.
- `src/approval.ts:12-34`: sets; `:56-66`: `bashApprovalPrefix`; `:72-86`: `isApprovedBashPrefix`.
- `src/tui.ts:135-163`: `SLASH_COMMANDS`; `src/help.ts:47`: help list.

### Tests / CI
- `tests/helpers.mjs`: `fixture`, `provider`, `sse`, `cli`.
- `.github/workflows/release.yml:18-42`: check job ubuntu only.
- Baseline: typecheck=0, lint=0, build=0, tests=1 (exit code 1; 138 tests / 133 pass / 3 fail / 2 skipped trên Windows; 3 failure pre-existing).

## 4. Lộ trình tổng quan v0.4 -> v0.7

| Version | Theme | Module mới | Acceptance chính |
|---|---|---|---|
| v0.4 | Trust & Verify | `src/verify.ts`, `src/syntax.ts`, `src/cost.ts`, `src/risk.ts` | verify fail→repair→pass/rollback; patch stale/ambiguous blocked; cost/time cap dừng run; critical deny trước approval |
| v0.5 | Observability & vận hành | `src/eventlog.ts`, `src/stub-provider.ts`, `src/stub-fixtures.ts`, `src/policy.ts` | event stream deterministic với stub; replay rebuild transcript tối thiểu; network default-deny; CI xanh 3 OS |
| v0.6 | Hiểu repo & multi-agent | `src/rag.ts`, `src/memory.ts`, `src/context-budget.ts`, `src/coord.ts` | retrieval không vượt budget; DAG đúng thứ tự; merge fail bị verify bắt; rollback đúng byte |
| v0.7 | Khác biệt dài hơi | (classifier, scope loop, reflection trong `src/workflow.ts`/`src/coord.ts`) | classifier conservative; scope loop không vượt budget; multi-candidate rerank; rollback candidate fail |

Dependency tổng quan: v0.4 → v0.5 → v0.6 → v0.7. Trong mỗi version, PR nhỏ có dependency riêng (§9).

**Quyết định hợp nhất (để agent không đặt tên khác):**

| Hạng mục | Tên thống nhất | Ghi chú |
|---|---|---|
| Syntax/patch | `src/syntax.ts` | Tách khỏi `verify.ts` để P01 độc lập |
| Verification | `src/verify.ts` | Không emit; caller emit event |
| Cost/risk/policy | `src/cost.ts`, `src/risk.ts`, `src/policy.ts` | `risk.ts` v0.4 không import `policy.ts`; policy land P12 |
| Event log/replay | `src/eventlog.ts` | `EVENT_LOG_VERSION=1`; `--event-log`; `goon replay` |
| Redaction | `src/redact.ts` | Dùng chung eventlog/memory/RAG; `redactSecrets`, `isSensitivePath` |
| Stub | `src/stub-provider.ts`, `src/stub-fixtures.ts` | Fixture mặc định là hằng TS |
| Memory/RAG | `src/rag.ts`, `src/memory.ts`, `src/context-budget.ts` | MemoryStore là entry duy nhất |
| Multi-agent DAG | `src/coord.ts` | `runDag` + Scheduler + merge |
| Harness deps | param 6 `HarnessDeps` | v0.4 chỉ `verify?`; P19 thêm `memory?` |
| Verify events | `verify_start`, `verify_result`, `verify_rollback` | Không dùng `stopped` cho rollback |
| Memory events | `memory_indexed`, `memory_retrieved`, `memory_error`, `context_budget` | Additive |
| DAG events | `dag_plan_invalid`, `dag_plan_ready`, `dag_todo_update`, `dag_merge` | Additive |
| v0.7 events | `workflow_classified`, `scope_round`, `cost_degrade` | Additive |
| Config | nested `verify`; top-level `maxCostUsd`, `maxTimeMs`, `pricing`, `allowNet*`, `stub*`, `memory*`, `rag*`, `context*`, `multiAgent*` | Tất cả có default |

## 5. v0.4 - Trust & Verify (chi tiết triển khai)

### 5.0 Hợp đồng chung v0.4

**HarnessDeps (thống nhất conflict constructor):**
```ts
// src/harness.ts
export interface HarnessDeps {
  verify?: { enabled?: boolean; runner?: VerifyRunner; policy?: VerifyPolicy };
  // memory?: MemoryStore;  // thêm ở P19 (v0.6), sau khi src/memory.ts tồn tại
}
```
Constructor:
```ts
constructor(readonly config: RuntimeConfig, readonly store: SessionStore, readonly tools: ToolRegistry,
  readonly emit: (event: HarnessEvent) => void = () => {}, model?: LanguageModel,
  private readonly deps?: HarnessDeps)
```
- Top-level CLI: v0.4 `new Harness(runtime, store, reg, emit, model)`; P19 (v0.6) truyền thêm `{ memory }`.
- Child harness (subagent/workflow/network): `new Harness(..., undefined, { verify: { enabled: false } })` (v0.4) để tránh rollback âm thầm. Callsites: `src/subagents.ts:506,834`, `src/cli.ts:894`, `src/network.ts:675,721`.
- Test integration: `new Harness(runtime, store, tools, emit, undefined, { verify: { runner, policy } })`.
- `verifyEnabled = this.deps?.verify?.enabled ?? this.config.verify.enabled` (config default true).

**HarnessEvent additions (v0.4):**
```ts
| { type: 'verify_start'; files: string[] }
| { type: 'verify_result'; status: VerifySeverity; stage?: VerifyStage; reason: string; errors: VerifyIssue[]; warnings: VerifyIssue[]; durationMs: number }
| { type: 'verify_rollback'; reason: string; restored: string[]; deleted: string[]; conflicts: string[] }
```
Import types từ `./verify.js`. `verify_rollback` là event riêng, **không** dùng `stopped`, để CLI không đánh dấu `denied`.

### 5.1 verification gate + auto-rollback

**Module mới `src/verify.ts`** (thuần TS, dùng `runCommand` từ `src/process.ts`).

Kiểu dữ liệu tối thiểu (dùng xuyên suốt plan):
```ts
export type VerifySeverity = 'pass' | 'fail' | 'warn' | 'skip';
export type VerifyStage = 'syntax' | 'format' | 'lint' | 'typecheck' | 'build' | 'test';
export interface VerifyIssue { stage: VerifyStage; path: string; line?: number; column?: number; message: string; rule?: string }
export interface VerifyStageResult { stage: VerifyStage; severity: VerifySeverity; durationMs: number; issues: VerifyIssue[]; output?: string; reason?: string }
export interface VerifyVerdict { pass: boolean; severity: VerifySeverity; stages: VerifyStageResult[]; errors: VerifyIssue[]; warnings: VerifyIssue[]; reason: string }
export interface VerifyPolicy { requireTests: boolean; requireLint: boolean; requireTypecheck: boolean; allowWarnings: boolean }
export interface VerifyConfig { enabled: boolean; commands: string[]; timeoutMs: number; maxRepairRounds: number; policy: VerifyPolicy }
export interface VerifyCommand { stage: VerifyStage; command: string; args: string[]; display: string }
export interface VerifyProject { kind: 'node' | 'python' | 'rust' | 'go' | 'unknown'; root: string; files: string[] }
export interface VerifyPlan { stages: VerifyStage[]; commands: VerifyCommand[]; projects: VerifyProject[]; approved?: boolean }
```
Defaults: `enabled=true`, `commands=[]`, `timeoutMs=120_000`, `maxRepairRounds=2`, `policy={ requireTests:false, requireLint:false, requireTypecheck:false, allowWarnings:true }`. `VerificationFailedError` giữ `verdict`; `formatVerifyErrors/Warnings` trả text ngắn cho user message; `redactVerdict(verdict, secrets)` redact `issues[].message`, `output`, `reason` trước khi push session/emit.

**Semantics của `require*` (bắt buộc, tránh ambiguity):**
- `requireTests=true` → chạy stage test và **phải pass**; `false` → **skip hoàn toàn** (không chạy `npm test`).
- `requireLint=true` → chạy lint (và format-check nếu có script) và phải pass; `false` → skip cả hai.
- `requireTypecheck=true` → chạy typecheck + build (nếu có script) và phải pass; `false` → skip cả hai.
- AST/syntax stage luôn chạy khi có file sửa, không phụ thuộc require flag.
- `verify.commands` non-empty **thay thế toàn bộ matrix**; require flag bị bỏ qua cho các stage đã có command.
- Tool/script không tồn tại → skip stage với `reason`, không fail; `allowWarnings=false` biến warning thành fail.

```ts
export interface VerifyRunnerResult { code: number; output: string; timedOut?: boolean; notFound?: boolean }
export interface VerifyRunner {
  run(command: string, args: string[], opts: { cwd: string; timeoutMs: number; signal: AbortSignal; env?: NodeJS.ProcessEnv }): Promise<VerifyRunnerResult>;
}
```

**Phát hiện project `detectProjects(cwd, changedFiles)`:**
- Chỉ dò trong `cwd`, không climb lên thư mục cha. Manifest ở cha → `unknown`, chỉ chạy AST.
- Manifest: `package.json` → node; `tsconfig.json`/`jsconfig.json` → node+TS; `pyproject.toml`/`setup.cfg`/`requirements.txt` → python; `Cargo.toml` → rust; `go.mod` → go.
- Trả project chứa changed files; không manifest → `unknown` (chỉ AST/skip command stages).

**Command matrix (mặc định, override qua config):**

| kind | AST | Format | Lint | TypeCheck | Build | Test |
|---|---|---|---|---|---|---|
| node (TS) | brace heuristic | prettier local | eslint local | tsc local | package script build | package script test / node --test |
| node (JS) | brace heuristic | prettier local | eslint local | skip | package script build | package script test / node --test |
| python | brace heuristic | black | ruff | mypy | skip | pytest |
| rust | brace heuristic | rustfmt | cargo clippy --offline | cargo check --offline | cargo check --offline | cargo test --offline |
| go | brace heuristic | gofmt -l | go vet ./... | go build ./... | go build ./... | go test ./... |
| unknown | skip | skip | skip | skip | skip | skip |

Nguyên tắc: prefer package scripts (`npm run lint/typecheck/build/test`) nếu có; resolve binary local qua `createRequire(import.meta.url)` → `node_modules/<pkg>/...`, chạy bằng `process.execPath`; không dùng bare `npx`/`npm` spawn (ENOENT win32). Tool binary/script của stage vắng mặt → skip; lỗi runtime, app module hoặc test fixture vắng mặt → fail (không skip). Network: Rust `--offline`, Go `GOPROXY=off` + `GOFLAGS=-mod=readonly`, Node chỉ chạy script có sẵn; không tự `npm install`. Repo hiện tại chỉ có `typescript` + `oxlint` (không eslint/prettier), nên matrix phải ưu tiên `npm run lint` (oxlint) và skip format khi tool vắng mặt.

**Scoping `scopeFiles(changedFiles, project)`:** lọc file tồn tại thuộc project; tool theo file nhận list changed (posix relative); tool theo package chạy ở root không truyền file list. Dedup+sort; bỏ `node_modules`, `.git`, `dist`.

**Timeout/output cap:** `runCommand(cmd, args, { cwd, signal, timeoutMs })`; timeout default 120000 clamp 1000-600000. Output cap runCommand ~64KB; verify cắt còn 8000 ký tự trong issue/detail; tổng cap 20 issues hoặc 16KB với suffix `[+N more]`. Timeout → stage `fail` với reason `timeout` (không skip, không warn); user abort → fail/cancelled.

**Override commands:** `verify.commands` mảng string. Mode: không bắt đầu `!` → split whitespace đơn giản (không shell expansion); bắt đầu `!` → `bash -c` (power user, rủi ro injection, chỉ khi user explicitly config; vẫn strip secret env qua `childEnv`). Default không override.

**Runner interface & resolve binary:** `VerifyRunner`, `VerifyRunnerResult`, `createVerifyRunner()`, `resolveLocalTool(root, tool)` (typescript → `node_modules/typescript/bin/tsc` qua process.execPath; eslint/prettier bin; npm script qua `npm_execpath` hoặc cmd.exe/npm fallback; python/rust/go bare binary với ENOENT→skip). Fake runner inject qua `HarnessDeps.verify.runner` cho test.

**Verdict parsing `parseCommandResult(stage, result, policy)`:** `notFound` (runner spawn ENOENT) → skip **chỉ khi** binary/tool của stage vắng mặt; `timedOut` → **fail** với reason `timeout` (không bao giờ skip); code 0 → pass (hoặc warn nếu warning heuristic); code !==0 → fail với issues parse regex `path:line: message`. Chỉ skip khi output khớp signature "tool package vắng mặt" gắn với stage (`npx canceled due to missing packages`, `command not found`, `is not recognized`) **và** tool của stage thực sự không resolve; **không** coi `Cannot find module`/`ENOENT` chung là missing tool — missing app module, missing test fixture, hoặc lỗi runtime đều phải fail. Test âm: test output `Cannot find module './app'` → fail; `ENOENT fixture.json` → fail; missing formatter binary → skip; timeout → fail.

`planVerification(cwd, changedFiles, config, policy): VerifyPlan` (detect project + resolve package scripts + exact commands, không chạy) và `runVerification(cwd, changedFiles, config, policy, runner, signal, plan?, approve?)`: nhận plan đã duyệt (hoặc tự gọi planVerification) → chạy tuần tự, short-circuit on fail → trả `VerifyVerdict`. **Fail-closed approval:** nếu `plan.commands.length > 0` và `plan.approved !== true` thì phải có `approve(plan)`; không có callback → throw `VerificationApprovalError`; callback trả false → trả verdict `skip` (pass=true, reason `approval denied`). `/dag`, scope loop và mọi caller ngoài harness phải truyền `approve` (gọi `tools.approve` với `plan.commands.map(c => c.display)`). Module không emit (caller emit).

**Tích hợp Harness:**
- State mới: `verifyAttempts`, `turnHadEdit`, `lastVerifyVerdict`, `verifyRolledBack` reset đầu `run` cùng `turnHadSuccessfulTool`.
- Detect edit trong `executeBatch`: sau execute mỗi call, nếu `meta.kind === 'edit'` set `turnHadEdit = true`.
- Hook sau `executeBatch` (giữ `flushGoalTurnProgress()` ngay sau executeBatch):
  - Nếu `verifyEnabled && turnHadEdit && !tools.effectiveReadOnly`:
    - `files = tools.files.turnChangedFiles()`; nếu >0:
      - `policy = this.deps?.verify?.policy ?? config.verify.policy`; `plan = planVerification(cwd, files, config.verify, policy)`; emit `verify_start` với `files` và `plan.commands.map(c => c.display)`.
      - Nếu `plan.commands.length > 0`: `allowed = await tools.approve({ tool: 'verify', input: { files, commands: plan.commands.map(c => c.display) }, signal })`. Prompt hiển thị **đúng command sẽ chạy** (kể cả package script detect được), không chỉ `verify.commands`.
      - deny → `verify_result` status `skip`, `lastVerifyVerdict = undefined`, không rollback, không fail.
      - allow (hoặc AST-only không có command) → `plan.approved = true`; runner = this.deps?.verify?.runner ?? createVerifyRunner(); verdict = await runVerification(cwd, files, config.verify, policy, runner, signal, plan); `verdict = redactVerdict(verdict, [config.apiKey])`; emit `verify_result`; `lastVerifyVerdict = verdict.severity === 'fail' ? verdict : undefined`.
      - fail → verifyAttempts++; `errorText = redactSecrets(formatVerifyErrors(verdict), [config.apiKey])`; push user message `[Verification failed (attempt X/Y)]` + errorText; save. Nếu attempts >= maxRepairRounds → rollbackTurn + throw `VerificationFailedError`.
      - pass/warn/skip → verifyAttempts=0; nếu warnings → push user message `redactSecrets(formatVerifyWarnings(verdict), [config.apiKey])`.
- **Định nghĩa pass:** `VerifyVerdict.pass = verdict.severity !== 'fail'` (warn và skip đều pass). Final rollback chỉ khi `lastVerifyVerdict?.severity === 'fail'`.
- Final verification trước khi model kết thúc turn: trước `if (!calls.length && !stepText.trim()) throw ...`:
  - nếu `!calls.length && !verifyRolledBack && lastVerifyVerdict?.severity === 'fail'` → rollbackTurn + throw `VerificationFailedError`.
- `rollbackTurn(reason)`: verifyRolledBack=true; backups = peekTurnBackups(); res = restoreBackups(backups, { force:false }); retained = backups filter conflict; replaceTurnBackups(retained); emit `verify_rollback`; push user message; save. Terminal rollback retain conflicting backups để checkpoint `finally` còn chứa conflict; `/revert` sau vẫn khôi phục conflict files.
- Giới hạn: `turnChangedFiles()` chỉ file tracked bởi WorkspaceFiles (write/edit/apply_patch). Bash/MCP writes không tracked → verify skip với reason `no tracked files`, `lastVerifyVerdict = undefined`; không rollback được bash-created files ở v0.4.

**Config/CLI/Event (5.1):** verify config/env/flags xem §5.4; events `verify_start`, `verify_result`, `verify_rollback` emit qua caller (không dùng `stopped`); CLI bắt `VerificationFailedError` → emit error + exit 1 (không phải 2); pair `notifyEvent` thêm nhánh verify (start chỉ status ngắn); subagent/workflow/network child `{ verify: { enabled: false } }`.

**Test/Acceptance (5.1):** `tests/harness-verify.test.mjs` (fake provider 2 turn, fake runner, model-gives-up rollback, approval deny skip, subagent disabled); acceptance: verify fail→repair→pass; model bỏ cuộc→rollback+exit 1; restore đúng byte; conflict retain.

### 5.2 patch safety

**`src/syntax.ts` (mới):** `validateSyntax(path, content): void`. Heuristic brace balance, skip strings/comments/template literals. JSON: `JSON.parse` trừ JSONC paths (`tsconfig*.json`, `jsconfig*.json`, `*.jsonc`, `.vscode/**/*.json`, `devcontainer.json`) và BOM strip. Unknown ext → skip. Giới hạn heuristic tài liệu hóa; test regex/template.

**`src/files.ts` patch safety:**
1. **Revision/stale check trong `applyPatch`:** `slotFor` sau đọc file tồn tại:
   ```ts
   if (slot.existed && this.revisions.get(file) !== this.digest(slot.before!)) {
     throw new Error('Read the current file before patching; it is unread or has changed.');
   }
   ```
   Áp dụng Add-to-existing, Update, Delete. Move destination không cần check (đã reject exists). Breaking change có chủ đích: `apply_patch` hiện không yêu cầu read trước (`tests/audit.test.mjs:35-64`, `tests/files.test.mjs:124-150`), nên:
   - cập nhật `src/tools.ts:616-619` mô tả tool: "Existing files must be read before patching."
   - cập nhật test hiện có thêm `await files.read(...)` trước applyPatch.
   - `previewDiff` không chạy revision gate.
2. **Uniqueness hunk:** `seekSubsequence` → throw ambiguous. Implement `findHunkIndex(lines, needle, startIdx)`:
   - needle length 0 → return startIdx (insertion-only, unchanged).
   - strategies exact/trimEnd/trim; hits count; nếu >1 throw `Patch hunk is ambiguous in <path> (matched N times); add more surrounding context.`; nếu 1 return; nếu 0 return -1.
   - `applyHunksToContent` dùng `findHunkIndex`; -1 throw existing message.
3. **AST/syntax validation trước commit:** trong `applyPatch` trước commit loop:
   ```ts
   for (const slot of order) {
     if (typeof slot.after === 'string') validateSyntax(slot.relPath, slot.after);
   }
   ```
   Error message: `Syntax validation failed for <path>: <details>`.

**Bảng lỗi apply_patch:**
| Tình huống | Thông điệp bắt buộc chứa |
|---|---|
| File thay đổi từ lần read cuối | `Read the current file before patching; it is unread or has changed.` |
| Hunk không tìm thấy | `Failed to match patch hunk in <path> near: "<line>"` |
| Hunk ambiguous | `Patch hunk is ambiguous in <path> (matched N times); add more surrounding context.` |
| Syntax/AST fail | `Syntax validation failed for <path>: <details>` |
| Delete file không tồn tại | `Cannot delete <path>: file does not exist.` |
| Update file không tồn tại | `Cannot update <path>: file does not exist.` |
| Move destination tồn tại | `Cannot move <path> to <dest>: destination exists.` |
| Hunk rỗng (insert/delete) | giữ nguyên hành vi (apply tại cursor) |

**Deletion tombstone (chống data loss):** `FileBackup` (`src/session.ts:8-15`) thêm `afterDeleted?: boolean` (optional, schema backward-compatible). `recordBackup` khi file bị xoá set `afterDeleted = true`, `afterHash = undefined`; `restoreBackups` xử lý:
- `afterDeleted && path hiện tồn tại` → **conflict** (user đã recreate/modify sau khi agent xoá), không overwrite, giữ file user.
- `afterDeleted && path vắng` → restore `content` nếu `existed` (khôi phục file gốc; `recordBackup` delete phải lưu `content` trước khi xoá), hoặc để vắng nếu file vốn không tồn tại.
- Legacy backup không có `afterDeleted` giữ logic `afterHash` hiện tại.
Test mới: agent xoá file → user recreate cùng path → rollback trả conflict, nội dung user còn nguyên; agent xoá file → rollback không conflict → file gốc được restore byte-exact.

**Public APIs WorkspaceFiles (cho rollback):**
```ts
peekTurnBackups(): FileBackup[]
turnChangedFiles(): string[]
hasTurnChanges(): boolean
clearTurnBackups(): void
replaceTurnBackups(backups: FileBackup[]): void
```
Giữ `takeTurnBackups`.

**Config/CLI/Event (5.2):** tool description `src/tools.ts:616-619` "Existing files must be read before patching."; `previewDiff` không chạy revision gate. Không có config/CLI mới.

**Test/Acceptance (5.2):** `tests/files.test.mjs` additions (stale reject, read-first compat `tests/files.test.mjs:124-150`/`tests/audit.test.mjs:35-64`, ambiguous reject, insertion-only apply, AST reject/false-positive guard, valid pass, deletion tombstone conflict, deletion rollback restore); acceptance: patch stale/ambiguous blocked; user-recreated deleted file không bị overwrite; read-first documented.

### 5.3 cost ledger + caps

**Module mới `src/cost.ts`.**

```ts
export interface ModelPrice { inputPer1M: number; outputPer1M: number; cachedInputPer1M?: number; cacheWritePer1M?: number; }
export interface TokenUsage { inputTokens: number; outputTokens: number; cachedInputTokens?: number; cacheWriteTokens?: number; }
export interface UsageSplit { inputTokens?: number; outputTokens?: number; totalTokens: number; }
export class CostLedger {
  static fromConfig(config: Config, model: string): CostLedger;
  priceFor(model: string): ModelPrice | undefined;
  computeCost(usage: TokenUsage, model: string): number;
  registerRun(budget: RunBudget, startedAtMs?: number): void;
  account(budget: RunBudget, usage: TokenUsage, model: string, onCost?: (cost: number) => void): void;
  hardCapExceeded(budget: RunBudget): { exceeded: boolean; reason: '' | 'cost cap' | 'time cap' };
  describe(budget: RunBudget, sessionCostUsd: number): string;
  warnIfInert(model: string, writer: (msg: string) => void): void;
}
export function createRunBudget(config: Config, shared?: RunBudget): RunBudget;
```

**Mở rộng `RunBudget` (`src/tools.ts:24`)** (backward compat, field mới optional):
```ts
export interface RunBudget {
  spent: number;        // tokens
  limit: number;        // token limit
  costSpent?: number;   // USD
  costLimit?: number;   // USD, 0 = unlimited
  startedAt?: number;   // epoch ms
  timeLimitMs?: number; // 0 = unlimited
  ledger?: CostLedger;
}
```
Tránh circular import: `cost.ts` chỉ `import type` từ `tools.ts`; `tools.ts` import value từ `cost.ts`.

**Harness (`src/harness.ts`):**
- `costLedger = CostLedger.fromConfig(config, config.model)` (fail-closed ngay tại đây).
- `run()`: budget mới/chia sẻ chỉ khởi tạo field còn thiếu (`costLimit ??=`, `timeLimitMs ??=`, `startedAt ??= Date.now()`, `ledger ??=`), không ghi đè shared.
- Nếu `maxTimeMs > 0`: `runSignal = AbortSignal.any([signal, AbortSignal.timeout(remaining)])` dùng cho model/tool calls; clamp timeout tool theo thời gian còn lại.
- `exhausted()` kiểm tra token + cost + time tại boundary; abort message rõ: `Run token budget reached...`, `Run cost budget reached ($x of $y).`, `Run time budget reached (a ms of b ms).`
- Usage parse (`src/harness.ts:574-580`) mở rộng cache:
  - `cacheRead = usage.inputTokenDetails?.cacheReadTokens ?? usage.cachedInputTokens ?? 0`
  - `cacheWrite = usage.inputTokenDetails?.cacheWriteTokens ?? 0`
  - `account(tokens, { inputTokens: input, outputTokens: output, cachedInputTokens: cacheRead, cacheWriteTokens: cacheWrite })`
- `account` gọi `budget.ledger?.account(...)` và cập nhật `session.costUsd`.
- Emit usage event mở rộng: `{ type:'usage'; input; output; total; cacheRead?; cacheWrite?; costUsd? }`.
- Checkpoint lưu `costUsdBefore: session.costUsd`. Revert **không giảm** `session.costUsd` (chi phí là sự kiện kế toán đã tiêu).
- v0.4: account mọi LLM call trong Harness (main turn, compaction, recap, btw) qua `accountUsage(usage, model)`; child cost aggregate vào parent session sau subagent/network.
- **Usage split cho auxiliary calls (additive, không phá API):** giữ `compactIfNeeded(): Promise<number>` cho backward compatibility; thêm `compactIfNeededDetailed(): Promise<{ tokens: number; usage?: UsageSplit }>` (no-op `{tokens:0}`), `run()` (`src/harness.ts:502`) dùng detailed; export cả hai + `UsageSplit` ở `src/index.ts:15`. `compactNow` trả `{ tokens, compactions, usage? }`; `recapSession` trả `{ text, tokens, usage? }`; `Harness.recap`/`askSideQuestion` trả `{ text, tokens, usage? }` (thêm field optional, không đổi shape cũ) và emit usage với `input/output/total` thật. Thêm `Harness.accountUsage(usage, model, budget?)`: `cost = this.costLedger.account(usage, model, budget)`, `session.costUsd += cost` (canonical), `budget.costSpent = session.costUsd` (mirror, gán chứ không cộng), emit `usage`. Các caller (`src/cli.ts` `/compact`, `/recap`, `/btw`, auto-compaction trong `run`) gọi `accountUsage` thay vì chỉ cộng `totalTokens`; auxiliary calls ngoài `run` dùng `budget` optional (không có vẫn ghi session/ledger). Nếu provider không trả split, fallback `input=0, output=total` và ghi `usageSplitMissing: true` trong test log.

- **Fail-closed khi thiếu giá:** `CostLedger.fromConfig` kiểm tra ngay lúc khởi tạo. Nếu `maxCostUsd > 0` và `priceFor(model)` undefined và không có `pricing.default` → throw `CostPricingError` trước khi `Harness.run`; CLI in hướng dẫn (`set pricing.default`, thêm model vào `pricing.models`, hoặc `--max-cost-usd 0`) và exit 2. Không bao giờ để cap inert. `pricing.models` dùng **exact key** bằng chuỗi model đã cấu hình (không wildcard, không normalize provider prefix); message lỗi liệt kê key đã cấu hình và model hiện tại. `warnIfInert` chỉ còn cho trường hợp `maxCostUsd=0` hoặc giá explicit 0 (không có cap để áp).
- `pricing.default` nếu có phải > 0; `pricing.models[model]` ưu tiên hơn default.

**Session schema (`src/session.ts:206-215`):**
- Thêm `costUsd: z.number().nonnegative().default(0)`.
- Checkpoint thêm `costUsdBefore: z.number().nonnegative().optional()`.
- Thêm `rejectedSignatures: z.record(z.string(), z.number().int().nonnegative()).optional()` (rejection loop).

**Chia sẻ budget:**
- **Canonical cost:** `session.costUsd` là nguồn sự thật cho session; `CostLedger.total` là nguồn cho một ledger; `budget.costSpent` chỉ là **mirror**. `accountUsage` tính `delta`, cộng `session.costUsd += delta`, rồi **gán** `budget.costSpent = session.costUsd` (không cộng thêm). Đầu mỗi run/resume: `budget.costSpent = session.costUsd`. Child dùng cùng ledger/budget; parent không cộng lại usage của child (tránh double count). Unit test không có session: `ledger.total` canonical, `budget.costSpent = ledger.total`.
- Subagent: giữ `this.registry.budget` (`src/subagents.ts:448,816,873`); đảm bảo field cost/time đi theo cùng object.
- Network: sửa `src/network.ts:514-519` copy `costSpent`, `costLimit`, `startedAt`, `timeLimitMs`, `ledger`; sync `parentBudget.costSpent` sau mỗi persona (`:563-565`).
- Workflow tool: đã truyền `reg.budget` (`src/cli.ts:894`). Slash `/workflow`: sửa `src/cli.ts:2833,2836,2848` và `/network` `:2921` để truyền budget vào `harness.run`, `runNetwork`, `runSubagents`.
- Child registries: set `policy` và `costLedger` sau construction (`src/subagents.ts`, `src/cli.ts:892`, workflow shell).

**Hiển thị:**
- `/cost` slash command mới (thêm vào `src/tui.ts:135-142` và descriptions `:144-199`; xử lý trong `src/cli.ts`). `/cost` và status bar cùng đọc `session.costUsd`; subagent/network cost aggregate vào parent session.
- `/status` (`src/cli.ts:1954-1975`) thêm dòng `Cost`, `Time cap`, `Token cap`.
- TUI status bar: `src/tui.ts:2674` (`tokenGroup`) hiển thị cost nếu >0; cộng dồn ở `:1032` (subagent progress) và `:1100` (usage).
**Test/Acceptance (5.3):** `tests/cost.test.mjs` (price input/output/cache, unknown model fail-closed, `pricing.default` fallback, zero-price warning, maxTimeMs abort, cost cap với usage mock, shared budget, overshoot); acceptance: unknown model + `maxCostUsd>0` → `CostPricingError`/exit 2; có `pricing.default` → cost cap thực sự trigger; cost/time cap dừng run với lý do rõ; `/cost` + status bar hiển thị; revert không giảm `session.costUsd`.

### 5.4 risk class + critical deny + rejection loop

**Module mới `src/risk.ts`.**

```ts
export type RiskClass = 'low' | 'medium' | 'high' | 'critical';
export function riskLevel(risk: RiskClass): number;
export function classifyBashRisk(command: string): RiskClass; // P12 mở rộng optional policy?: ExecutionPolicy
export function classifyMcpRisk(server: string, tool: string, remote: unknown, config: McpServerConfig): McpRiskAssessment;
export interface McpRiskAssessment { risk: RiskClass; net: boolean; reason: string }
```
`risk.ts` chỉ `import type { McpServerConfig }` từ `./mcp.js` để tránh cycle; P13 mới nối classifier vào MCP tool.

**Tool metadata (`src/tools.ts:20-37`):**
- `ToolDefinition`/`RegisteredTool` thêm `risk?: RiskClass`, `net?: boolean`, `trusted?: boolean`.
- Default risk: `risk ?? (readOnly ? 'low' : 'medium')`.
- `ToolRegistry` thêm `riskOf(name, input?)` ở P04; `policy: ExecutionPolicy` thêm ở P12 (v0.5); `costLedger: CostLedger` thêm ở P05.
- `execute()` mở rộng trả `Promise<{ text: string; denied?: boolean; loop?: boolean }>`:
  - `risk = riskOf(...)`.
  - nếu `risk === 'critical'`: trả `{ text: 'Permission denied: ... critical risk...', denied: true }` **không gọi approve**.
  - approval gate: `const trustedAuto = definition.trusted === true && risk === 'medium'; if (!trustedAuto && (!definition.readOnly || risk !== 'low') && !await this.approve({ ..., risk, trusted: definition.trusted })) return denied`.
  - `trustedAuto` nằm trong `ToolRegistry.execute` (không chỉ CLI closure) để unit test với approve luôn-false vẫn thấy trust auto-approve medium; critical đã deny ở trên.
  - network gate cho bash và MCP tool `net === true` theo execution policy (thêm ở P12/P13, v0.5).

**Bash classifier (`src/risk.ts`):**
- Wrapper peeling: `sudo`, `doas`, `env`, `time`, `nice`, `nohup`, `stdbuf`, `timeout`, `command`, `builtin`, `xargs`, `setsid`, và env assignment `KEY=val`.
- Segment splitter: tách `;`, `&&`, `||`, `|`, `&`, newline, `(`, `)`, `{`, `}`; quote-aware; nếu chứa shell escape thực (`$(...)`, backtick, `<(`, `>(`) ngoài single quote → `critical`.
- Bảng: safe read/build/test (`cat`, `ls`, `grep`, `rg`, `npm test`, `npm run build`, `node --test`, `pytest`, `go test`, `cargo test` không kèm network/install) → `low`; mutating fs/unknown → `medium`; network commands (`curl`, `wget`, `ssh`, `scp`, `rsync`, `nc`, `ftp`, `telnet`, `git push/fetch/clone/pull`, package install) → `high`; destructive thực sự: `rm` recursive+force vào `/`, `/*`, root/home/`..`; `dd`, `mkfs`, `shred`; `eval`, `source`, shell interpreter `-c` → `critical`; `rm -rf node_modules` không critical (high).
- Compound: overall risk = max risk của các segment; từ "test" trong `npm test`/`node --test` không nâng risk; `npm test && curl ...` → high. Test matrix: `npm test` low, `node --test tests/x.test.mjs` low, `npm run build` low, `npm install` high, `git push` high, `rm -rf /` critical.
- Redirection `>`/`<` nâng floor lên ít nhất `medium`.

**Approval interface (`src/tools.ts:17`):** mở rộng backward-compatible:
```ts
export interface Approval { tool: string; input: unknown; signal: AbortSignal; risk?: RiskClass; trusted?: boolean }
```
`ToolRegistry.execute` truyền `risk` và `trusted`; cập nhật mọi `approve` callback: CLI closure (`src/cli.ts:703`), pair `requestRemoteApproval` (thêm risk/trusted vào prompt), subagent wrappers (`src/subagents.ts:495,824` forward nguyên request), tests. Nếu `risk` undefined → coi `medium` để không auto-approve ngoài ý muốn. `tools.ts` chỉ `import type { RiskClass }` từ `./risk.js` (type-only, không cycle runtime).

**Workflow shell steps (cùng approval gate):** `WorkflowExecutionHooks.approveShell` mở rộng `(command, signal, risk?: RiskClass)`; `executeWorkflow` (`src/workflow.ts:633-636`) tính `classifyBashRisk(command)` và truyền risk. CLI closures (`src/cli.ts:884-885` headless, `:2829-2830` TUI) tạo `Approval` với `{ tool:'bash', input:{ command }, risk: risk ?? classifyBashRisk(command), trusted:false }`. Nếu step `shell` mà không có `approveShell` → throw `WorkflowApprovalError`, **không chạy** (breaking behavior có chủ đích). Red-team tests: `rm -rf /` → critical deny kể cả auto; `git push` → high prompt; `npm test` → low/auto; workflow shell critical không chạy khi approve callback trả false.

**CLI auto-mode/high-risk (sửa `src/cli.ts:704`, thuộc P04):**
```ts
const risk = request.risk ?? 'medium';
if (risk === 'critical') return false;
const autoAllowed = mode === 'auto' && risk !== 'high';
if (autoAllowed || (risk !== 'high' && sessionAllowedTools.has(request.tool))) return true;
if (risk !== 'high' && request.tool === 'bash'
    && isApprovedBashExact(request.input, sessionAllowedExactBashCommands)) return true;
if (risk !== 'high' && request.tool === 'bash'
    && isApprovedBashPrefix(request.input, sessionAllowedBashPrefixes)) return true;
```
High không bao giờ auto/always/exact/prefix; luôn đi tiếp tới prompt và yêu cầu gõ tên tool. Critical return false trước mọi nhánh.

**Exact-command allowlist (`src/approval.ts`):**
- Thêm `bashApprovalExact(input): string | undefined` (full trimmed command, collapse whitespace) và `isApprovedBashExact(input, set): boolean`.
- CLI thêm `sessionAllowedExactBashCommands: Set<string>`.
- Prompt bash thêm tuỳ chọn `e(xact)` bên cạnh `p(refix)`.
- Critical không bao giờ được allowlist.
- `mode === 'auto'` và `sessionAllowedTools` (`a`) **không** bypass critical/high; high luôn prompt và yêu cầu gõ tên tool.
- Exact-command allowlist chỉ áp dụng medium/low; high/critical không lưu vào exact set. `a` (tool-level) cũng không bypass high/critical.

**Rejection-loop guard:**
- Vì `run()` dừng ở denial đầu tiên (`src/harness.ts:609-610`), guard phải bền vững theo session, không đếm trong một batch.
- `session.rejectedSignatures?: Record<string, number>`.
- Trong approval closure/execute: khi một call bị deny, tăng count theo signature; nếu `>= 3`, đánh dấu `result.loop = true` (hoặc throw `RejectionLoopError`) và lưu session.
- Nếu cùng signature sau đó được approved/chạy thành công, reset count.
- `executeBatch` trả `{ denied, loop? }`; `run()` nếu `loop`, emit `{ type:'stopped', reason:'Tool rejected 3 times with identical arguments (...). Stopping run.' }`.
- Reset count khi signature khác.

**Config (`src/config.ts`) — v0.4 keys:**
```ts
verify: z.object({ ... }).default({ ... })   // chi tiết §5.1 và bảng precedence bên dưới
maxCostUsd: z.number().nonnegative().default(0)
maxTimeMs: z.number().int().min(0).default(0)
pricing: z.record(z.string(), z.object({ inputPer1M, outputPer1M, cachedInputPer1M?, cacheWritePer1M? })).optional()
allowNet: z.boolean().default(false)            // policy wiring ở P12 (v0.5)
allowNetHosts: z.array(z.string()).default([])
```
Env mapping v0.4: `GOON_VERIFY_ENABLED`, `GOON_VERIFY_TIMEOUT_MS`, `GOON_VERIFY_MAX_REPAIR_ROUNDS`, `GOON_VERIFY_CMDS` (JSON array hoặc phân tách bằng dấu phẩy), `GOON_VERIFY_REQUIRE_TESTS`, `GOON_VERIFY_REQUIRE_LINT`, `GOON_VERIFY_REQUIRE_TYPECHECK`, `GOON_MAX_COST_USD`, `GOON_MAX_TIME_MS`, `GOON_PRICING_JSON` (optional). Precedence: overrides > env > stored; deep merge cho `verify` (`{ ...validated.verify, ...env.verify, ...overrides.verify }`).

**CLI flags v0.4:** `--no-verify`, `--verify-cmd` (repeatable, `multiple:true` trong `parseArgs`), `--max-cost-usd`, `--max-time-ms`. Apply sau `loadConfig`, không sửa file config. Help `src/help.ts:20-42` thêm 2-4 dòng.

**Event outputs:** TUI `src/tui.ts:1032-1103` thêm nhánh verify (start/result/rollback); CLI plain `src/cli.ts:663-695` thêm stderr messages; JSON mode tự in event; pair `src/pair.ts:1205-1227` notifyEvent thêm nhánh verify (start chỉ status ngắn). `denied` chỉ set khi event `stopped`; `verify_rollback` không set `denied`, exit code 1 không phải 2.

**Config/CLI/event cost-risk:** usage event mở rộng optional fields; backward compat.

**Test/Acceptance (5.4):** `tests/risk.test.mjs` + `tests/approval.test.mjs` (critical deny với approve luôn-false, rejection loop 3 lần, classifier wrapper/segment, exact vs prefix allowlist, redteam quotes); acceptance: critical không chạy kể cả auto/session allowlist; high luôn prompt; rejection loop dừng sau 3 lần identical.

### Test v0.4
- `tests/verify.test.mjs`: detectProjects (manifest kinds + workspace boundary parent manifest → unknown); verdict parsing (exit 0/1, timeout → fail, notFound + missing-tool signature → skip, generic `Cannot find module`/`ENOENT` → fail); local binary resolution (win32 smoke); path normalization Windows; read-only/plan không gọi runner; approval deny → skip (pass=true); `runVerification` không có approve + có command → `VerificationApprovalError`; offline args (rust `--offline`, go env).
- `tests/files.test.mjs` additions: stale reject; read-first compatibility (update `tests/files.test.mjs:124-150`, `tests/audit.test.mjs:35-64`); ambiguous reject; insertion-only apply; AST reject; AST false-positive guard (regex/template, JSONC, BOM); valid pass.
- `tests/harness-verify.test.mjs`: fake provider 2 turn (write bad.ts → verify fail injected → edit); fake runner (code 1 nếu BROKEN else 0); events verify_result fail rồi pass; verify_start; model-gives-up → verify_rollback + restore + reject `VerificationFailedError` + exit 1 (không phải 2); approval deny → skip no rollback + `lastVerifyVerdict` cleared; secret in runner output → redacted in session/event; subagent `{ enabled:false }` không gọi runner.
- Rollback test: new file terminal → deleted, checkpoint files rỗng; conflict retention → user file giữ, conflicts báo, checkpoint còn backup; restore đúng byte (Buffer/sha256); Windows path posix keys.
- `tests/cost.test.mjs` (mới): price input/output/cache; unknown model fail-closed; `pricing.default` fallback; zero price warning; cap abort harness với maxTimeMs nhỏ; cost cap với usage mock; subagent/network shared budget; time cap overshoot ghi nhận; compaction/recap/btw account đúng split (không coi toàn bộ là output).
- Risk tests: `ToolRegistry` auto mode critical returns denied, approve không gọi; rejection loop 3 denied identical calls across runs → loop flag/stopReason; risk classifier wrapper peeling; MCP trust tests vẫn pass (`tests/mcp.test.mjs:155-183`, `:74-79`).
- Exact allowlist: prefix vs exact tests; redteam quotes/backticks, `find -exec`, interpreters, `awk` system, `sed e`, wrapper nested.

**Acceptance v0.4 (checklist):**
- [ ] verify fail → model thấy lỗi → sửa → pass (integration).
- [ ] model bỏ cuộc sau fail → verify_rollback, file restore, run reject; exit code 1 không phải 2.
- [ ] rollback restore đúng byte; conflict retain trong checkpoint.
- [ ] patch stale/ambiguous blocked; read-first trong tool description; test cũ cập nhật.
- [ ] read-only/plan không chạy verify; approval deny → skip không rollback.
- [ ] critical không chạy kể cả auto/session allowlist; rejection loop dừng sau 3.
- [ ] cost/time cap dừng với lý do rõ; `/cost` + status bar hiển thị cost.
- [ ] typecheck/lint/build xanh; test không regression.

## 6. v0.5 - Observability và vận hành

### 6.1 Event log/replay

**Module mới `src/eventlog.ts`.**

`EVENT_LOG_VERSION = 1`; `EventEnvelope { v:1; seq:number; at:string; sessionId:string; type:string; event:unknown }`; `eventLogPath(dataDir, sessionId)` = `join(dataDir, 'sessions', sessionId + '.events.jsonl')`; `openEventLog(path)`; `readEventLog(path)` → `{ envelopes, truncated }`; `normalizeEventEnvelope(envelope, ctx)`; `EventLogWriter` (append/close/switchSession); `StreamRedactor`.

- **Một đường emit duy nhất:** refactor `src/cli.ts` `emit` thành `publishEvent(event, { logOnly? })`: (1) durable append + fsync trước render; (2) render. `renderEvent` là phần thân cũ mở rộng cho `session`/`done`/`error`. Harness nhận `publishEvent` làm `emit`. Thay 3 stdout writes ở `src/cli.ts:1069,1112,1120` bằng `publishEvent`. Thêm `publishEvent({ type:'user', text:input }, { logOnly:true })` trước `harness.run` `:1107`. `tool_stream` vẫn log trước render (golden thấy stream) nhưng `--json` không in (compat).
- **LoggedEvent** = `HarnessEvent | SubagentEvent | {type:'session'|'done'|'error'|'user'}`.
- **seq (chống torn line >64 KiB):** `readLastSeq(path)` ưu tiên sidecar `path + '.seq'` (số nguyên, atomic write sau mỗi append); nếu sidecar thiếu/hỏng/lệch → tail scan. Tail scan bắt đầu 64 KiB; nếu buffer không có newline hoặc dòng đầu bị rách, nhân đôi cửa sổ tới khi gặp newline hoặc tới `min(fileSize, 1 MiB)`; nếu một dòng >1 MiB thì đọc toàn file (bounded bởi kích thước file) rồi parse; không được restart seq về 1 khi file còn seq cũ. Sidecar ghi bằng temp + rename, best-effort; tail scan là nguồn sự thật. Session resume cùng id → cùng file → seq tiếp tục. TUI session switch → `switchSession(newId)`.
- **append:** `openSync(path,'a',0o600)`; POSIX `fchmodSync(fd,0o600)`; writeSync + fsyncSync; lỗi write → callback `onError` → stderr, run tiếp tục. Đóng fd trong `close()`, flush carry redactor trước khi đóng.
- **Session delete:** `SessionStore.deleteSession` unlink thêm `${id}.events.jsonl`, `${id}.events.jsonl.seq` và `.tmp` (`src/session.ts:452-461`).
- **Config/CLI:** `eventLog: z.boolean().default(false)`; env `GOON_EVENT_LOG`; flag `--event-log` / `--no-event-log`; path canonical `sessions/<id>.events.jsonl`.
- **Replay:** dispatch `replay` sớm sau `sessions` (`src/cli.ts:435-439`), trước auto setup (`:531`) và resolveRuntime (`:542`). Không cần model/API key.
  - `goon replay <session-id-or-prefix|path.jsonl> [--json] [--envelopes] [--rebuild-session]`
  - Target: path tồn tại → dùng; session id/prefix → resolve qua SessionStore.list, canonical path; không tìm thấy → exit 1 stderr rõ.
  - Output: mặc định và `--json` in raw event (`envelope.event`) mỗi dòng; `--envelopes` in cả envelope.
  - `readEventLog`: parse từng dòng; dòng cuối không parse → `truncated:true`, trả envelope trước; lỗi parse giữa file → throw; kiểm tra v===1, seq tăng dần; gap warning stderr, không fail.
  - `--rebuild-session`: lấy `session` đầu, `user` events, `done` cuối (text, denied), `error` nếu có; ghi `<id>.replayed.json` cạnh session gốc (không overwrite `.json`), mode 0600, atomicJson. Nếu crash trước done → chỉ user message (partial). Không reconstruct tool messages/checkpoints/todos (v0.6).
  - Idempotent: chạy hai lần cùng output (trừ timestamp file).

**Redaction & an toàn event log:** dùng `redactSecrets`/`redactObject`/`isSensitivePath` từ `src/redact.ts` (cùng helper với memory/RAG). Không regex trên JSON string; `EventLogWriter` redact từng string field trước stringify. Patterns theo thứ tự **bearer trước key-value**: literal secret; env `*_API_KEY|*_TOKEN|*_SECRET|*_PASSWORD`; bearer `/\b(bearer)\s+[A-Za-z0-9._~+/=-]+/gi` → `Bearer [REDACTED]`; key-value `/((?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|token|secret|password|authorization)\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s"',}]+)/gi` → `$1[REDACTED]`. `redactObject` redact toàn bộ value khi key khớp `/^(api[_-]?key|token|secret|password|authorization|access[_-]?token|refresh[_-]?token|client[_-]?secret)$/i`, rồi redact string con. Không log nội dung file nhạy cảm (`.env*`, `*.pem`, `*.key`, `id_rsa*`, `.npmrc`, `.netrc`, `.git-credentials`, `secrets/**`, `.ssh/**`, `.aws/**`). Stream delta secret/bearer bị chia qua nhiều chunk → `StreamRedactor` giữ tail `max(256, longestSecret - 1)` ký tự, redact rồi emit phần trước tail; `flushAll()` ở done/error/close. Crash giữa turn có thể mất tối đa tail (trade-off bảo mật). File mode 0600; POSIX fchmod; Windows không có mode bit, ghi docs.

### 6.2 Stub provider + golden JSONL

**`src/stub-provider.ts` + `src/stub-fixtures.ts`.**

Stub implement `LanguageModelV3` đầy đủ: `specificationVersion:'v3'`, `provider:'goon-stub'`, `modelId`, `supportedUrls:{}`, `doGenerate`, `doStream`. `doGenerate` trả content/finishReason/usage nested/warnings. `doStream` phát stream-start, reasoning-*, tool-input-*, tool-call, text-*, finish với cùng usage nested.

Fixture:
```ts
export interface StubTurn {
  match?: string; reasoning?: string[]; text?: string[];
  toolCalls?: Array<{ id: string; name: string; input: unknown }>;
  usage?: { inputTokens?: number; outputTokens?: number; totalTokens?: number };
  error?: { message: string; statusCode?: number; retryable?: boolean };
}
export interface StubFixture { v: 1; model?: string; input?: string; turns: StubTurn[]; auxiliary?: StubTurn; }
```
- **Chọn turn (deterministic):**
  1. Nếu last user text khớp `match` (substring) của một turn chưa dùng → turn chưa dùng đầu tiên đó.
  2. Nếu không có match nhưng `fixture.input` khớp last user text → turn chưa dùng đầu tiên (kể cả không có `match`).
  3. Nếu chỉ còn đúng một turn chưa dùng → dùng turn đó.
  4. Ngược lại → `auxiliary`. `match` là optional; fixture không có `match` vẫn chạy được qua bước 2/3.
- Không khớp (compaction/recap/side question) → `auxiliary`; không có → text cố định `"[stub] auxiliary response"`. Auxiliary không tiêu thụ turn chính.
- Hết turn chính → `"[stub] script exhausted"` finish stop, không throw (golden); fixture lỗi → throw `APICallError` status 400 `isRetryable:false` (import `APICallError` từ `ai`, không import `@ai-sdk/provider`), nằm trong `isNonRetryableApiError` (`src/harness.ts:53`), không retry.
- **Golden/replay chỉ chạy main session** (không subagent/workflow) để turn counter deterministic. `createStubModel(fixture, cursor = { index: 0 })` tạo cursor **mới** mỗi model instance (không dùng module-level default). CLI tạo một `stubCursor` per process và truyền cho mọi `createModel`/child harness; golden test có child harness phải truyền shared explicit `StubCursor` (object `{ index: number }`), nếu không turn counter sẽ order-dependent.
- Default fixture là hằng TS trong `src/stub-fixtures.ts` (không file JSON trong src; tsc không copy). User fixture qua `--stub-fixture`/`GOON_STUB_FIXTURE` (JSON, zod validate, cap 1 MiB).
- Không gọi fetch, không đọc env credential, không mở socket.

Config: `stub: z.boolean().default(false)`, `stubFixture: z.string().optional()`. Env: `GOON_STUB`, `GOON_STUB_FIXTURE`. `resolveRuntime`: luôn `protectSecretEnv(config.keyEnv)`; nếu stub trả `{...config, model: config.model ?? 'stub-model', baseUrl:'stub://local', apiKey:'goon-stub-no-key', cwd, dataDir}`. `createModel`: nhánh stub đầu tiên. `buildProviderOptions` trả undefined nếu stub. Auto setup `src/cli.ts:531` thêm `!config.stub`. Doctor: nếu stub báo `stub:true`, bỏ discoverModels/network.

`tests/helpers.mjs`: thêm `normalizeEvent(event, ctx)` đệ quy (xử lý path/sessionId/cwd/volatile fields/at/createdAt/updatedAt/startedAt/finishedAt/durationMs; renumber seq ở envelope; normalize session/subagent id), `normalizeStream(events, ctx)`, `stubCli(f, args)` (không thêm `--model`/`--base-url`/`GOON_API_KEY`).

Golden: `tests/golden.test.mjs` + `tests/golden/*.jsonl`; `GOON_UPDATE_GOLDEN=1` chỉ local. Test: stub-text, stub-tool-call, stub-error, stub-usage, replay idempotent, replay rebuild.

### 6.3 Execution policy (network/wrapper)

**Module mới `src/policy.ts`.**

```ts
export interface ExecutionPolicy {
  network: { mode: 'deny' | 'allow' | 'allowlist'; hosts: ReadonlySet<string> };
  limits: { defaultToolTimeoutMs: number; maxToolTimeoutMs: number; maxOutputBytes: number; hardOutputBytes: number; mcpMaxOutputBytes: number };
  allowNetForServer(serverConfig: McpServerConfig): boolean;
}
export function resolveExecutionPolicy(config: Config, flags: Record<string, unknown>): ExecutionPolicy;
export function extractNetworkHost(command: string): string | undefined;
```

- Network default-deny: mode `deny` chặn mọi bash command phân loại network trước approval.
- `allowNetHosts` exact host match (không suffix/wildcard), lowercase + strip trailing dot; parse comma-separated hoặc repeatable. Allowlist mode: không xác định được host → deny (fail closed).
- `--allow-net` → mode `allow`; `--allow-net-hosts=...` hoặc config `allowNetHosts` → mode `allowlist`.
- MCP: `src/mcp.ts:821-823` không dùng `trust` để set `readOnly`; `readOnly` chỉ từ `readOnlyTools`. Thêm `net` flag: `net = config.net === true || remote.annotations?.openWorldHint === true` (annotations chỉ escalate, không downgrade). `allowNet = config.allowNet === true`. Nếu `net === true` và không allowed (global hoặc server allowNet), tool bị chặn bởi policy trước approval. HTTP transport không tự bị chặn; gate áp dụng metadata `net`/openWorld.
- **McpToolSpec (`src/mcp.ts:603-611`):** thêm `risk?: RiskClass; net?: boolean; trusted?: boolean; annotations?: { readOnlyHint?: boolean; destructiveHint?: boolean; openWorldHint?: boolean }`; lưu annotations lúc discovery; `attach` (`src/mcp.ts:889-895`) truyền `risk`, `trusted`, `net` vào `ToolRegistry.register` — không được tính `classifyMcpRisk` rồi bỏ kết quả. Test assert spec/registered tool giữ đủ field.
- **Annotation → risk mapping (bắt buộc, test):**
  - `destructiveHint === true` → `risk='high'`, `net=false`.
  - `openWorldHint === true` → `risk='high'`, `net=true` (kể cả destructive).
  - Chỉ `readOnlyHint === true` → floor `medium` (annotation là untrusted, **không** hạ xuống low).
  - Không annotation → `medium`.
  - `readOnlyTools.includes(tool)` (user allowlist) → `low`.
  - `config.trust === true` → base risk tối đa `medium` (không nâng risk); annotation `destructive`/`openWorld` vẫn `high` và thắng trust (trust không downgrade annotation).
  - `classifyMcpRisk` trả `{risk, net, reason}`; MCP tool dùng `risk` này trong `ToolRegistry.execute`.
- **Breaking behavior có chủ đích:** `trust` không còn expose MCP tools trong `--read-only`; chỉ `readOnlyTools` làm điều đó. Cập nhật `tests/mcp.test.mjs:155-183`, README/CHANGELOG; test mới assert trusted server + `--read-only` không list tool.
- `trust` auto-approve medium/low trong `ToolRegistry.execute` (`trustedAuto`), không bypass critical/high.
- Risk floor MCP: medium trừ khi user `readOnlyTools` (low) hoặc trust auto-approve; annotations `destructive/openWorld` nâng high, không dùng annotation hạ xuống low.

**Centralize limits:** `src/process.ts` export constants `DEFAULT_OUTPUT_LIMIT_BYTES`, `DEFAULT_HARD_OUTPUT_LIMIT_BYTES`, `DEFAULT_COMMAND_TIMEOUT_MS`, `MAX_COMMAND_TIMEOUT_MS`; `runCommand` options thêm `outputLimitBytes?`, `hardOutputBytes?`. `ExecutionPolicy.limits` là nguồn duy nhất cho bash tool, workflow shell, MCP timeout/output. `limitText` dùng chars; MCP output theo bytes → thêm `limitTextBytes` hoặc convert `Math.floor(bytes/4)` bảo thủ. Ghi chú gap: MCP và một số đường bypass `runCommand` (`src/mcp.ts:832-850`, TUI powershell `src/tui.ts:1572`); policy vẫn áp dụng timeout/output qua SDK; process-group kill chỉ cho `runCommand`.

Config keys: `allowNet`, `allowNetHosts` (đã có từ v0.4; policy wiring ở P12/v0.5). CLI flags: `--allow-net`, `--allow-net-hosts`. Env: `GOON_ALLOW_NET`, `GOON_ALLOW_NET_HOSTS`. MCP config (`src/mcp.ts` schema): thêm `servers.<name>.net?: boolean`, `servers.<name>.allowNet?: boolean`; giữ `trust` và `readOnlyTools`.

### 6.4 CI đa nền tảng

Sửa `.github/workflows/release.yml`: matrix `os: [ubuntu-latest, windows-latest, macos-latest]`, `node: ['22.16.0','24']`, `fail-fast:false`. `npm ci --ignore-scripts --no-audit --no-fund`; typecheck, lint, test. `package.json` test script quote glob: `"test": "npm run build && node --test \"tests/*.test.mjs\""`. umask guard chỉ non-Windows:
```yaml
- name: Restrictive umask
  if: runner.os != 'Windows'
  run: |
    (umask 027; node --test "tests/*.test.mjs")
    (umask 007; node --test "tests/*.test.mjs")
```
Windows gates (land trước khi bật matrix Windows):
- `tests/files.test.mjs:14,22` mode POSIX → skip trên win32.
- `tests/files.test.mjs:25-48` umask → skip win32.
- `tests/publish.test.mjs:34-43` shim `gh` POSIX → skip win32 (lý do đúng: shim `gh`, không phải `execFile('npm')`).
- `tests/tui.test.mjs:56`, `tests/audit.test.mjs:438` đã skip win32.
- `.gitattributes` mới (LF normalization + binary excludes); chạy `git add --renormalize .` trong PR riêng.
- Termux: `scripts/smoke-termux.sh` chạy thủ công: `npm ci`, `npm run build`, `node --test tests/eventlog.test.mjs tests/stub-provider.test.mjs tests/golden.test.mjs`. Không block CI; báo cáo rõ nếu chưa chạy được.

## 7. v0.6 - Hiểu repo và multi-agent

### 7.1 Memory/RAG lexical

**`src/rag.ts` (mới):** `Embedder`, `HashEmbedder` (FNV-1a, dim 384, deterministic, offline; tokenize lowercase `[a-z0-9_]+`, bỏ token <2), `Chunk`, `RetrievedChunk`, `LexicalIndex` (in-memory, linear scan, cosine, top-k, threshold default 0, LRU eviction theo lastAccess, capacity 5000), `chunkFile` (per-function/class TS/JS/Go/Python bằng regex + fallback fixed 40 dòng overlap 8; soft 1500 chars, hard 4000 chars), `indexRepo` (an toàn: lstat, skip symlink, realpath + isWithinRoot, skip >1 MiB/binary/non-text, redactSecrets trước chunk), `parseGoonignore`/`isIgnored` (subset gitignore: comment, blank, negation `!`, trailing `/`, `*`,`?`,`**/`, path segment; không full gitignore), `redactSecrets`, `isSensitivePath` (skip `.env`, `*.pem`, `*.key`, ...). Cold-start: default skip dirs (`.git`, `node_modules`, `vendor`, `dist`, `build`, `out`, `.next`, `.cache`, `.turbo`, `coverage`, `.venv`, `venv`, `__pycache__`, `.idea`, `.vscode`, `target`, `bin`, `obj`). Persistence `rag.json` lưu `{version, cwd, indexedAt, files:{path:{mtimeMs,size,chunks}}}`; vector recompute khi load; `ensureRepoIndexed()` so mtime/size, reindex changed, dedup in-flight promise; không index lại mỗi `Harness.run()`. `Embedder` seam: `embed(texts, signal?)`; `LexicalIndex` nhận `embedder?`. Không gọi semantic; doc/test dùng "lexical (hashed term-frequency)".

**`src/memory.ts` (mới):** 6 loại memory:
- Working: per-turn RAM (task, state, notes).
- Conversation: per-session, view đọc `SessionStore.messages` (đã có), không retrieval lại.
- Exec: per-task JSON rolling 50, chỉ lesson/compact, không chèn raw.
- Repo: per-project JSON cache + tree.
- Knowledge: per-project `LexicalIndex` + `rag.json`.
- Preference: per-user `configDir/preferences.json` (key/value), **không ghi đè** `memories.json`.

`MemoryStore.open({ dataDir, projectId, capacity })`; projectId = `sha256(cwd).slice(0,16)`. Interface rút gọn:
```ts
export type MemoryKind = 'working' | 'conversation' | 'exec' | 'repo' | 'knowledge' | 'preference';
export interface MemoryPromptContext { rag: RetrievedChunk[]; repoTree?: string; workingNotes?: string; }
export class MemoryStore {
  static open(opts: { dataDir: string; projectId: string; capacity?: number; configDir?: string }): Promise<MemoryStore>;
  readonly working: WorkingMemory; readonly exec: ExecMemory; readonly repo: RepoMemory;
  readonly knowledge: KnowledgeMemory; readonly preference: PreferenceMemory;
  handleEvent(event: MemoryEvent): Promise<void>;
  retrieve(query: string, budgetTokens: number): Promise<MemoryPromptContext>;
  ensureRepoIndexed(): Promise<void>;
  persist(): Promise<void>;
  close(): Promise<void>;
}
```
Sub-store APIs: `WorkingMemory` (setTask/setState/addNote/snapshot/clear), `ExecMemory` (append/recent/clear, rolling 50), `RepoMemory` (refresh/invalidate/treeSummary), `KnowledgeMemory` (ensureRepoIndexed/reindex/addInsight/retrieve/size), `PreferenceMemory` (get/set/all/remove). `MemoryEvent` gồm `task_started|state_changed|task_completed|assistant_message|tool_result|patch_applied|verify_result|user_preference`. `handleEvent(event)` là điểm vào duy nhất; redact trước lưu; serialize writes qua promise chain + atomicJson. `retrieve(query, budgetTokens)` trả working notes + repo tree + knowledge chunks + preference liên quan (không conversation/exec raw). `ensureRepoIndexed()` idempotent, in-flight dedup, staleness mtime/size; gọi từ `Harness.run` một lần/session chỉ khi `ragEnabled`. CLI lifecycle: nếu `memoryEnabled`, mở `MemoryStore` sau khi có runtime/store, truyền vào `new Harness(runtime, store, reg, emit, model, { memory })`, đóng trong `finally`; nếu false, không mở và không truyền (trừ khi user gõ `/memory`, command tự mở on-demand). `/memory rag ...` dùng cùng instance. Subagents không nhận MemoryStore (tránh double-write); parent ghi exec từ báo cáo subagent.

**Wiring `handleEvent` (bắt buộc, nếu không memory sẽ miss event):**
- `Harness.run` đầu run → `task_started`; cuối run (kể cả lỗi) → `task_completed` với outcome.
- Trong `executeBatch` sau mỗi tool result → `tool_result` (chỉ summary/lesson, redacted; không raw output).
- Sau khi edit tool commit thành công (`meta.kind==='edit'`) → `patch_applied` (path + bytes changed, không nội dung file).
- Sau mỗi `verify_result` → `verify_result` (status, stage, issue count).
- Sau assistant final message → `assistant_message` (text đã redact, cắt ngắn).
- `/memory pref set` → `user_preference`.
- Thứ tự: emit event ra UI trước, sau đó `void memory.handleEvent(...)` trong cùng promise chain; lỗi memory → `memory_error`, không làm fail run.

**Redaction dùng chung (`src/redact.ts`):** `redactSecrets(text, secrets)`, `redactObject(value, secrets)` và `isSensitivePath(path)`. Secret patterns theo thứ tự **bearer trước key-value**: literal `config.apiKey`/`keyEnv` value, env vars khớp `/(_API_KEY|_TOKEN|_SECRET|_PASSWORD)$/i`, bearer `/\b(bearer)\s+[A-Za-z0-9._~+/=-]+/gi`, key-value `/((?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|token|secret|password|authorization)\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s"',}]+)/gi`. `redactObject` redact toàn bộ value khi key khớp `/^(api[_-]?key|token|secret|password|authorization|access[_-]?token|refresh[_-]?token|client[_-]?secret)$/i`. Sensitive paths: `.env`, `.env.*` (trừ `.env.example`), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`, `id_ed25519*`, `.npmrc`, `.netrc`, `.git-credentials`, `credentials*`, `secrets/**`, `.ssh/**`, `.aws/**`, `.config/gcloud/**`. Event log, RAG index, memory persistence và tool output summary đều dùng helper này. Test: bearer token bị chia chunk, JSON `{"token":"..."}`, `Authorization: Bearer <token>`.

CLI: mở rộng `/memories` → `/memory` (alias); thêm `/memory rag index|stats|search`, `/memory budget`, `/memory pref set|get|rm`. Giữ `/memories list|add|rm|clear`. TUI/help thêm `/memory`, `/dag`.

Config keys: `memoryEnabled` default false (opt-in v0.6 để giữ backward compatibility; bật sau benchmark); `ragEnabled` default false (opt-in, tránh đổi startup latency/hành vi Termux); `ragTopK` default 10; `ragCapacity` default 5000; `ragThreshold` default 0; `ragIndexMaxFiles` default 2000; `ragIndexMaxFileBytes` default 1_048_576; `contextReserveRatio` default 0.15; `contextConversationRatio` default 0.45; `contextFilesRatio` default 0.25. Env: `GOON_MEMORY_ENABLED`, `GOON_RAG_ENABLED`, `GOON_RAG_TOP_K`, `GOON_RAG_CAPACITY`, `GOON_MULTI_AGENT_MODE`, `GOON_MULTI_AGENT_REWORK_CAP`, `GOON_MULTI_AGENT_MAX_CONCURRENCY`.

Events (thêm vào `HarnessEvent`):
```ts
| { type: 'memory_indexed'; files: number; chunks: number; durationMs: number }
| { type: 'memory_retrieved'; chunks: number; tokens: number }
| { type: 'memory_error'; message: string }
| { type: 'context_budget'; inputBudget: number; system: number; conversation: number; files: number; truncated: boolean; reason?: string }
```

### 7.2 Context budget

**`src/context-budget.ts` (mới).**

Types/helpers: `ContextBudgetInput`, `ContextBudgetResult`, `ContextBudgetError`, `allocateContextBudget`, `prepareContextBudget`, `pruneToolResults`, `pruneConversation`, `truncateSystemToBudget`.

`contextWindow` tổng; `maxOutputTokens` trừ trước. `inputBudget = max(1024, contextWindow - maxOutputTokens)`.

`allocateContextBudget(config, system)`:
1. `reserve = floor(inputBudget * config.contextReserveRatio)`.
2. `baseSystemTokens = estimatedTokens([], system)`.
3. `systemCap = floor(inputBudget * 0.35)`, `systemFloor = floor(inputBudget * 0.15)`.
4. `systemBudget = min(max(baseSystemTokens, systemFloor), systemCap)`. Nếu baseSystemTokens > systemCap: truncate phần `Workspace guidance (AGENTS.md):` và `Persistent user memories:` trước, giữ identity/rules; emit `context_budget: system truncated`.
   - Min context 8192/512: inputBudget=7680, systemCap=2688 > base system ~1170 token → không cắt base.
5. `remaining = inputBudget - reserve - systemBudget`.
6. `conversationRatio = config.contextConversationRatio`, `filesRatio = config.contextFilesRatio`; `conversation = floor(remaining * conversationRatio / (conversationRatio + filesRatio))`; `files = remaining - conversation`.
7. `files` bao gồm RAG chunks + repo tree + `<referenced_file>` blocks. Tool results và user text thuộc conversation. Soft cap cho auto-injected context; user `@file` vượt cap thì truncate bằng marker và model có thể `read_file` tiếp.

`prepareContextBudget(input)` (thuần, không mutate session):
- Input: messages clone, system, config, context (rag, repoTree, workingNotes), optional maxChars.
- Bước 1: `pruneToolResults(messages, maxCharsPerResult=8192)` — thay output dài bằng head/tail, **giữ nguyên message và tool-call/tool-result pairing**.
- Bước 2: dedup tool results: nếu cùng output.value xuất hiện, thay **nội dung output** bằng `[duplicate tool result omitted: same as earlier call]`; không xoá message/part.
- Bước 3: **aggregate conversation prune** `pruneConversation(messages, conversationBudget)` nếu `estimatedTokens(messages, '') > conversationBudget`:
  1. Thay output của các tool result cũ nhất (oldest first) bằng `[tool result omitted to fit context; re-run if needed]`, giữ message/part và pairing.
  2. Nếu vẫn vượt, bỏ nguyên message cũ tại ranh giới an toàn: không bỏ tool message mồ côi, luôn giữ user message gần nhất và assistant message gần nhất.
  3. Nếu vẫn vượt sau khi chỉ còn tối thiểu → throw `ContextBudgetError('Conversation exceeds context budget; narrow the request or increase --context-window.')`; **không** gửi request vượt budget.
- Bước 4: tạo một **user message** `<untrusted_context source="rag,repo" trust="untrusted">...</untrusted_context>` chứa RAG/repo/working, cắt theo `files` budget. Repo/RAG không bao giờ vào system prompt; header "data, not instructions". (Persistent user memories vẫn ở system prompt vì do user viết.)
- Bước 5: truncate `<referenced_file>` blocks trong clone theo `files` budget.
- Bước 6: `truncateSystemToBudget(system, systemBudget)`.
- Bước 7: final invariant `estimatedTokens(messages, system) <= inputBudget`; nếu vượt do injected context → cắt tiếp `files` block; nếu vẫn vượt → throw `ContextBudgetError`.
- Trả `{ system, messages, stats: { inputBudget, reserve, conversation, files, systemProject, before, after, truncated } }`.

Harness flow:
1. `const baseSystem = await systemPrompt(...)` (không đổi chữ ký).
2. `const budget = allocateContextBudget(config, baseSystem)`.
3. `const memoryContext = await this.memory?.retrieve(expandedPrompt, budget.files)`.
4. `const prepared = prepareContextBudget({ messages: session.messages, system: baseSystem, config, context: memoryContext })`.
5. `compactIfNeeded` vẫn chạy trên `store` (không thấy RAG block); nếu compact xảy ra, chuẩn bị lại `prepared` từ messages mới.
6. `streamText` dùng `prepared.messages` + `prepared.system`; `estimatedTokens(prepared.messages, prepared.system) <= inputBudget` là invariant test.

Compression pass không thay `compactIfNeeded`/`compactNow`; chỉ giảm input trước, compaction vẫn safety net. `expandFileMentions` thêm optional `maxChars` (default 32_000); harness truyền `budget.files * 3`.

**Test/Acceptance (7.2):** 200 tool results × 8 KiB → per-result prune rồi aggregate prune; không còn cặp tool-call/result mồ côi; conversation cực lớn → `ContextBudgetError` (không gửi vượt); injected RAG/file cắt theo budget; `estimatedTokens <= inputBudget`; min context 8192 không cắt base system.

### 7.3 Multi-agent DAG

**`src/coord.ts` (mới).**

Types: `TodoStatus`, `AgentRole` (`planner|coder|reviewer|tester|researcher`), `Todo`, `Plan`, `MergeResult`, `MergeConflict`, `PlanValidationError`, `PlanDeadlockError`, `Scheduler`, `merge`, `MAX_REWORK_CYCLES` (lấy từ `config.multiAgentReworkCap`, default 3), `applyReviewVerdict`, `ROLE_TOOLSETS`, `runDag`.

- `Plan { id, goal, todos, createdAt }`; `Todo { id, title, acceptance, dependsOn[], status, role, assignee?, artifacts[], reworkCycles }`.
- `role` bắt buộc; `assignee` chỉ nhãn hiển thị. `runDag.spawn` dùng `todo.role`, không đoán từ assignee.
- `validatePlan`: zod schema; ID không rỗng/không trùng; dependsOn trỏ ID tồn tại; no self-dep; acyclic (DFS/Kahn); role hợp lệ. Lỗi → `PlanValidationError`, emit `dag_plan_invalid`, không spawn.
- Scheduler: `depsMet(todo)` chỉ true khi **tất cả** deps `done`. Dep `failed`/`blocked` → todo `blocked`, lan xuống dependents. `dispatchReady` chỉ dispatch pending+deps done; write roles (coder) chạy **tuần tự mặc định** (`writeConcurrency=1`); read-only roles chạy song song tới `bound`. Không còn inflight/ready nhưng còn pending → `PlanDeadlockError` (mark blocked). Rework: `applyReviewVerdict` tăng `reworkCycles`; `> MAX_REWORK_CYCLES` → `failed`.
- Per-todo snapshot+rollback: trước mỗi write todo `registry.files.beginTurn()`; sau spawn `backups = takeTurnBackups()`; diff từ backups; artifacts từ backup paths. Subagents đã merge child backups vào parent (`src/subagents.ts:526-531`, `:852-858`), nên parent lấy được. Review rejected/test failed → `restoreBackups(backups)` trước rework; conflict → todo failed (không đè user edits). Plan fail/cancel → rollback tất cả backups reverse order.
- Merge là validate/aggregate, không apply: workspace đã chứa thay đổi của todo done vì `delegate_edit` ghi trực tiếp; `merge` gộp diff, phát hiện conflict, verify lại. `diffs: Record<todoId,string>`; artifacts conflict nếu hai todo done cùng file với diff non-empty → `MergeConflict`. Combined diff rỗng và không có artifacts → `verified:false, reason:'no changes'` (trừ `allowEmpty:true`). `Verifier` chạy trên combined diff/workspace; false → `MergeResult.verified=false` + `ErrNotVerified`; `runDag` rollback toàn bộ.
- Shared budget: `runDag` dùng `opts.budget` nếu có (giữ nguyên `costSpent/costLimit/startedAt/timeLimitMs/ledger`), ngược lại tạo `{ spent: 0, limit: config.runTokenBudget > 0 ? config.runTokenBudget : DEFAULT_DAG_TOKEN_BUDGET }`; sau đó `costLimit ??= config.maxCostUsd`, `timeLimitMs ??= config.maxTimeMs`, `startedAt ??= Date.now()`. Mọi spawn nhận cùng object; `Subagents` thêm tham số budget tường minh (không đọc `registry.budget` khi chạy `/dag`). `/dag` set `reg.budget = budget` trong `try/finally`, rồi restore.
- Role tool scoping: `ROLE_TOOLSETS` map role → tool names/wildcards; enforce trong `ToolRegistry.isExposed` (vì `execute` gọi `isExposed` ở `src/tools.ts:718`). Planner/Reviewer/Researcher: read-only tools. Coder: read/write/edit/apply_patch/bash + optional MCP. Tester: read + bash (bash vẫn approval-gated, không phải sandbox; `/dag` ưu tiên `runVerification` với `config.verify` **và `approve` callback gọi `tools.approve`**, fallback `DagHooks.test`). MCP mặc định **không** expose cho DAG roles; nếu cần, `DagOptions.allowedMcpTools` (danh sách `mcp__server__tool` hoặc `mcp__*`) thêm vào allowed set. Peer tools không expose.
- **Child registry propagation:** `Subagents` thêm public `runTasks(tasks, opts: { readOnly; signal; budget; allowedTools? })`; `delegate_task`/`delegate_edit` và `runDag` đều gọi method này (không gọi private `child`/`run` từ `coord.ts`). Child `ToolRegistry` constructor mở rộng `(cwd, approve, readOnly, allowedTools?)` và `isExposed` trả false nếu tool không nằm trong `allowedTools`. `runDag` truyền `ROLE_TOOLSETS[role]` + `readOnly` vào cả `delegate_task` và `delegate_edit` (không chỉ registry cha); test spawn coder/tester assert tool ngoài role bị ẩn ở child.

Ánh xạ: Planner/Reviewer/Researcher → `delegate_task` (read-only explorer); Coder → `delegate_edit` (sequential writes); multi-perspective → `run_network`/`AgentNetwork.brainstorm`; pipeline → `run_workflow`/`executeWorkflow` (v0.7 mới thêm DAG step); background → `spawn_agent` + `wait_agent`.

**`runDag(goal, hooks, opts)` flow:** validatePlan → planner → Scheduler dispatch (write serial) → spawn → review → (rejected: rollback + rework, cap) → test → (failed: rollback + rework, cap) → markDone → merge + verify; verify false → rollback all + `ErrNotVerified`.

CLI `/dag <goal>`: planner dùng read-only prompt trả JSON Plan; parse bằng `plannerPlanSchema` + `validatePlan`. `spawn` gọi trực tiếp `Subagents.runTasks` với role options (`readOnly`, `allowedTools`, shared budget); `delegate_task`/`delegate_edit` dùng cùng method; `reg.budget = budget` try/finally. `test` ưu tiên `runVerification` với `config.verify` **và `approve` callback** (approve exact commands qua `tools.approve`); fallback `DagHooks.test`. `verify` dùng `runVerification` trên workspace với cùng approve callback; nếu verification module chưa có, `/dag` báo lỗi rõ và yêu cầu `DagHooks.verify`. Không có approve callback + có command → `VerificationApprovalError` (không chạy command); AST-only vẫn chạy. `DagHooks.test` nếu chạy command cũng phải approval-gated; mặc định chỉ dùng in-process test hook, không shell.

Config keys: `multiAgentMode: 'off'|'manual'|'auto'` default `'manual'`; `multiAgentReworkCap` default 3; `multiAgentMaxConcurrency` default 2; `multiAgentMcpTools` default `[]`. `/dag` từ chối nếu mode `off`.

Events (thêm vào `HarnessEvent`):
```ts
| { type: 'dag_plan_invalid'; message: string }
| { type: 'dag_plan_ready'; planId: string; todos: number }
| { type: 'dag_todo_update'; todoId: string; status: TodoStatus; role: AgentRole; attempt?: number }
| { type: 'dag_merge'; verified: boolean; conflicts: number; reason?: string }
```

`AgentRoleType` mở rộng planner/coder/reviewer/tester/researcher (giữ default/explorer/worker). `Subagents`: `buildRolePrompt` thêm prompt role mới; `child(task, readOnly, signal, budget, role?)` và `run(..., role?)` nhận budget tường minh; `ToolRegistry` thêm `allowedTools?: ReadonlySet<string>`; `isExposed` check đầu tiên (exact + `prefix:*` + `mcp__*`), nên `execute` chặn cả hidden call. MCP denied by default.

Workflow: v0.6 chỉ thêm `runDag` như command/tool độc lập; **không đổi** `executeWorkflow` linear. v0.7 mới thêm `WorkflowStepType='dag'` + `runDag` hook.

## 8. v0.7 - Khác biệt dài hơi

Events bổ sung (additive):
```ts
| { type: 'workflow_classified'; mode: 'single' | 'multi'; reason: string }
| { type: 'scope_round'; round: number; action: 'expand' | 'contract' | 'stop'; reason: string }
| { type: 'cost_degrade'; reason: string; candidates: number }
```

### 8.1 Dynamic workflow classifier
- **Hàm thuần:** `classifyWorkflowComplexity(prompt, context): { mode: 'single'|'multi'; reason: string }` trong `src/workflow.ts`; không gọi model.
- **Tín hiệu:** số verb/imperative, chuỗi "and/then/sau đó", số file ước lượng từ path mention, keyword "plan/workflow/multi-step"; thiếu tín hiệu → `single` (conservative).
- **Tích hợp:** `run_workflow`/`/workflow` chỉ chọn DAG khi `multiAgentMode !== 'off'` và classifier trả `multi`; `executeWorkflow` linear giữ nguyên. `WorkflowStepType='dag'` + `runDag` hook trong `WorkflowExecutionHooks`.
- **Event:** `workflow_classified { mode, reason }` (additive).
- **Test/Acceptance:** bảng prompt single/multi; ambiguous → single; workflow cũ vẫn linear; không tăng cost khi single.

### 8.2 Scope loop
- **Hàm:** `runScopeLoop(goal, hooks, opts)` trong `src/coord.ts` (hoặc harness wrapper), dùng `runVerification` + `prepareContextBudget`; `opts.approve` bắt buộc khi có command (gọi `tools.approve` với exact commands); không có + có command → `VerificationApprovalError` (fail-closed), AST-only vẫn chạy.
- **Vòng lặp:** fail → mở rộng context/tests (thêm output verify, file liên quan, test command); pass → thu hẹp scope (bỏ context dư); tối đa `min(config.verify.maxRepairRounds + 1, 4)` vòng.
- **Dừng:** pass, không còn thay đổi, budget/cost/time cap, hoặc hết vòng; terminal fail → rollback theo §5.1.
- **Event:** `scope_round { round, action: 'expand'|'contract'|'stop', reason }`.
- **Test/Acceptance:** fake runner fail 2 lần rồi pass; budget cạn → dừng; không infinite loop; không vượt shared budget.

### 8.3 Multi-candidate reflection + rerank + cost-degrade
- **Hàm:** `generateCandidates(task, n)` (cap 2) + `reflect(candidate, verdict)` + `rerank(candidates)` trong `src/coord.ts`.
- **Cách ly:** mỗi candidate chạy trên backup set riêng (`beginTurn/takeTurnBackups/restoreBackups`); candidate thua bị rollback, candidate thắng được giữ.
- **Rerank:** verify pass > warnings > cost thấp; deterministic tie-break theo thứ tự sinh.
- **Cost-degrade:** nếu budget còn lại < 20% `costLimit`/`maxTimeMs` → giảm còn 1 candidate và hạ `effort`; emit `cost_degrade { reason, candidates }`.
- **Test/Acceptance:** candidate 1 fail/candidate 2 pass → chọn 2; cả hai fail → rollback tất cả; budget thấp → 1 candidate; rerank order; không vượt budget.

### 8.4 Roadmap bổ sung (v0.7)
- OS-level sandbox (Windows Job Object / Unix process group đã có) — mọi spawn qua `runCommand` hoặc policy wrapper.
- Refreshable price table (không cần release), `/cost` history, persisted budget per session; đảm bảo mọi đường gọi đều đi qua `accountUsage`.
- Full session restore từ event log + snapshot/tool messages (v0.6 roadmap events-tests): xóa session `.json`, replay dựng lại messages/tool history/checkpoints, resume chạy tiếp.
- MCP annotations trust model + process sandboxing cho stdio MCP (HTTP MCP metadata `net` bị chặn nếu không policy; stdio child kill khi abort).
- Real embedder seam wiring: `embedderProvider: 'hash'|'openai'|'ollama'` default `hash`; factory tạo `Embedder`; hash default unchanged; không leak key.
- Retention/pruning snapshots; concurrent patches không corrupt.
- OTel/metrics projection từ event log (exporter là subscriber độc lập, không ảnh hưởng replay).

## 9. Kế hoạch triển khai theo PR

**DoD chung cho mọi PR:** code + test mới/cập nhật + `npm run typecheck` + `npm run lint` + `npm run build` + `npm test` (không thêm failure so với baseline) + cập nhật README/CHANGELOG nếu PR thêm config/CLI/event + báo cáo output thật. Mỗi PR dưới đây có thêm DoD riêng.

### v0.4
- **P01** `src/syntax.ts` + tests — **S** — deps: none.
  DoD: `validateSyntax` pass TS/JS brace balance, skip strings/comments/template; JSON parse + JSONC/BOM guard; unknown ext skip; test regex/template không false-positive; typecheck/lint/test xanh.
- **P02** patch safety + public backup APIs + tool description read-first + cập nhật test cũ — **M** — deps: P01.
  DoD: stale reject; ambiguous hunk reject; insertion-only giữ nguyên; AST validation trước commit; deletion tombstone `afterDeleted` + conflict khi user recreate; test read-first cũ được cập nhật; không regression `tests/files.test.mjs`.
- **P03** `src/verify.ts` + tests — **M** — deps: P01.
  DoD: `planVerification` + `runVerification(plan, approve)`; command execution fail-closed nếu thiếu approve; detectProjects matrix; scopeFiles; parse verdict exit 0/1/timeout/notFound; timeout → fail, generic missing module → fail; local binary resolution; fake runner; không network; unit tests xanh.
- **P04** `src/risk.ts` + ToolDefinition risk + Approval risk/trusted + critical deny + auto/high CLI fix + exact-command allowlist + rejection loop + session `rejectedSignatures` + tests — **M** — deps: none (trước P05).
  DoD: critical trả denied và `approve` không được gọi; `src/cli.ts:704` không auto/always/exact/prefix cho high; workflow shell (`src/workflow.ts:633-636`) truyền risk, `approveShell` bắt buộc (thiếu → `WorkflowApprovalError`), critical/high không auto; exact allowlist chỉ medium/low; rejection loop 3 lần identical qua nhiều run → `stopped`; classifier wrapper/segment tests; risk.ts không import policy.ts.
- **P05** `src/cost.ts` + config cost fields + harness accounting + `/cost` + `/status` + tests — **M** — deps: P04.
  DoD: price/cost math; unknown model fail-closed `CostPricingError` + exit 2; `pricing.default` fallback; cost/time cap abort với lý do; `session.costUsd` canonical, `budget.costSpent` mirror (không double-add); `compactIfNeededDetailed` + usage split cho recap/btw account đúng (API cũ giữ nguyên); shared budget fields; `/cost` hiển thị; tests.
- **P06** harness verify integration (events + hook + final verification + rollback + `HarnessDeps`) — **L** — deps: P02, P03.
  DoD: fake provider fail→repair→pass; model bỏ cuộc → rollback byte-exact + `VerificationFailedError`; verify_start/result/rollback events; approval prompt hiển thị đúng command từ `planVerification`; verdict/output redacted trước session/event; subagent `{enabled:false}`.
- **P07** config/CLI/TUI/pair verify wiring (`--no-verify`, `--verify-cmd`, help, events render, pair) + cost flags — **M** — deps: P05, P06.
  DoD: env/flag precedence; `--no-verify` skip; `--verify-cmd` repeatable; TUI/pair render; help; disabled path không đổi hành vi.
- **P08** integration tests + docs v0.4 — **M** — deps: P02-P07.
  DoD: toàn bộ acceptance v0.4 §5.1-§5.4 pass; docs/README/CHANGELOG cập nhật; baseline không thêm failure.

### v0.5
- **P09** `src/eventlog.ts` + `publishEvent/renderEvent` refactor + canonical path + `--event-log` + session delete + `goon replay` + tests — **L** — deps: none (sau v0.4 để constructor ổn định).
  DoD: envelope/seq/resume; torn tail; seq sidecar + tail-scan grow cho event >64 KiB (không restart seq); mode 0600 POSIX; redaction split/join + bearer/JSON + StreamRedactor; deleteSession xóa log + sidecar; replay raw/envelopes/rebuild; tests.
- **P10** `src/stub-provider.ts`, `src/stub-fixtures.ts`, config stub, resolveRuntime, createModel, auto setup guard, `stubCli`, tests — **M** — deps: P09.
  DoD: LanguageModelV3 direct model tests text/tool/usage/error; turn selection 1-4 (match/input/sole/auxiliary); fresh cursor per model instance + explicit shared cursor for child harness; `APICallError` import từ `ai`; stub CLI không cần key/model/baseUrl; không fetch; fixture zod + 1 MiB cap.
- **P11** normalizer đệ quy + golden files + replay rebuild tests — **M** — deps: P09, P10.
  DoD: 4 golden streams (text/tool/error/usage) main-session only; replay idempotent; rebuild có user prompt + final answer; `--json` không thêm `tool_stream`/`user`.
- **P12** `src/policy.ts` + bash network classifier + network default-deny + wrapper peeling + limits centralization + tests — **L** — deps: P04.
  DoD: deny/allow/allowlist; `--allow-net-hosts`; wrapper nested; exact allowlist (đã có P04) vẫn chỉ medium/low; limits dùng chung; redteam tests.
- **P13** MCP trust/risk/network + tests — **M** — deps: P04, P12.
  DoD: `trust` không set `readOnly`; mapping destructive→high, openWorld→high+net, readOnlyHint→medium floor; annotations chỉ escalate; `McpToolSpec` giữ risk/net/trusted/annotations và `attach` truyền đủ; `net` gate; critical vẫn deny; trusted server + `--read-only` không list tool; `tests/mcp.test.mjs` pass + test mới.
- **P14** workflow/subagent budget sharing + child registry policy/costLedger + tests — **M** — deps: P05, P12.
  DoD: child kế thừa cost/time/policy; network sync; workflow child budget; tests.
- **P15** CI matrix + Windows gates + `.gitattributes` + quote glob + Termux smoke — **M** — deps: P11 (Linux xanh trước).
  DoD: matrix 3 OS × 2 Node xanh; Windows skip mode/umask/shim `gh`; quote glob; Termux smoke script documented.

### v0.6
- **P16** `src/rag.ts` + tests — **M** — deps: none.
  DoD: chunk/index/retrieve top-k; FNV determinism; `.goonignore` subset; sensitive path skip; symlink/outside-root skip; redaction; LRU evict.
- **P17** `src/memory.ts` + CLI/TUI/help + tests — **L** — deps: P16.
  DoD: 6 memory kinds; preference file tách `memories.json`; `handleEvent` single-writer; concurrent persist; `/memory` commands; TUI/help; `src/redact.ts` patterns (`redactSecrets`/`isSensitivePath`) có test.
- **P18** `src/context-budget.ts` + integration context/harness + tests — **M** — deps: P17.
  DoD: allocation invariant; min context 8192/512; per-result prune + aggregate conversation prune giữ tool-call/result pairing; `ContextBudgetError` khi không fit; untrusted block là user role; truncate referenced files; `estimatedTokens <= inputBudget`; tests.
- **P19** wire memory vào harness (ensureRepoIndexed, retrieve, handleEvent, events) — **L** — deps: P17, P18.
  DoD: index một lần/session; `ragEnabled=false` không index; patch reindex; wiring đủ `task_started/tool_result/patch_applied/verify_result/assistant_message/task_completed`; tool result redaction; memory events; HarnessDeps thêm `memory`.
- **P20** `src/coord.ts` + tests — **M** — deps: none.
  DoD: validatePlan unique/deps/cycle/role; scheduler failed-dep → blocked; write serial; rework cap; merge conflict/empty/verify false; tests.
- **P21** `runDag` + subagents/workflow/CLI `/dag` + tests — **L** — deps: P20, P05 (budget), P03 (verify optional).
  DoD: role toolset enforce trong `execute` **và** child registry (`allowedTools` propagate qua public `Subagents.runTasks`); `runDag` không gọi private methods; per-todo backup/rollback; shared budget tường minh; `/dag` lifecycle; `/dag` verify/test truyền `approve` callback, thiếu + có command → `VerificationApprovalError`; hidden tool call bị chặn ở child; tests.

### v0.7
- **P22** dynamic workflow classifier + scope loop + workflow `dag` step — **M** — deps: P21.
  DoD: classifier conservative default single; rules tests; scope loop không vượt budget; scope loop verify truyền `approve` callback; workflow cũ vẫn linear.
- **P23** multi-candidate reflection + rerank + cost-degrade — **L** — deps: P19, P21.
  DoD: cap 2 candidate; rerank order; degrade effort/model khi budget thấp; rollback candidate fail; tests.
- **P24** real embedder seam wiring — **S** — deps: P16.
  DoD: hash default unchanged; inject `openai`/`ollama` test; không leak key vào log/env.
- **P25-P27** OS sandbox, refreshable price table, full session restore, OTel, retention — **L/–** — roadmap, chia PR khi cần; DoD xác định khi scope được duyệt.

**Merge order bắt buộc:** trong mỗi version giữ dependency; không merge P06 trước P02/P03; không merge P19 trước P17/P18; không merge P21 trước P20. PR độc lập (P01, P04, P09, P16, P20) có thể làm song song nhưng vẫn nên tuần tự để tránh conflict `configSchema`/`HarnessEvent`.

## 10. Test plan và lệnh nghiệm thu

Lệnh cụ thể:
```bash
npm run typecheck
npm run lint
npm test          # = npm run build && node --test tests/*.test.mjs
npm run build
npm run release:pack
```

- **Windows (môi trường hiện tại):** `npm run typecheck`, `npm run lint`, `npm run build` exit 0. `npm test` exit 1 do 3 failure pre-existing (files.test.mjs:9 mode POSIX, files.test.mjs:25 umask, publish.test.mjs:11 shim `gh`) + skipped PTY/mode. Regression criterion: không thêm failure mới; các test POSIX-mode/shim `gh` được skip trên Windows theo §6.4.
- **Termux/Linux:** full suite phải xanh. umask guard chạy 027/007. Mode bits assert 0o600/0o755.
- **`npm run release:pack`:** chạy trên cả Windows và Termux/Linux; script pack + cài local archive, có thể cần npm registry/cache cho dependencies. Nếu môi trường offline, ghi rõ là không chạy được, không coi là fail logic.
- **Golden update:** `GOON_UPDATE_GOLDEN=1` chỉ local; CI không set.
- **Termux smoke:** `scripts/smoke-termux.sh` chạy `npm ci`, `npm run build`, `node --test tests/eventlog.test.mjs tests/stub-provider.test.mjs tests/golden.test.mjs`.
- **Stub proof:** test dùng `stubCli` không thêm `--model`/`--base-url`/`GOON_API_KEY`; assert request count 0 (không có HTTP server).
- **Replay rebuild:** xóa/rename session `.json`, chạy `goon replay <id> --rebuild-session`, assert `.replayed.json` có id/cwd/user prompt/final answer; session gốc không bị overwrite.

Checklist nghiệm thu theo version ở §5-§8 và DoD §9.

## 11. Rủi ro và câu hỏi mở

### Rủi ro kỹ thuật
- **Chạy tsc/npm test mỗi turn có thể chậm** → timeout + default tests off (verify requireTests default false).
- **Heuristic brace balance có false positive** → unknown skip; test regex/template; JSONC/BOM guard.
- **Stale check có thể chặn external edits** → lỗi rõ; previewDiff không gate.
- **Uniqueness mới có thể reject patch cũ dựa fuzzy first-match** → an toàn hơn; insertion-only giữ nguyên.
- **Windows command availability** → resolve local binary + skip safe; test smoke win32.
- **Breaking change có chủ đích:** apply_patch read-first; test và tool description phải cập nhật trong P02; model/workflow dùng apply_patch trực tiếp cần biết.
- **Verify command là code execution** → approval gate + subagent disable; không phải sandbox network; script package.json vẫn có thể fetch nếu user/auto mode cho phép.
- **fsync mỗi event có thể chậm** → chỉ bật `--event-log`; batch fsync để roadmap v0.7.
- **LanguageModelV3 mapping có thể lệch AI SDK 6** → prototype + test direct model trước CLI golden.
- **Golden flakiness** nếu normalize thiếu field volatile → normalizer đệ quy bắt buộc.
- **Risk classifier heuristic false positive high/critical** → làm phiền user; config `riskOverrides` (roadmap v0.7).
- **Network default-deny có thể phá workflow hiện tại dùng network** → docs + `--allow-net`.
- **MCP annotations không đáng tin** → chỉ escalate, không downgrade; critical luôn deny.
- **Process-group kill không áp dụng MCP** → gap theo dõi; test.
- **Cost price table có thể stale** → fail-closed `CostPricingError` khi `maxCostUsd>0` và thiếu giá; user set `pricing.default` hoặc tắt cap.
- **Rejection-loop guard bền vững theo session** → reset nếu session reset.
- **Network gate (v0.5) chỉ chặn command classified network** → `python -c`/`node -e` có thể gọi network mà classifier không thấy; giới hạn đã biết, auto mode không phải sandbox.
- **Regex chunking chất lượng thấp hơn AST** → fallback window; không claim semantic.
- **Cold-start indexing chậm trên repo lớn** → ragEnabled default false, cap maxFiles, progress+skip, ensureRepoIndexed một lần/session.
- **Cache rag.json stale nếu mtime/size không đổi nhưng content đổi** → reindex thủ công.
- **Role tool filtering có thể ảnh hưởng test subagent hiện có** → chỉ áp dụng cho DAG.
- **Tester bash không phải sandbox** → user approve; `/dag` ưu tiên verification runner.
- **truncateSystemToBudget có thể cắt AGENTS.md** → emit cảnh báo; base rules giữ lại.
- **Budget allocation có thể giảm context cho repo lớn** → compaction safety net + config ratios.

### Câu hỏi mở (cần người dùng quyết định)
1. Default `verify.enabled`: đề xuất `true` (safe skips + approval skip). Nếu maintainer muốn opt-in, đặt `false`. (verification I.1)
2. Default `requireTests`: đề xuất `false` = skip hoàn toàn; `true` = chạy và phải pass. (I.2)
3. Hook chạy sau mỗi executeBatch hay chỉ cuối turn: đề xuất sau mỗi batch có edit + final check trước return. (I.3)
4. Terminal rollback có `force` hay không: đề xuất không force, retain conflicts. (I.4)
5. `verify.commands` có cho phép shell (`!`) hay không: đề xuất cho phép opt-in rõ ràng. (I.5)
6. Giá theo model nên lưu ở config hay file riêng refreshable? (v0.7) (cost-risk)
7. `/cost` có cần persist cost history theo turn hay chỉ tổng session? (v0.4 đề xuất tổng + checkpoint) (cost-risk)
8. High confirmation nên bắt buộc gõ tên tool hay gõ lại full command? (v0.4 đề xuất gõ tên tool) (cost-risk)
9. `--allow-net` có nên persist vào config hay chỉ runtime flag? (v0.5 đề xuất runtime flag + config optional) (cost-risk)
10. Có cần `--event-log` mặc định on ở v0.6 không? (đề xuất opt-in v0.5, benchmark rồi mới default) (events-tests)
11. `--rebuild-session` ghi cùng id hay id `-replayed`? (đề xuất `-replayed`) (events-tests)
12. Có cần log workflow child session ở v0.5 không? (đề xuất v0.6/v0.7; v0.5 chỉ main session) (events-tests)
13. Stub fixture match theo regex/JSON pointer thay vì substring? (đề xuất substring đủ cho golden v0.5) (events-tests)
14. Provider embedding thật cho v0.7 (openai vs ollama vs local) — chỉ ảnh hưởng P24. (memory-agents)
15. Có nên expose raw `exec` memory qua `/memory exec` cho debug không? Đề xuất không, chỉ summary. (memory-agents)
16. Có nên giữ per-todo backups qua session restart (v0.7) hay chỉ RAM? Đề xuất chỉ RAM ở v0.6. (memory-agents)
17. `memoryEnabled` default false (opt-in) để giữ backward compatibility; có bật true sau benchmark Termux không? (memory-agents)

## 12. Checklist bàn giao

Agent tự kiểm trước khi bàn giao:
- [ ] Đọc toàn bộ plan; không sửa source ngoài phạm vi PR.
- [ ] Mỗi PR có test mới/cập nhật tương ứng.
- [ ] `npm run typecheck` exit 0.
- [ ] `npm run lint` exit 0.
- [ ] `npm run build` exit 0.
- [ ] `npm test` không thêm failure mới (so với baseline 3 pre-existing Windows failures).
- [ ] `npm run release:pack` thành công (nếu đụng packaging).
- [ ] Config cũ parse được; session v1 parse được; HarnessEvent consumer cũ không crash.
- [ ] API key không vào env con (verify runner dùng childEnv; stub không đọc credential).
- [ ] Approval gate vẫn đứng trước verify/bash/MCP; critical deny trước approval.
- [ ] Redaction: secret literal (kể cả bị chia chunk) không xuất hiện trong event log/persist/index.
- [ ] README/CHANGELOG cập nhật cho lệnh/config/event mới.
- [ ] Báo cáo output thật: files changed, test count, exit codes.
- [ ] Các câu hỏi mở §11 đã escalate hoặc ghi rõ quyết định đã lấy (với lý do).
- [ ] Giới hạn đã biết được tài liệu hóa: bash/MCP writes không tracked (v0.4); event log chỉ main session; workflow linear chưa DAG; tester bash không sandbox; network gate chỉ command classified network.

---

*Baseline: typecheck=0, lint=0, build=0, tests=1 (exit code 1; 138 tests / 133 pass / 3 fail / 2 skipped trên Windows; full suite xanh trên Termux/Linux). Không thêm native dependency. Giữ backward compatibility.*
