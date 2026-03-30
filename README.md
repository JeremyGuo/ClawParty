# ClawParty 2.0

ClawParty 2.0 is a Rust-based multi-agent host around a reusable agent runtime.

The repository is split into two layers:

- `agent_frame/`: a standalone agent runtime with tools, skills, context compaction, and CLI/SDK entrypoints
- `agent_host/`: a long-running host that manages channels, sessions, foreground agents, background agents, subagents, cron jobs, logging, and recovery

## Why This Exists

This project is not trying to be just another single-agent chat loop.

It is designed for cases where you need:

- a Telegram bot or CLI endpoint instead of a one-shot terminal agent
- persistent sessions and workdirs
- background tasks and delegated subagents
- cron-triggered work
- structured logs, token accounting, and operational recovery
- a host layer that can survive process restarts and restore runtime state

## How It Differs From OpenClaw

If you think of OpenClaw as the "agent runtime" layer, ClawParty 2.0 is the "host orchestration" layer built around that kind of runtime.

The practical distinction is:

- OpenClaw-style systems focus on a single agent execution environment
- ClawParty 2.0 focuses on running that agent inside a larger service with channels, sessions, background execution, subagents, cron, persistence, and operational safeguards

In this repository:

- `agent_frame` is the closest thing to the raw runtime
- `agent_host` is the differentiator

That means the real distinction is not "better prompts" or "more tools". It is service architecture:

- multi-channel abstraction
- session lifecycle management
- foreground/background/subagent topology
- runtime persistence
- logging and token usage tracking
- failure handling and recovery policy

## Repository Layout

```text
.
├── agent_frame/          # Standalone Rust agent runtime
├── agent_host/           # Long-running host/orchestrator
├── run_test.sh           # Convenience launcher for local configs
├── test_telegram.json    # Local Telegram test config
└── .env.example          # Example environment variables
```

## Key Features

### `agent_frame`

- CLI mode and SDK mode
- built-in tools for file I/O, patching, process execution, web fetch, and web search
- skill discovery and `load_skill`
- tool timeout support
- context compression
- idle context compaction API
- token usage accounting including cache read/write/hit/miss

### `agent_host`

- `Channel` abstraction with current implementations for Telegram and CLI
- persistent `Session` storage and attachment lifecycle
- `Main Foreground Agent`, `Main Background Agent`, and `Sub-Agent`
- background sinks and broadcast routing
- cron task management with optional checker commands
- persistent background/subagent registry across restarts
- structured JSONL logs and agent/session/channel views

## Quick Start

### 1. Prepare environment variables

Create a local `.env` from `.env.example` and add your keys:

```bash
cp .env.example .env
```

Example variables:

```dotenv
OPENROUTER_API_KEY=your_key
TELEGRAM_BOT_TOKEN=your_bot_token
```

### 2. Run tests

```bash
cargo test --manifest-path agent_frame/Cargo.toml
cargo test --manifest-path agent_host/Cargo.toml
```

### 3. Run the host locally

CLI example:

```bash
./run_test.sh agent_host/example_config.json
```

Telegram example:

```bash
./run_test.sh test_telegram.json
```

## Configuration Notes

Model configs support both provider-native web search and an external web search provider.

Examples in this repo include both:

- `native_web_search`
- `external_web_search`

Only one should be effectively active for a given model at runtime:

- if `native_web_search.enabled = true`, `agent_frame` suppresses the standalone `web_search` tool
- otherwise `web_search` is provided via the configured external provider

## Security and Repository Hygiene

This repository intentionally does **not** track:

- `.env`
- generated workdirs such as `*_workdir/`
- logs, sessions, and local runtime state
- build artifacts under `target/`

The goal is that GitHub only receives source, example configs, and automation definitions, not local secrets or live runtime data.

## CI/CD

GitHub Actions are configured for:

- CI on push and pull request
- release artifact builds on version tags like `v0.1.0`

CI runs formatting checks and tests for both Rust crates.

CD builds release binaries for:

- `agent_host`
- `agent_frame`'s `run_agent`

## Status

This codebase is an actively evolving service runtime. The current architecture already covers orchestration, persistence, and observability, but there is still room to harden cancellation and deeper recovery semantics for long-running delegated work.
