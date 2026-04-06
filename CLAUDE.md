# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                              # Debug build
cargo build --release                    # Release build
cargo test                               # Run all tests
cargo test <test_name>                   # Run a single test
cargo test -- --nocapture               # Run tests with stdout
make test                                # cargo nextest run --all-features
cargo clippy --all-targets --all-features
cargo fmt
cargo doc --open
```

Requires Rust 1.90+ (edition 2024). The `testing` feature gates test utilities.

## Architecture

A Rust SDK for bidirectional streaming communication with the Claude Code CLI subprocess. The SDK wraps the CLI process over stdio and exposes async Rust APIs.

**Three public query APIs:**
- `query()` / `query_stream()` — one-shot convenience functions in `src/query.rs`
- `ClaudeClient` (`src/client.rs`) — stateful bidirectional client with `connect()` / `query()` / `receive_response()` / `disconnect()`

**Layered internally:**
```
query.rs / client.rs  →  InternalClient (src/internal/client.rs)
                       →  SubprocessTransport (src/internal/transport/)
                       →  Claude Code CLI process (stdio)
```

`SubprocessTransport` implements the `Transport` trait, making the transport pluggable (used by the `testing` feature to inject mock transports).

**Configuration** is built via `ClaudeAgentOptions::builder()` (typed-builder) in `src/types/config.rs`. It covers model, tools, MCP servers, hooks, permissions, sessions, efficiency, and plugins.

**Message types** live in `src/types/messages.rs`. The `Message` enum has variants: `Assistant`, `User`, `System`, `Result`, `StreamEvent`, `RateLimitEvent`, `ControlCancelRequest`, `Unknown`. Content is represented via `ContentBlock` (for assistant messages) and `UserContentBlock` (for user input including multimodal images).

**Hooks** (`src/types/hooks.rs`) intercept six lifecycle events: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `PreCompact`. Registered via `HookMatcher` with optional tool-name patterns.

**Permissions** (`src/types/permissions.rs`) use `CanUseToolCallback` for dynamic allow/deny decisions and `PermissionUpdate` for runtime permission rule changes.

**MCP integration** (`src/types/mcp.rs`) supports in-process SDK servers (via `SdkMcpServer` trait and `SdkMcpTool`) and external Stdio/SSE/HTTP servers. In-process tools follow the naming convention `mcp__{server_name}__{tool_name}`.

**Session management** (`src/sessions.rs`, `src/types/sessions.rs`) supports resuming, forking, and mutating sessions. File checkpointing uses UUIDs for rewind support.

**Concurrency:** Lock-free design using `DashMap`, `flume` channels, and `AtomicBool`. The only `Mutex` is on subprocess stdin/stdout. Goal is zero deadlocks.

**Testing utilities** (feature = `"testing"`): mock transport, message/result builders, hook/permission recorders, scenario framework — all in `src/testing/`.

See `examples/` (01–24) for runnable demonstrations of every major feature.
