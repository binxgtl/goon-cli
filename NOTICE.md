# Project identity and references

Goon CLI is an independent implementation. It is not an official Z.ai,
DeepSeek, Anthropic or OpenAI product. No ZCode or DeepSeek Harness runtime
source is copied into this distribution or used as a dependency.

Design references inspected for version 0.1.0:

- [ZCode](https://github.com/zai-org/ZCode), commit
  `aac4755666d09fdcd70272fcf063c077a639015f` (Apache-2.0): separation of
  model/provider, runtime, permission, tool and persistence boundaries;
  explicit ownership of session history and configuration.
- [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness), commit
  `86a402200d95fecef4529d969836f1fe13c6cb0d` (MIT): replaceable capability
  interfaces, bounded parallel-safe tool scheduling, exclusive mutation
  barriers, ordered results and conservative recovery after interruption.

Goon is written in Rust and implements the model protocols itself. The release
binaries statically link third-party crates under their own licenses (MIT,
Apache-2.0, ISC, BSD-3-Clause, Zlib, Unicode-3.0, CDLA-Permissive-2.0 and
similar); `THIRD-PARTY-NOTICES.txt`, shipped in the npm package and attached to
every release, lists each crate with its license texts. Versions before the Rust
rewrite used the Vercel AI SDK and its provider adapters for protocol handling.
The implementation and build process use no official Claude Code binary.

A deeper stream/subagent review for 0.2.1 rechecked ZCode at the same commit
and DeepSeek Harness at `1517a4dbca0a2789ff1d41ba439600f8547d23fa`.
See [docs/REFERENCE_REVIEW.md](docs/REFERENCE_REVIEW.md) for exact source links,
contracts adopted, independent compatibility choices and remaining scope.
