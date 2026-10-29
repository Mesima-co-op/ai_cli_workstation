# aiws: AI CLI workstation

One Rust program to **use, monitor and combine AI models from the terminal**: a CLI, a terminal UI and an HTTP daemon, on your PC, over SSH or in Docker.

- **One entry point** for Claude Code (your subscription), the Anthropic API, OpenAI-compatible APIs (including Ollama) and Hermes Agent.
- **Monitoring**: every call logged with tokens, cost, latency and routing reason; daily and monthly budgets.
- **Terminal UI**: dashboard, request inspector, chat with live streaming, settings editor, themes.
- **Multi-model pipeline**: Jev (or a built-in heuristic) decides, Hermes plans, Claude executes, and anything not set up is skipped automatically.
- **Hermes agent runs**: live tool progress and approval prompts.
- **Private key store**: save keys from the TUI or `aiws key set`; no shell setup, nothing in the config file.

## Quick start

```powershell
cargo build --release
.\target\release\aiws.exe init
.\target\release\aiws.exe check      # what is installed, what to install
.\target\release\aiws.exe tui
.\target\release\aiws.exe chat "hello"
```

Requires Rust (stable) and a C/C++ build toolchain. `aiws check` tells you which models and tools are available on your machine; only the Claude Code provider is enabled by default.

## Documentation

Full guides live in the **[wiki](https://github.com/Mesima-co-op/ai_cli_workstation/wiki)**:

| | |
|---|---|
| [Getting Started](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Getting-Started) | install, build, first run |
| [Providers](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Providers) · [Hermes](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Hermes-Setup) · [Jev](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Jev-Setup) | connecting models |
| [Pipeline](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Pipeline) | decide, plan, execute and the fallbacks |
| [TUI Guide](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/TUI-Guide) · [Daemon and API](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Daemon-and-API) | using it |
| [Configuration Reference](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Configuration-Reference) · [Troubleshooting](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Troubleshooting) | reference |
| [Development](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Development) · [Git Workflow](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Git-Workflow) | contributing |

## Status

Tested against mock servers: streaming, Hermes agent runs with approvals, the pipeline and its fallbacks, the key store and the TUI. Not yet verified against real services: Jev (its API is not public, so the client is an assumption), a live Hermes server, and the Anthropic API with a real key.

## Security

- Keys are never written to `config.toml`, the log or messages. See [Keys and Security](https://github.com/Mesima-co-op/ai_cli_workstation/wiki/Keys-and-Security).
- The daemon refuses to listen on a non-loopback address without a token.
- If you find a vulnerability, please open a private security advisory on GitHub instead of a public issue.

## License

MIT, see [LICENSE](LICENSE).
