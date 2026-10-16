# Stream and subagent review — 0.2.1

Reviewed on 2026-10-16 against pinned public sources:

- ZCode `aac4755666d09fdcd70272fcf063c077a639015f` (current main).
- DeepSeek Harness `1517a4dbca0a2789ff1d41ba439600f8547d23fa`
  (current master, dsh-v0.2.1-alpha.5). The four DSH implementation files below
  are unchanged from the earlier inspected `86a402200d95fecef4529d969836f1fe13c6cb0d`.

This is a design review and an independent implementation, not a runtime import.

## Sources and decisions

| Boundary | Reference implementation inspected | Goon decision |
|---|---|---|
| Provider reasoning vs answer | ZCode [runner-normalization.ts](https://github.com/zai-org/ZCode/blob/aac4755666d09fdcd70272fcf063c077a639015f/apps/zcode-cli/packages/adapters/src/model/runner-normalization.ts) maps text/reasoning start, delta and end independently. DSH [translate.ts](https://github.com/deepseek-ai/deepseek-harness/blob/1517a4dbca0a2789ff1d41ba439600f8547d23fa/packages/llm/llm-deepseek/src/translate.ts) separates text/thinking and retains reasoning signatures in replay metadata. | Keep SDK response messages unchanged. Project visible answers separately. Native reasoning has its own opt-in event, never a text event. |
| Background delta cost | ZCode [zcodeSessionEventCoalescer.ts](https://github.com/zai-org/ZCode/blob/aac4755666d09fdcd70272fcf063c077a639015f/packages/services/src/zcode-agent/zcodeSessionEventCoalescer.ts) batches reconstructible deltas and flushes before controls. DSH [assistant-stream.ts](https://github.com/deepseek-ai/deepseek-harness/blob/1517a4dbca0a2789ff1d41ba439600f8547d23fa/packages/llm/llm/src/assistant-stream.ts) packs adjacent deltas without repeating event envelopes. | The parent does not display child text, so its progress channel omits child text/reasoning entirely. No timer, token batching or repeated full task payload is needed there. Completed child messages remain in child sessions. |
| Lifecycle vs presentation | ZCode [product-projection.ts](https://github.com/zai-org/ZCode/blob/aac4755666d09fdcd70272fcf063c077a639015f/apps/zcode-cli/packages/bootstrap/src/zcode-protocol-v4/product-projection.ts) has dedicated subagent rows and lifecycle identities. DSH [manager.ts](https://github.com/deepseek-ai/deepseek-harness/blob/1517a4dbca0a2789ff1d41ba439600f8547d23fa/packages/subagent/subagent/src/manager.ts) captures results, disposes resources, releases capacity, then notifies settlement. | Separate start/progress/terminal event variants. Only start carries task. Done requires successful session close; an error follows failed cleanup. Capacity is reclaimed even if close throws. |
| Child report selection | DSH [assistant-output.ts](https://github.com/deepseek-ai/deepseek-harness/blob/1517a4dbca0a2789ff1d41ba439600f8547d23fa/packages/subagent/subagent/src/assistant-output.ts) selects the last non-empty assistant message rather than every narration delta. | A child returns its last non-empty visible assistant answer. Tool preambles and reasoning do not become the parent's report. The public Harness.run aggregate text return remains unchanged. |

Additional ZCode contracts inspected: `contracts/src/model/index.ts`,
`docs/model-tool-call-stream-commit-boundary.md`,
`docs/performance/streaming-tool-input-backpressure.md`,
`docs/subagent-respond-to-coordinator.md`. DSH's block assembler, headless
reasoning surface and subagent capability/result types were also inspected.

## Inline think compatibility is a Goon policy

Neither reference above proves that arbitrary `<think>` strings should be
removed from all text. They provide typed reasoning boundaries for protocols
that supply them. A gateway that inlines these tags needs a compatibility rule.

Goon's default `leading-think` presentation policy recognizes one or more leading
case-insensitive `<think>...</think>` blocks, permitting leading whitespace and
nested markers. It holds marker suffixes across arbitrary SSE chunks, does not
scan accumulated reasoning again, and keeps an unclosed block in the reasoning
channel. Once visible answer text starts, later tags are literal. Code fences,
inline code and prose containing think tags therefore remain intact.

A bare literal think element at the very start is ambiguous. Use
`--inline-reasoning off` (or GOON_INLINE_REASONING=off) for that endpoint/task.
This rule is a display compatibility heuristic, not a semantic reasoning detector
or secret scrubber. It does not claim to recognize every gateway's tag format.

The installed AI SDK's `extractReasoningMiddleware` was inspected as well.
Applying a model middleware would change assembled response content and replay
shape. Goon instead applies its projection to emitted/returned text, child
reports and generated compaction summaries. Original completed provider messages,
native reasoning and opaque metadata stay available for the next tool step or
resume. Failed/aborted streams retain Goon's existing partial-transcript limits;
Goon does not implement either upstream's complete durable token event journal.

`--show-reasoning` opts into separate `reasoning` NDJSON events or labeled
stderr/TUI output. The default emits neither native nor extracted reasoning.
Child reasoning is never mirrored into parent progress, including with this
option. Terminal responses without a non-whitespace projected answer fail with
an explicit missing-answer error;
truncated/cancelled streams cannot execute tool calls or flush hidden text as an
answer. Summary generation also rejects a reasoning-only summary before replacing
active history.

## Terminal-answer correction in 0.2.2

The 0.2.1 implementation enforced its missing-answer error only after observing
a reasoning delta or a recognized inline think block. That did not implement the
full intended contract. Empty/whitespace responses, opaque gateway fields ignored
by the compatible adapter, and Anthropic redacted thinking without text deltas
could finish successfully with an empty answer. A parent could do so after
successful child completion as well.

0.2.2 validates the terminal step's projected answer directly, without requiring
any evidence of reasoning. Tool-only intermediate steps are valid; prior tool
narration does not substitute for a final answer. Missing-answer failures retain
usage and completed tool history, release locks, and do not trigger automatic
model retries or replay side effects. Permission denial remains an explicit
stopped/denied outcome (exit 2), not successful completion.

The new tests reproduce these response shapes through the real SDK and CLI,
including redacted thinking that emits reasoning start/end but no delta. The
redacted payload remains in SDK response metadata; unsupported gateway fields
are not reconstructed or claimed to be durably preserved. These controlled
fixtures establish a guard defect; without the user's raw gateway trace they
do not identify the exact payload of the reported claude-sonnet-5-5 run.

## NDJSON migration from 0.2.0

A 0.2.0 subagent wrapper overloaded state=start for every child harness event.
In 0.2.1 consumers should discriminate state first:

```json
{"type":"subagent","id":"CHILD","state":"start","task":"Inspect one file"}
{"type":"subagent","id":"CHILD","state":"progress","event":{"type":"tool","name":"read_file","id":"CALL","state":"start"}}
{"type":"subagent","id":"CHILD","state":"progress","event":{"type":"usage","input":100,"output":10,"total":110}}
{"type":"subagent","id":"CHILD","state":"done"}
```

Progress carries only tool, usage, compaction or stopped events. Task is present
only at start; done/error have no nested event. Each started child emits exactly
one terminal event after the cleanup attempt; done requires successful close. Errors opening a session do not invent
a started child id. Tool approvals and cancellations remain immediate controls,
not throttled text. Start/terminal state tracking no longer needs to inspect an
optional nested event to distinguish tokens from lifecycle.

## Verification and remaining scope

A separate before/after CLI comparison used the released 0.2.0 executable and
this implementation with identical mock-provider responses: two children, each
emitting 256 one-character answer deltas, with a 150-word task context each.
Both exited successfully. Only parent subagent events are counted:

| Measurement | Released 0.2.0 | Fix |
|---|---:|---:|
| Subagent events | 518 | 6 |
| NDJSON bytes, including newline separators | 689109 | 3037 |
| Events carrying task | 518 | 2 |

These numbers describe this controlled fixture, not a paid-model benchmark.
The tool-bearing regression below is a different fixture with twelve events.

The regression fixture streams 100 repeated tool-preamble words per child, one
character per delta, plus native/inline reasoning and a final report. Two children
produce exactly twelve parent subagent events: each has start, two usage events,
two tool lifecycle events and done. Increasing narration token count adds no
parent subagent events. The parent receives only final reports. Tests separately
inject close failures and verify terminal ordering, lock cleanup and capacity
reuse, as well as every split point in think markers, literal code, resume,
compaction, cancellation, truncation and opt-in rendering.

Goon still has no durable event replay cursor, coordinator inbox, continuable
child catalog, adaptive provider retry ledger or compact per-token audit journal.
Copying those systems piecemeal would add ownership and recovery contracts that
this implementation does not yet provide. Parent text streaming and the TUI's
bounded scrollback are unchanged; phone-side rendering performance still needs
measurement on an actual Termux device.
