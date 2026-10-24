# Báo cáo triển khai — goon-cli v0.4 → v0.7 (P01–P24)

- **Nhánh:** `fix/security-audit-v0.3.1` — HEAD `eb7839a` (`chore: release v0.3.1 [skip ci]`)
- **Ngày báo cáo:** 2026-10-24
- **Tài liệu tham chiếu:** `docs/PLAN-yolo-port.md` (§5–§9, §11)
- **Trạng thái quan trọng nhất: TOÀN BỘ THAY ĐỔI CHƯA ĐƯỢC COMMIT.** Xem §9.

---

## 1. Tóm tắt

| Chỉ số | Baseline | Cuối đợt triển khai |
|---|---|---|
| Gate | **pass** | **pass** |
| Full test suite | **3 failures** | **1 failure** (chưa đạt 0) |
| E2E | — | **pass** |
| Release pack | — | **THẤT BẠI** — exit 1, `spawn npm ENOENT` (lỗi môi trường/spawn, xem §7.4) |
| Security findings mở | — | **7** (1 high, 2 medium, 4 low) — chưa fix |

**Điểm chính:**

1. Đã triển khai đủ **24/24 phase (P01–P24)** qua 10 đợt, tương ứng v0.4 (verification/patch safety, risk class, cost ledger, harness/CLI/TUI), v0.5 (event log/replay/stub provider, execution policy/MCP, budget share/CI), v0.6 (memory/RAG, context budget, multi-agent DAG), v0.7 (workflow động, multi-candidate, embedder). Chi tiết ở §2–§3.
2. Full test suite giảm từ **3 failures → 1 failure**; gate và E2E đều pass. **Còn 1 failure chưa được xác định tên test** trong dữ liệu bàn giao — chưa thể tuyên bố bộ test sạch (xem §4.4).
3. **Release pack fail** (`npm run release:pack` → exit 1, `spawn npm ENOENT`). Đây là lỗi môi trường/spawn của script pack, không phải lỗi test hay build (typecheck/lint/build đều xanh ở các đợt trước). Cần chạy lại trên môi trường đúng hoặc sửa script (xem §7.4).
4. **Audit bảo mật cuối phát hiện 7 findings chưa xử lý**, trong đó 1 high (bypass phân loại `rm -rf` qua backslash-newline / ANSI-C quoting) và 2 medium (package-manager script trong auto mode — known issue 7; verify commands đi ngoài execution policy). Các nhóm bảo mật khác (secret/env con, redaction, approval, MCP trust, subagent self-escalation, patch/path traversal, event log poisoning, cost cap, wrapper peeling, process kill) **đã kiểm và ĐẠT** (xem §6).
5. Toàn bộ thay đổi (23 file sửa + ~40 file/module mới + test) nằm trong working tree, **chưa add/commit/push**.

---

## 2. Phạm vi P01–P24

Phân bổ theo plan §7.1–§7.3/§9, đã kiểm tra sự tồn tại của toàn bộ module mới trong cây source hiện tại:

| Version | Phases | Module mới chính | Module sửa chính | Nội dung |
|---|---|---|---|---|
| v0.4 | P01–P03 | `src/syntax.ts`, `src/verify.ts` | `src/files.ts`, `src/session.ts`, `src/index.ts`, `src/tools.ts` | Verification core (validateSyntax TS/JS/JSON, detectProjects, scopeFiles, planVerification, runVerification) + harden `apply_patch` (read-first revision check, hunk ambiguity reject, syntax gate trước commit, deletion tombstone `afterDeleted`, public backup APIs) |
| v0.4 | P04–P05 | `src/risk.ts`, `src/cost.ts` | `src/tools.ts`, `src/approval.ts`, `src/cli.ts`, `src/session.ts`, `src/subagents.ts`, `src/workflow.ts` | RiskClass + phân loại bash/MCP (critical deny trước approval, high không bao giờ auto-remember, exact/prefix allowlist chỉ medium/low, rejection loop bền theo session); CostLedger (model pricing, fail-closed `CostPricingError`, maxCost/maxTime, cached-token split, `session.costUsd` canonical + mirror `budget.costSpent`, budget cha/con dùng chung) |
| v0.4 | P06–P08 | — | `src/harness.ts`, `src/cli.ts`, `src/tui.ts`, `src/pair.ts`, `src/config.ts`, `src/help.ts` | Verify hook trong Harness (inject lỗi đã redact, repair cap, rollback byte-exact, final verification trước exit; verify tắt cho subagent/workflow/network con); wiring CLI/TUI/pair/help cho verify + cost flags |
| v0.5 | P09–P11 | `src/redact.ts`, `src/eventlog.ts`, `src/stub-provider.ts`, `src/stub-fixtures.ts` | `src/cli.ts`, `src/session.ts`, `src/config.ts`, `src/index.ts` | Event log JSONL append-only (envelope v/seq/at/sessionId/type/event, fsync từng dòng, mode 0600, sidecar `.seq` atomic, torn-tail repair, `deleteSession` xoá log+sidecar+tmp); CLI `--event-log`/`--no-event-log`, `goon replay` (raw/envelopes/`--rebuild-session`); stub provider LanguageModelV3 deterministic + fixtures zod cap 1 MiB, không fetch/không API key; 4 golden streams (text/tool/error/usage) + normalize đệ quy |
| v0.5 | P12–P13 | `src/policy.ts` | `src/tools.ts`, `src/mcp.ts`, `src/risk.ts`, `src/workflow.ts`, `src/config.ts`, `docs/MCP.md` | Execution policy network deny/allow/allowlist (host normalization, wrapper-aware extraction cho curl/wget/ssh/scp/rsync/git/nc/package managers, fail-closed với redirect flags); deny trước approval cho bash + MCP net tools; `--allow-net`/`--allow-net-hosts`, `GOON_ALLOW_NET*`; output cap byte-bounded; MCP risk/annotations (trust ≠ readOnly, destructive/openWorld → high, trust auto-approve chỉ medium/low) |
| v0.5 | P14–P15 | — | `src/subagents.ts`, `src/network.ts`, `src/workflow.ts`, `.github/workflows/release.yml`, `.gitattributes`, `scripts/smoke-termux.sh` | Budget/RunBudget/CostLedger + ExecutionPolicy dùng chung cha/con (subagents, spawned agents, Agent Network personas, workflow steps); sync `budget.costSpent` về caller sau mỗi persona + synthesis; CI matrix 3 OS × 2 Node, fail-fast: false, umask guard POSIX-only |
| v0.6 | P16–P17 | `src/rag.ts`, `src/memory.ts` | `src/cli.ts`, `src/config.ts`, `src/help.ts` | RAG lexical (HashEmbedder FNV-1a 384-dim deterministic, chunkFile hàm/class TS/JS/Go/Python, LexicalIndex cosine top-k LRU, `.goonignore`, walkRepo an toàn symlink/sensitive/binary, redactSecrets trước chunk, snapshot `rag.json`, `ensureRepoIndexed` mtime/size dedup); MemoryStore 6 kinds, `preferences.json` tách `memories.json`, single-writer atomic write, `retrieve(query, budgetTokens)`; CLI `/memory rag index|stats|search`, `/memory budget`, `/memory pref …` |
| v0.6 | P18–P19 | `src/context-budget.ts` | `src/harness.ts`, `src/cli.ts`, `src/context.ts`, `src/config.ts` | Context budget (invariant reserve+system+conversation+files === inputBudget, min 8192/512 không cắt base system, `pruneToolResults` 8 KiB head/tail + dedup, `pruneConversation` giữ pairing tool-call/result và không bỏ user/assistant gần nhất, `ContextBudgetError` fail-closed, 3 config ratios); wiring memory vào Harness (ensureRepoIndexed 1 lần/session, retrieve theo files budget, 4 event mới `context_budget`/`memory_indexed`/`memory_retrieved`/`memory_error`, patch reindex cuối run) |
| v0.6 | P20–P21 | `src/coord.ts` | `src/tools.ts`, `src/subagents.ts`, `src/cli.ts`, `src/config.ts` | Multi-agent DAG (`validatePlan`, Scheduler write-serial/read-parallel, `applyReviewVerdict` rework cap, merge conflict/empty/verify, `runDag` per-todo backup/rollback, ROLE_TOOLSETS, `ErrNotVerified`/`PlanDeadlockError`); `allowedTools` enforce cả `schemas()` lẫn `execute()`; CLI `/dag` + `goon dag <goal>` (planner LLM + verify qua runVerification có approve callback); 4 event `dag_*` |
| v0.7 | P22–P24 | `src/embedder.ts` | `src/workflow.ts`, `src/coord.ts`, `src/cli.ts`, `src/tui.ts`, `src/config.ts` | `classifyWorkflowComplexity` thuần (default `single`, bảo thủ) + step type `dag` mới (chỉ chạy khi classifier=multi ∧ multiAgentMode≠off ∧ có hook `runDag`); `runScopeLoop` verify fail-closed qua `approve`, cap vòng `min(maxRepairRounds+1, 4)`, rollback §5.1 reverse order; `runCandidates` (cap 2, isolation turn backup/rollback-on-fail), `reflect`, `rerank` (pass>warnings>cost>index), `costDegrade` (<20% ngân sách → 1 candidate + hạ effort, emit `cost_degrade`); embedder factory `hash|openai|ollama` (key chỉ trong Authorization header, không log, không gửi tới ollama); 3 event additive (`workflow_classified`, `scope_round`, `cost_degrade`); `runDagCore` dùng chung cho `/dag` và `dag` steps |

