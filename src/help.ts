import { VERSION } from './version.js';

// Kept apart from cli.ts so `goon --help` does not load the AI SDK, TUI or tool registry.
export const HELP = `goon cli ${VERSION} — independent coding harness

Usage: goon [options] [prompt]
       goon resume [ID|latest|number]
       goon pair
       goon workflow list | show <name> | init <name> [desc] | rm <name> | run <name> [args...]
       goon network <topic>
       goon setup
       goon init --provider openai-compatible --base-url URL --model MODEL [--effort LEVEL]
       goon doctor | sessions
       goon mcp list | check [name] | get <name> | add <name> ... | remove <name> | import-codex [name...]

Options:
  -p, --prompt TEXT          Run one task (or pipe a prompt on stdin)
  -C, --cwd DIRECTORY        Workspace (defaults to current directory)
      --provider NAME        openai-compatible | openai-responses | anthropic
      --base-url URL         Provider API root, usually ending in /v1
      --model NAME           Exact model ID from your provider
      --effort LEVEL         Reasoning effort: low | medium | high | xhigh | max
      --key-env NAME         Credential variable (default GOON_API_KEY)
      --resume ID|latest     Resume a saved session in this workspace
      --pair                Start Cloudflared tunnel + QR device pairing on launch
      --yes                 Approve file changes and shell commands automatically
      --read-only           Read tools and research agents; no writes or shell
      --json                Emit NDJSON events instead of terminal output
      --no-tui              Use plain readline instead of the full-screen UI
      --no-mcp              Disable configured MCP servers for this run
      --no-subagents        Disable delegation tools for this run
      --inline-reasoning MODE leading-think (default) | off
      --show-reasoning      Separate reasoning events / labeled stderr output
      --context-window N    Actual model context window (default 128000)
      --max-output-tokens N  Per-request output limit (default 16384)
      --run-token-budget N   Run token budget (default 0 = unlimited)
      --timeout-ms N        Model request deadline (default 300000)
  -o, --output-last-message FILE  Also write the final answer of a -p run to FILE
  -h, --help                Show help

Piping: with -p, text piped on stdin is attached as context, e.g.
  git diff | goon -p 'review this diff'      goon -p 'explain' < error.log
  -v, --version             Show version

Interactive commands:
  /help, /setup, /key, /model, /effort, /mode, /auto, /view, /transcript, /copy, /btw,
  /goal, /recap, /skills, /prompts, /memories, /presets, /mention, /pair, /workflow, /network,
  /cd, /pwd, /mcp, /subagents, /plan, /revert, /undo, /checkpoints, /todo, /compact, /diff, /review, /fork, /rename,
  /delete, /export, /init, /status, /tools, /sessions, /debug-config, /warnings, /doctor, /clear, /exit, or !<cmd>.
Shortcuts: Shift+Tab mode, Ctrl+T transcript, Ctrl+O copy, Alt+. / Alt+, effort, Tab autocomplete, Esc Esc rewind, Ctrl+C cancel.
API keys are read from environment variables or ~/.config/goon/credentials.json.
Shell commands run as your OS user; approvals are not an OS sandbox.
`;
