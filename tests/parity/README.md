# TypeScript parity corpus

`corpus.jsonl` records about 10,000 calls to the pure functions of the TypeScript implementation
(module, function, arguments and the result or thrown error). `tests/parity.rs` replays every record
against the Rust port and compares the results. The only allowed differences are the deliberate bug
fixes listed in `DIVERGENCES` in that file.

The corpus was captured from the TypeScript sources as of commit `ce60f0c`, which stayed unchanged
through release v0.3.3, the last TypeScript release. Those sources are no longer in the tree, so the
corpus is now a frozen reference. To capture it again, check out `v0.3.3` and run:

```bash
npm ci && npm run build
node tests/parity/instrument.mjs dist dist-instrumented && rm -rf dist && mv dist-instrumented dist
PARITY_OUT=/tmp/calls.jsonl node --test "tests/*.test.mjs"
PARITY_OUT=/tmp/calls.jsonl node tests/parity/synth.mjs
python3 tests/parity/dedupe.py /tmp/calls.jsonl tests/parity/corpus.jsonl
```

Replay part of it with `PARITY_ONLY=classifyBashRisk cargo test --test parity`, which keeps the
records whose `module.fn` contains the text. `PARITY_SHOW=<n>` prints up to n mismatches (default 40).