Số đợt: **10 đợt triển khai**, phủ đủ **P01–P24**. P25–P27 chưa triển khai (xem §7.1).

---

## 3. Kết quả từng đợt

> Số liệu dưới đây là kết quả đã ghi nhận tại thời điểm hoàn thành từng đợt (từ biên bản bàn giao của từng đợt). "Targeted" = chạy `node --test` trên nhóm file test cụ thể, không phải full suite.

### 3.1 v0.4 — Verification core và patch safety (P01–P03)
- Thêm `src/syntax.ts` (`validateSyntax`: cân bằng brace TS/JS bỏ qua string/comment/template/regex; JSON parse xử lý JSONC/BOM) và `src/verify.ts` (`detectProjects`, `scopeFiles`, `resolveLocalTool`, `planVerification`, `runVerification`, `parseCommandResult`, `createVerifyRunner`, format/redact verdict).
- Harden `apply_patch` trong `src/files.ts`: read-first revision check, `findHunkIndex` từ chối hunk mơ hồ, validate syntax trước commit, deletion tombstone (`afterDeleted`) restore có nhận diện conflict, public backup APIs (`peekTurnBackups`/`turnChangedFiles`/`hasTurnChanges`/`clearTurnBackups`/`replaceTurnBackups`), lan truyền `afterDeleted` trong `mergeBackups`. Cập nhật session FileBackup schema, mô tả tool `apply_patch`, export index, README/CHANGELOG.
- Test: `tests/syntax.test.mjs` mới (8/8), `tests/verify.test.mjs` mới (15/15), `tests/files.test.mjs` 15/17 (**2 failure pre-existing trên Windows**), `tests/audit.test.mjs` 21/21. Typecheck, lint, build pass.

### 3.2 v0.4 — Risk class và cost ledger (P04–P05)
- P04: RiskClass + phân loại shell/MCP; critical deny **trước** approval; high không bao giờ auto-remember; exact/prefix allowlist chỉ áp medium/low; rejection loop bền theo session; workflow shell approval/risk.
- P05: `CostLedger` + model pricing; fail-closed `CostPricingError` khi `maxCostUsd>0` thiếu giá; cap maxCost/maxTime; tách cached-token usage; `session.costUsd` canonical với mirror `budget.costSpent`; cập nhật `/cost` và `/status`; budget cha/con dùng chung; tổng hợp cost con.
- Đã xử lý 2 known issue: (1) thống nhất account signature; (5) child harness không còn ghi đè mirror budget dùng chung.
- Test: thêm `tests/risk.test.mjs`, `tests/cost.test.mjs`. Typecheck, lint, build và targeted tests pass.

### 3.3 v0.4 — Harness, CLI, TUI và tài liệu (P06–P08)
- Verify hook trong Harness: sau batch có edit → inject lỗi đã redact → repair cap → rollback byte-exact → final verification trước exit path; verify tắt cho subagent/workflow/network con. Wiring config/CLI/TUI/pair/help cho verify + cost flags, cùng integration tests.
- Known issues: đã xử lý (3) commands trong `verify_start`, (4) reset `verifyAttempts` khi deny/skip, (5) seed budget từ `ledger.total` thay vì session của child; bổ sung red-team test cho known issue 7; known issue 1, 2, 6 xác minh đúng/không còn áp dụng.
- Số liệu: typecheck/lint/build sạch; bộ test targeted **140 pass / 2 skip / 0 fail** (2 failure `files.test.mjs` là pre-existing trên Windows, tái hiện được tại HEAD sạch). Full `npm test` không chạy theo ràng buộc đợt đó.

### 3.4 v0.5 — Event log, replay và stub provider (P09–P11)
- P09: `src/redact.ts` + `src/eventlog.ts` (JSONL append-only, envelope `v/seq/at/sessionId/type/event`, fsync từng dòng, mode 0600 POSIX, sidecar `.seq` atomic, tail scan 64 KiB→1 MiB/full-file, torn-tail truncated, redaction literal/bearer/JSON/key-value + `StreamRedactor`, `deleteSession` xoá log+sidecar+tmp); CLI `--event-log`/`--no-event-log`; `goon replay` raw/`--envelopes`/`--rebuild-session` (cảnh báo seq gap, không cần model/key, không ghi đè session gốc).
- P10: stub provider LanguageModelV3 deterministic (`doGenerate`/`doStream`, usage nested, `APICallError`), `stub-fixtures` zod + cap 1 MiB, chọn turn theo match/input/single-unused, cursor mới mỗi model + chia sẻ qua runtime cho child harness; không fetch/không API key; cấu hình stub/stubFixture/eventLog qua file/env/CLI; doctor báo `stub:true`.
- P11: `normalizeEvent`/`normalizeStream` đệ quy (path, session/subagent id, timestamp, durationMs, seq); 4 golden streams text/tool/error/usage; replay raw/envelopes idempotent; rebuild partial-safe/idempotent; `--json` giữ tương thích (không in user/tool_stream).
- Cập nhật README, CHANGELOG, help, export index. Không sửa `docs/PLAN-yolo-port.md`, `.mimosa`, `.zcode`.

### 3.5 v0.5 — Execution policy và MCP (P12–P13)
- `src/policy.ts`: network modes deny/allow/allowlist, chuẩn hoá host chính xác, wrapper-aware host extraction (curl/wget/ssh/scp/rsync/git/nc/package managers), shared process limits, helper `allowNet` cho MCP server.
- Wire policy vào ToolRegistry (deny trước approval cho bash + MCP net tools), workflow shell steps, CLI `--allow-net`/`--allow-net-hosts`, config `allowNet`/`allowNetHosts` + `GOON_ALLOW_NET*`, registry con/subagent. Thêm `outputLimitBytes`/`hardOutputBytes` vào `runCommand`, `limitTextBytes` byte-bounded cho MCP output. MCP tool risk/net/trusted/annotations: trust không còn kéo theo readOnly, destructive→high, openWorld→high+net, readOnlyHint floor medium, readOnlyTools low, trusted auto-approve giới hạn medium/low.
- Số liệu: build pass, lint sạch; `node --test tests/policy.test.mjs tests/mcp.test.mjs tests/risk.test.mjs` → **55 tests / 55 pass / 0 fail**; targeted features/files/subagents/harness-verify/audit/runtime/tui/golden/verify — chỉ còn 2 failure pre-existing Windows (mode/umask) ở features/files.

### 3.6 v0.5 — Chia sẻ budget và CI đa nền tảng (P14–P15)
- P14: child run budgets thống nhất — subagents, spawned agents, Agent Network personas, workflow steps dùng chung RunBudget/CostLedger của cha; registry con kế thừa ExecutionPolicy của cha; Agent Network sync `budget.costSpent` về caller sau mỗi persona và synthesis. Sửa `/workflow` và `/network` pass budget/policy dùng chung thay vì khởi tạo budget mới mỗi step.
- P15: CI matrix 3 OS × 2 Node (đã xác minh `.github/workflows/release.yml:22-26`: `ubuntu-latest, windows-latest, macos-latest` × `['22.16.0','24']`, `fail-fast: false`), umask guard non-Windows, test script quote glob, test POSIX-only skip trên win32, thêm `.gitattributes` + `scripts/smoke-termux.sh`.
- Regression tests mới: policy deny trước approval, ledger/mirror sharing, ignored allowedTools, network cost sync, workflow token-budget sharing. Typecheck/lint/build + targeted tests pass trên Windows (**0 failures; 3 POSIX-only skips**).

### 3.7 v0.6 — Memory và RAG lexical (P16–P17)
- P16 (`src/rag.ts`): HashEmbedder FNV-1a 384 chiều deterministic; Embedder seam; `chunkFile` (hàm/class TS/JS/Go/Python bằng regex, fallback 40 dòng overlap 8, soft 1500/hard 4000 ký tự); `LexicalIndex` in-memory (cosine, top-k tie-break deterministic, threshold, LRU eviction); `parseGoonignore`/`isIgnored` (subset gitignore); `walkRepo`/`indexRepo` an toàn (skip symlink, realpath ngoài root, >1 MiB, binary, sensitive path, `.goonignore`, skip dirs); `redactSecrets` trước chunk; snapshot `rag.json` (vector tính lại khi load); `ensureRepoIndexed` so mtime/size + in-flight dedup.
- P17 (`src/memory.ts` + CLI/TUI/help): MemoryStore 6 kinds (working/conversation view/exec rolling 50/repo/knowledge/preference); `preferences.json` tách `memories.json`; `handleEvent` single-writer qua promise chain + atomic write; `retrieve(query, budgetTokens)` trả working notes + repo tree + rag chunks + preference liên quan. CLI `/memory rag index|stats|search`, `/memory budget`, `/memory pref set|get|rm|list` (mở on-demand, luôn đóng, lỗi không fatal); `/memories` cũ giữ nguyên. Config `memoryEnabled`/`ragEnabled=false` + `ragTopK`/`ragCapacity`/`ragThreshold`/`ragIndexMaxFiles`/`ragIndexMaxFileBytes` + 4 env `GOON_*`.
- Số liệu: typecheck/lint/build exit 0; `node --test tests/rag.test.mjs tests/memory.test.mjs` → **28/28 pass**; suites lân cận (runtime, eventlog, tui, harness-verify, features, audit, golden, cost) → **159 pass / 3 skipped (pre-existing Windows) / 0 fail**.

### 3.8 v0.6 — Context budget và wiring memory (P18–P19)
- P18 (`src/context-budget.ts`): `allocateContextBudget` với invariant reserve+system+conversation+files === inputBudget; min context 8192/512 không cắt base system; `truncateSystemToBudget` cắt AGENTS.md/memories trước identity; `pruneToolResults` 8 KiB head/tail + dedup; `pruneConversation` aggregate giữ pairing tool-call/result, không bao giờ bỏ user/assistant gần nhất; truncate `referenced_file` blocks; `ContextBudgetError` fail-closed; untrusted block là user role. 3 config keys `contextReserveRatio`/`contextConversationRatio`/`contextFilesRatio` (default 0.15/0.45/0.25); wire `prepareContextBudget` vào `harness.run` và `expandFileMentions maxChars`.
- P19: `HarnessDeps.memory`; `ensureRepoIndexed` 1 lần/session chỉ khi `ragEnabled`; `retrieve` mỗi request theo files budget; wiring đủ `task_started`/`tool_result`/`patch_applied`/`verify_result`/`assistant_message`/`task_completed` (redact, không raw output); patch reindex cuối run có edit; mở/đóng MemoryStore trong `cli.ts` khi `memoryEnabled`; 4 event mới (`context_budget`, `memory_indexed`, `memory_retrieved`, `memory_error`).
- Sửa 2 regression do event mới gây ra (subagent progress channel, golden streams). Số liệu: typecheck/lint/build exit 0; **toàn bộ 27 file test chạy targeted đều pass**.

### 3.9 v0.6 — Multi-agent DAG (P20–P21)
- `src/coord.ts`: `validatePlan`, Scheduler write-serial/read-parallel, `applyReviewVerdict` với rework cap, merge conflict/empty/verify, `runDag` per-todo backup/rollback, `ROLE_TOOLSETS`, `ErrNotVerified`/`PlanDeadlockError`. Mở rộng ToolRegistry với `allowedTools` (enforce cả `schemas()` lẫn `execute()`); `Subagents.runTasks` public pin toolset cha; CLI `/dag` + `goon dag <goal>` (planner LLM + verify/test qua `runVerification` có approve callback); config keys `multiAgent*`; 4 sự kiện `dag_*` render CLI/TUI/pair; help/README/CHANGELOG.
- Số liệu: typecheck/lint/build xanh; **26 test mới** (`tests/coord.test.mjs` + `tests/dag.test.mjs`) và toàn bộ 27 file test cũ chạy targeted đều pass (76/76 khi chạy song song 6 file).

### 3.10 v0.7 — Workflow động và multi-candidate (P22–P24)
- P22: `classifyWorkflowComplexity` thuần (bảo thủ, default `single`) trong `src/workflow.ts`; step type `dag` mới (schema JSON + markdown `[dag]`) chỉ chạy khi classifier=multi ∧ multiAgentMode≠off ∧ có hook `runDag`; `runScopeLoop` trong `src/coord.ts` — verify fail-closed qua `approve` callback, cap vòng `min(maxRepairRounds+1, 4)`, dừng theo budget/cost/time, rollback §5.1 (reverse order).
- P23: `runCandidates` (cap 2, isolation `beginTurn`/`takeTurnBackups`/rollback-on-fail), `reflect`, `rerank` (pass>warnings>cost>index), `costDegrade` (<20% ngân sách còn lại → 1 candidate + hạ effort, emit `cost_degrade`).
- P24: `src/embedder.ts` — factory `hash|openai|ollama` (hash mặc định không đổi; key chỉ nằm trong header Authorization, không log/scrub trong lỗi/không gửi tới ollama); config `embedderProvider`/`embedderModel`/`embedderBaseUrl` + env; wire vào MemoryStore trong `cli.ts`.
- 3 event additive (`workflow_classified`, `scope_round`, `cost_degrade`) vào HarnessEvent + render CLI/TUI; extract `/dag` core thành `runDagCore` dùng chung cho `/dag` và `dag` steps của `run_workflow`/`/workflow`. 7 lỗi đã biết trong đề bài đợt này đều đã được sửa và xác minh từng vị trí trong tree. README/CHANGELOG cập nhật.

---

## 4. Lệnh đã chạy + kết quả

> **Nguồn dữ liệu:** các lệnh mục 4.1–4.3 là kết quả đã ghi nhận từ từng đợt triển khai và từ đợt audit bảo mật (không chạy lại trong lần viết báo cáo này). Các lệnh mục 4.5 là những lệnh tôi **tự chạy trong phiên viết báo cáo này** để đối chiếu.

### 4.1 Gate (typecheck / lint / build) — theo từng đợt

| Đợt | typecheck | lint | build |
|---|---|---|---|
| v0.4 P01–P03 | pass | pass | pass |
| v0.4 P04–P05 | pass | pass | pass |
| v0.4 P06–P08 | sạch | sạch | sạch |
| v0.5 P09–P11 | — (không ghi số) | — | — |
| v0.5 P12–P13 | — | sạch | pass |
| v0.5 P14–P15 | pass | pass | pass |
| v0.6 P16–P17 | exit 0 | 0 warnings/0 errors, exit 0 | exit 0 |
| v0.6 P18–P19 | exit 0 | exit 0 | exit 0 |
| v0.6 P20–P21 | xanh | xanh | xanh |
| v0.7 P22–P24 | — | — | — |

Lưu ý trung thực: một số đợt (P09–P11, P22–P24) không ghi số exit code cụ thể trong biên bản, chỉ ghi "cập nhật tài liệu/xác minh vị trí"; không có đợt nào báo gate fail.

### 4.2 Targeted tests — theo từng đợt

| Đợt | Lệnh / phạm vi | Kết quả |
|---|---|---|
| v0.4 P01–P03 | syntax / verify / files / audit | 8/8, 15/15, **15/17 (2 fail pre-existing Windows)**, 21/21 |
| v0.4 P04–P05 | risk / cost (mới) + targeted | pass |
| v0.4 P06–P08 | bộ targeted | **140 pass / 2 skip / 0 fail** |
| v0.5 P12–P13 | `node --test tests/policy.test.mjs tests/mcp.test.mjs tests/risk.test.mjs` | **55/55 pass, 0 fail** |
| v0.5 P14–P15 | targeted trên Windows | **0 failures; 3 POSIX-only skips** |
| v0.6 P16–P17 | `node --test tests/rag.test.mjs tests/memory.test.mjs` | **28/28 pass** |
| v0.6 P16–P17 | suites lân cận (runtime, eventlog, tui, harness-verify, features, audit, golden, cost) | **159 pass / 3 skipped / 0 fail** |
| v0.6 P18–P19 | 27 file test targeted | pass |
| v0.6 P20–P21 | coord + dag (mới) + 27 file cũ | **26 test mới pass**; 76/76 khi chạy song song 6 file |
| v0.7 P22–P24 | — (biên bản ghi xác minh 7 lỗi đã sửa theo vị trí, không ghi số test) | — |

### 4.3 Targeted tests — đợt audit bảo mật

Audit bảo mật chạy 5 nhóm targeted (tổng **252 tests, 0 fail**), có kiểm tra `dist/` tươi hơn `src` (mtime) nên test chạy trên build hiện hành:

| Nhóm lệnh | Kết quả |
|---|---|
| `node --test tests/risk.test.mjs tests/eventlog.test.mjs tests/verify.test.mjs tests/cost.test.mjs tests/coord.test.mjs` | 82 tests / 81 pass / 0 fail / 1 skip |
| `node --test tests/mcp.test.mjs tests/files.test.mjs tests/policy.test.mjs` | 51 tests / 49 pass / 0 fail / 2 skip (mode-POSIX/umask skip đúng trên Windows theo plan §6.4) |
| `node --test tests/harness-verify.test.mjs tests/subagents.test.mjs tests/scope-loop.test.mjs tests/workflow-classifier.test.mjs` | 48 pass / 0 fail |
| `node --test tests/memory.test.mjs tests/rag.test.mjs tests/context-budget.test.mjs tests/stub-provider.test.mjs tests/workflow.test.mjs` | 61 pass / 0 fail |
| `node --test tests/e2e/redaction.test.mjs tests/e2e/approvals.test.mjs tests/e2e/eventlog-resume.test.mjs tests/e2e/budget-cost.test.mjs` | 10 pass / 0 fail |

### 4.4 Full suite, E2E và release pack (kết quả cuối do workflow chạy)

| Kiểm tra | Kết quả | Ghi chú |
|---|---|---|
| Gate cuối | **pass** | — |
| Full `npm test` cuối | **1 failure** (baseline 3 failures) | Tên test failure cụ thể không có trong dữ liệu bàn giao — cần xem log của lần chạy full cuối để xác định. **Không tuyên bố bộ test sạch.** |
| E2E cuối | **pass** | Xem §5 |
| `npm run release:pack` | **exit 1 — THẤT BẠI** | Lỗi: `Error: spawn npm ENOENT` (errno -4058, syscall `spawn npm`, spawnargs `['run','build']`, Node.js v22.14.0). Script `scripts/pack-release.mjs:21` gọi `command('npm', ['run', 'build'], root)` qua child_process; trên Windows spawn('npm') không shell không resolve được `npm.cmd` → ENOENT. **Lỗi môi trường/spawn của script pack, không phải lỗi build hay test.** Đáng chú ý thêm: môi trường chạy pack có Node v22.14.0, thấp hơn `engines` yêu cầu `>=22.16.0` trong `package.json:18`. |

### 4.5 Lệnh tôi tự chạy trong phiên viết báo cáo này

Các lệnh dưới đây tôi đã thực thi để đối chiếu báo cáo với cây source hiện tại; kết quả khớp với dữ liệu bàn giao:

1. `git log --oneline -3 && git branch --show-current && git status --short` → HEAD `eb7839a`, branch `fix/security-audit-v0.3.1`, **78 mục thay đổi chưa commit** (18 file `src/` sửa + ~14 module mới + test/docs/CI). Xác nhận **chưa commit/push**.
2. `ls src/*.ts` → đủ 15 module mới: `context-budget.ts`, `coord.ts`, `cost.ts`, `embedder.ts`, `eventlog.ts`, `memory.ts`, `policy.ts`, `rag.ts`, `redact.ts`, `risk.ts`, `stub-fixtures.ts`, `stub-provider.ts`, `syntax.ts`, `verify.ts` (+ `risk.ts` thuộc P04) — khớp bảng §2.
3. Đọc và đối chiếu từng vị trí code của 7 security findings (kết quả khớp, xem §6.1).
4. `grep -n "spawn\|npm" scripts/pack-release.mjs` → dòng 21–28 spawn `npm` trực tiếp — nhất quán với lỗi ENOENT ở §4.4.
5. `grep -n -A6 "matrix" .github/workflows/release.yml` → matrix 3 OS × 2 Node, fail-fast: false — khớp P15.
6. `grep -n "P2[5-9]" docs/PLAN-yolo-port.md` → dòng 819 xác nhận P25–P27 còn ở trạng thái roadmap.

**Không chạy trong phiên này** (và vì thế không tuyên bố kết quả): full `npm test`, typecheck, lint, build, `npm run release:pack`, smoke-termux — các giá trị tương ứng lấy từ §4.1–§4.4 theo dữ liệu đã ghi nhận.

---

## 5. E2E như người dùng

- **Kết quả E2E cuối: pass** (theo gate cuối của workflow).
- Bộ E2E trong repo: `tests/e2e/` gồm 6 file test + `helpers.mjs`:
  - `redaction.test.mjs`, `approvals.test.mjs`, `eventlog-resume.test.mjs`, `budget-cost.test.mjs` — **đã chạy trong đợt audit: 10 tests / 10 pass / 0 fail**.
  - `cli-basics.test.mjs`, `verify-loop.test.mjs` — **audit không chạy riêng** (ngoài checklist bảo mật trực tiếp); thuộc full suite cuối đã chạy bởi workflow với kết quả tổng thể còn 1 failure.
- Luồng người dùng được phủ bởi E2E + integration: chạy CLI với event log bật, approve/deny tool, replay/`--rebuild-session` từ log, resume session, budget/cost chia sẻ cha–con, redaction secret trong log/JSON/session.

---

## 6. Bảo mật

### 6.1 Findings chưa xử lý (7 — đã đối chiếu code hiện tại trong phiên này, khớp từng vị trí)

| # | Mức | ID | Vị trí | Mô tả & hệ quả |
|---|---|---|---|---|
| 1 | **high** | approval-bypass/command-classifier | `src/risk.ts:137-141` (nhánh backslash trong `scanShell`), `src/risk.ts:212` (tokenize), `src/risk.ts:322-332` (`classifyRm`) | Bash line-continuation (backslash + newline) và ANSI-C quoting `$'...'` khiến cờ recursive/force của `rm` không được nhận diện. Nhánh backslash chạy **trước** nhánh tách `\n` nên gộp `\`+`\n` vào token hiện tại → tokenize ra token `"\n-rf"` (không bắt đầu bằng `-` → không tính là flag; `classifyRm:323` lọc `startsWith('-')` — đã xác minh tại dòng 323). `$'\x2drf'` được tokenize như single-quote literal trong khi bash thực tế decode escape. Cả hai làm `classifyRm` mất `-rf` → phân loại thấp hơn thực tế. |
| 2 | **medium** | auto-mode/package-manager-script (known issue 7) | `src/risk.ts:361-377` (`classifyPackageManager`, `SAFE_SCRIPT_NAMES`), `src/approval.ts:83` (`canAutoApprove`), `src/cli.ts:1094-1100` | `npm run <script-bất-kỳ>` phân loại medium (chỉ script trong `SAFE_SCRIPT_NAMES` là low — xác minh dòng 367-371). Auto mode tự duyệt medium (`canAutoApprove:84` chỉ chặn high/critical — xác minh), và `assessCommandNetwork` trả NO_NETWORK cho `npm run` nên policy network không thấy. Đã xác nhận runtime: `npm run deploy` → medium; `npm test` → low. `node exfil.js` → medium là cùng lớp known gap đã ghi trong plan §11. |
| 3 | **medium** | policy-gap/verify-commands | `src/verify.ts:594-626`, `src/harness.ts:964-968`, `src/cli.ts:1659-1664`, `src/cli.ts:1094-1097`, `src/approval.ts:83` | Verify plan commands chạy qua `runner.run` trực tiếp (`verify.ts:609` — xác minh), không qua `classifyBashRisk` lẫn `isNetworkAllowedForCommand`. Hệ quả: (a) `verify.commands` do user cấu hình (vd `curl https://...` hoặc `!curl ...` qua `bash -c`) chạy được cả khi execution policy đang `deny` network — invariant "network default-deny chặn mọi bash phân loại network trước approval" không áp dụng cho verify; (b) tool `verify` không có `risk` trong approve request (xác minh `harness.ts:965-968` và `cli.ts:1659-1664`) nên `cli.ts:1094-1098` mặc định risk='medium' → trong auto mode toàn bộ verify plan (kể cả `npm run test/build/lint` — script do repo kiểm soát) tự chạy không giám sát; (c) `sessionAllowedTools` có thể chứa 'verify' sau khi user nhấn 'a' một lần. |
| 4 | low | TUI-terminal-injection | `src/cli.ts:1053-1062` (renderEvent: `verify_start`/`verify_result`/`verify_rollback`/`dag_plan_invalid`/… ghi stderr raw) | Ở plain mode (`--no-tui`), `verify_start.commands` (display string lấy từ package.json scripts của repo qua `planVerification`), `dag_plan_invalid.message` (lỗi planner — có thể chứa output của model), `verify_result.reason` ghi stderr không qua `cleanTerminal`/`terminalText`. Đường TUI đã ĐẠT (`src/tui.ts:945-947` strip VT); finding chỉ còn đường stderr plain-mode. |
| 5 | low | robustness/latent-infinite-loop | `src/files.ts:104-106` (`limitTextBytes` defensive loop) | Vòng `while (Buffer.byteLength(result) > maxBytes && tail.length > 0)` tính lại `result` từ cùng biến `tail` không đổi (luôn `tail.slice(0,-1)` — xác minh dòng 105) → nếu điều kiện bao giờ đúng sẽ lặp vô hạn. Với toán hiện tại (head+tail ≤ budget = maxBytes−128, marker+digits < 128) điều kiện không bao giờ đúng — dead code tiềm ẩn, không trigger được với input hiện tại. |
| 6 | low | secret-persistence/session | `src/tools.ts` (`toolApprovalSignature` — `stableStringify` toàn bộ input), `src/cli.ts:1228-1240` (`rejectionGuard` persist vào `session.rejectedSignatures`) | Signature bị deny lưu nguyên văn `tool:<JSON input>` vào session JSON qua `store.save()` (xác minh `cli.ts:1228-1233`). Một bash command chứa secret (vd `curl -H "Authorization: Bearer sk-..."`) bị deny sẽ persist command đó **không redact** vào `sessions/<id>.json`; map cũng tăng trưởng không giới hạn theo số signature khác nhau. File session 0600 local, nhưng nhất quán với mục tiêu redaction tổng thể thì đây là lỗ hổng residual. |
| 7 | low | TOCTOU/path-traversal/RAG | `src/rag.ts:443-459` (`walkRepo`: readdir + lstat skip symlink + realpath trong root) vs `src/rag.ts:525` (`indexRepo` `readFile(file.absolutePath)` — follow symlink) | Cửa sổ TOCTOU: file bị thay bằng symlink ra ngoài root giữa lúc walk và lúc đọc sẽ được `readFile` theo symlink (xác minh cả hai vị trí). Chỉ xảy ra nếu attacker local ghi được vào workspace giữa hai bước; sensitive-path filter/redaction vẫn áp lên nội dung đọc được. |

**Tình trạng xác nhận:** cả 7 findings đều là xác nhận từ audit (finding #2 có probe runtime: `npm run deploy` → medium, `npm test` → low, chạy từ bản compile tạm ở `%TEMP%/sec-audit`, không sửa file repo). Tôi đã đối chiếu lại từng vị trí file:line trên tree hiện tại trong phiên này — **khớp hết**. Chưa có finding nào được fix.

### 6.2 Các nhóm bảo mật đã kiểm và ĐẠT (từ đợt audit bảo mật)

| Nhóm | Kết luận | Bằng chứng chính (file:line theo audit) |
|---|---|---|
| Secret/env con | ĐẠT | `childEnv` xoá `GOON_API_KEY` + keyEnv khỏi mọi spawn (`src/process.ts:12-20`; `runCommand` luôn spawn với childEnv :55); verify runner dùng `runCommand` (`src/verify.ts:691-713`); embedder không spawn, key chỉ trong Authorization header (`src/embedder.ts:127-133`), lỗi scrub key trước khi surface (:114-124); stub không fetch/đọc credential, resolveRuntime stub trả `apiKey=''` |
| Redaction | ĐẠT | EventLogWriter merge `collectSecrets()`+apiKey, redactObject từng envelope, StreamRedactor giữ tail 256 ký tự, flush ở done/error/usage/close (`src/eventlog.ts:316-451`); log 0600 + fchmod, sidecar 0600 atomic rename (`eventlog.ts:180-189, 352-363`); `--json` stdout redact; RAG `redactSecrets` trước chunk (`src/rag.ts:531-533`); MemoryStore merge collectSecrets (`src/memory.ts:629`), preference redact (`memory.ts:717-721`); verdict redact trước emit/persist (`src/harness.ts:999, 1012-1018`); scope-loop redact verdict (`src/coord.ts:875-878`); tool event chạm sensitive path bị strip diff/outputPreview (`eventlog.ts:267-313`) |
| Approval | ĐẠT | critical denied trước approve kể cả auto (`src/tools.ts:806-808`); high không bao giờ auto/always/exact/prefix, prompt buộc gõ tên tool (`src/cli.ts:1095-1105, 1226`); exact allowlist chỉ nhận command không quote/meta (`src/approval.ts` `bashApprovalExact`); prefix approval giữ SHELL_META_RE, normalize .exe/.cmd (`approval.ts:56-70`); rejection loop 3 lần identical bền theo session (`src/tools.ts` deny + `src/cli.ts:1228-1240`); workflow shell: critical throw trước approval, thiếu `approveShell` → `WorkflowApprovalError`, network gate trước approval (`src/workflow.ts:741-768`) |
| MCP trust/annotations | ĐẠT | trust không còn set readOnly (`src/mcp.ts:864-871`); annotation chỉ escalate — readOnlyHint floor medium, destructive/openWorld → high, trust không hạ (`src/risk.ts:493-520`); net gate trước approval (`src/tools.ts:810-823`); attach truyền đủ risk/net/trusted (`src/mcp.ts:950-966`) |
| Subagent self-escalation | ĐẠT | `isExposed` enforced trong `execute()` (`src/tools.ts:789-792`); child registry pin allowedTools (`src/subagents.ts` runTasks/child); DAG role toolset + MCP chỉ qua config `multiAgentMcpTools` (`src/coord.ts:56-67`) |
| Patch/path traversal | ĐẠT | stale revision check (`src/files.ts:559-577`), hunk ambiguous reject (`files.ts:295-320`), `validateSyntax` trước commit — patch atomic về lỗi syntax (`files.ts:615-625`), deletion tombstone + conflict giữ file user (`files.ts:654-709`); `replaceTurnBackups` giữ key posix |
| Event log poisoning/replay | ĐẠT | torn-tail repair truncate dòng cuối không parse (`eventlog.ts:221-258`); seq tăng dần bắt buộc, gap chỉ warning (`eventlog.ts:486-503`); `replay --rebuild-session` sanitize id `[^A-Za-z0-9._-]→_` nên không traversal (`src/cli.ts` replayCommand); `deleteSession` xóa log + sidecar + tmp (`src/session.ts:314-327`). Log chỉ main session, user event log-only |
| Cost cap/overflow | ĐẠT | fail-closed `CostPricingError` khi maxCostUsd>0 thiếu giá (`src/cost.ts:140-150`); cap check cost/time với reason rõ (`cost.ts:210-227`); `Number.isFinite` guard, Math.max 0; budget mirror không double-count child (harness accountUsage + network/subagents aggregate delta per-child-session) |
| Command wrapper peeling / network policy / process kill | ĐẠT | wrapper peeling sudo/env/time/timeout/xargs/setsid với floor high cho sudo/doas/xargs (`src/risk.ts:246-303`); policy host extraction fail-closed, redirect flags (`--proxy`/`--resolve`/`-o ProxyCommand`/`-c git`) → unknown → deny (`src/policy.ts:118-127, 143-154`); POSIX kill process group (−pid), Windows `taskkill /T /F` + fallback (`src/process.ts:61-74`); output cap head+tail 64 KB / hard 4 MB kill; timeout clamp 1–600 s (tools.ts bash, workflow.ts shell, policy.limits) |
| TUI escape | ĐẠT ở đường TUI | event text qua `terminalText` (strip VT) — `src/tui.ts:945-947`; finding #4 chỉ còn đường stderr plain-mode |
| Pair notify | ĐẠT | `pair.ts` notifyEvent chỉ gửi summary giới hạn (reason 200 chars, preview 2000 chars đã cleanTerminal + redact apiKey) — không thấy secret gửi ra thiết bị pair |
| TOCTOU khác | ĐẠT | patch path dùng revisions/digest stale-check; event log 2-writer race (2 CLI cùng session) có thể xen kẽ dòng — tự gây ra, chấp nhận; RAG TOCTOU window còn lại xem finding #7 |

### 6.3 SSRF — ràng buộc và kết luận

- **Kết luận: N/A — không có URL fetch mới phát sinh từ input không tin cậy.**
- Đã kiểm: provider baseUrl do user cấu hình (`normalizeGatewayUrl` không đổi); pair/cloudflared user-initiated; MCP HTTP servers user cấu hình; embedder baseUrl user cấu hình (`embedder.ts:189-192`), ollama mặc định `127.0.0.1` (local, không phải SSRF từ untrusted input); stub không network; `policy.ts` chỉ là classifier/gate, không fetch.
- Theo tiêu chí đề bài: base URL user tự cấu hình cho provider local **không tính là SSRF**. Ghi chú: trong tree hiện tại không có code tự fetch URL từ nội dung model/tool output, nên ràng buộc SSRF (chỉ http/https, chặn localhost/loopback/private/reserved trước khi request) chưa phát sinh điểm cần áp dụng; nếu P25+ thêm tính năng fetch từ untrusted input thì phải áp ràng buộc này.

### 6.4 Residual risk đã ghi nhận (không tính finding)

- Session messages lưu verify error text chỉ redact apiKey + pattern `key=value`/bearer — env literal trần (không có prefix `key=`) có thể nằm trong session JSON; nhất quán với tiền lệ raw tool output cũ (file 0600 local).
- Event log 2-writer race (2 CLI cùng session) — tự gây ra, chấp nhận.
- `unused/` không nằm trong `tsconfig include` (chỉ `src/**`) — không build, không audit sâu.

---

## 7. Hạn chế

### 7.1 Roadmap P25–P27 chưa triển khai
Theo `docs/PLAN-yolo-port.md:819`: **P25–P27** = OS sandbox, refreshable price table, full session restore, OTel, retention — mức **L/–**, "roadmap, chia PR khi cần; DoD xác định khi scope được duyệt". Chưa làm gì trong các đợt P01–P24 này.

### 7.2 Termux thật chưa kiểm chứng
Đã thêm `.gitattributes` + `scripts/smoke-termux.sh` và CI matrix 3 OS × 2 Node, nhưng **chưa chạy trên thiết bị Termux thật**. Smoke script mới chỉ tồn tại, chưa có kết quả chạy ghi nhận.

### 7.3 Performance chưa benchmark
RAG index (chunkFile/LexicalIndex), HashEmbedder, MemoryStore retrieve, context-budget allocation, DAG scheduler — chưa có benchmark số liệu nào được ghi nhận. Đã kiểm đúng-y-câu-hỏi: không tìm thấy dữ liệu perf trong biên bản các đợt.

### 7.4 Release pack fail (cần xử lý trước khi phát hành)
`npm run release:pack` (→ `node scripts/pack-release.mjs`, `package.json:38`) **exit 1** với `Error: spawn npm ENOENT` (errno −4058, spawnargs `['run','build']`, Node.js v22.14.0). Nguyên nhân kỹ thuật: `scripts/pack-release.mjs:21-28` spawn `npm` trực tiếp qua child_process; trên Windows spawn('npm') không `shell: true` không resolve `npm.cmd`. Đây là lỗi môi trường/script pack — build và test không liên quan (typecheck/lint/build đều pass ở các đợt). Phương án: chạy pack trên POSIX/CI, hoặc sửa script spawn `npm.cmd`/dùng `shell: true` trên Windows, hoặc dùng `process.execPath` + npx-cli path. **Nếu cần pack offline:** script pack còn chạy `npm pack` + `npm install --prefix` để smoke test (`pack-release.mjs:22, 28`) — cả hai đều cần npm hoạt động; chưa kiểm kịch bản offline, và hiện tại pack đã fail ở bước spawn nên offline chưa đánh giá được.

### 7.5 Full test còn 1 failure
Baseline 3 failures → cuối 1 failure. Tên test/cấu hình gây failure **không có trong dữ liệu bàn giao** — cần xem log lần chạy full cuối. Trước khi phát hành phải đưa về 0.

### 7.6 Các việc đã chủ động không làm (theo ràng buộc workflow)
Không commit/push; không sửa `docs/PLAN-yolo-port.md`, `.mimosa/`, `.zcode/`; một số đợt không chạy full `npm test` (script workflow tự chạy); audit không chạy typecheck/lint/build/release:pack (workflow tự chạy); `tests/e2e/cli-basics` + `verify-loop` audit không chạy riêng.

---

## 8. Cách verify (tái hiện kết quả)

```bash
# 0) Trạng thái chưa commit
git status --short          # kỳ vọng: 78 mục (18 M src + module/test mới untracked)
git log --oneline -1        # kỳ vọng: eb7839a (v0.3.1) — không có commit mới

# 1) Gate
npm run typecheck           # kỳ vọng: exit 0
npm run lint                # kỳ vọng: 0 warnings / 0 errors
npm run build               # kỳ vọng: exit 0

# 2) Full suite (hiện tại còn 1 failure — KHÔNG kỳ vọng 0)
npm test

# 3) Targeted theo nhóm đã chạy trong đợt audit (252 tests / 0 fail):
node --test tests/risk.test.mjs tests/eventlog.test.mjs tests/verify.test.mjs tests/cost.test.mjs tests/coord.test.mjs
node --test tests/mcp.test.mjs tests/files.test.mjs tests/policy.test.mjs
node --test tests/harness-verify.test.mjs tests/subagents.test.mjs tests/scope-loop.test.mjs tests/workflow-classifier.test.mjs
node --test tests/memory.test.mjs tests/rag.test.mjs tests/context-budget.test.mjs tests/stub-provider.test.mjs tests/workflow.test.mjs

# 4) E2E
node --test tests/e2e/

# 5) Release pack (hiện tại FAIL trên Windows do spawn npm ENOENT)
npm run release:pack        # kỳ vọng hiện tại: exit 1, spawn npm ENOENT (xem §7.4)

# 6) Tái hiện security findings theo vị trí trong §6.1, ví dụ:
#    - finding #2: cấu hình auto mode, chạy `npm run deploy` (script bất kỳ) → được auto-approve mà không qua network policy
#    - finding #3: cấu hình verify.commands chứa `curl https://...`, đặt --allow-net deny → verify vẫn chạy command
#    - finding #6: deny một bash command chứa secret, mở sessions/<id>.json → thấy signature chưa redact
```

Chức năng chính để kiểm thủ công như người dùng: `goon` với `--event-log` → `goon replay` (raw/`--envelopes`/`--rebuild-session`); `/memory rag index|stats|search`, `/memory pref …`; `/dag`, `goon dag <goal>`; `/cost`, `/status`; `--allow-net-hosts`; stub provider (`GOON_STUB*`, doctor báo `stub:true`); `/workflow` với classifier + scope loop + multi-candidate.

---

## 9. Trạng thái Git — CHƯA COMMIT

- Nhánh `fix/security-audit-v0.3.1`, HEAD `eb7839a` — **không có commit mới nào** cho toàn bộ công việc v0.4→v0.7 (xác minh bằng `git log` + `git status --short` trong phiên này: 78 mục thay đổi).
- Thành phần chưa commit: 23 file sửa (src, tests, README, CHANGELOG, docs/MCP.md, package.json, `.github/workflows/release.yml`) + các file/module mới (15 module `src/`, ~30 file test + fixtures + golden + e2e, `.gitattributes`, `scripts/smoke-termux.sh`, `docs/PLAN-yolo-port.md`) + thư mục không audit (`unused/`).
- **Việc tiếp theo bắt buộc trước release:** (1) fix 1 failure còn lại của full suite; (2) xử lý findings #1 (high) và #2, #3 (medium); (3) sửa/chạy lại release:pack trên môi trường đúng; (4) commit có chủ đích theo đợt, sau đó mới PR.
