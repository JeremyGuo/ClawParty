#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$REPO_ROOT"

set -a
source .env >/dev/null 2>&1
set +a

: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY must be set after sourcing .env}"

cargo test --manifest-path agent_frame/Cargo.toml --test live_openrouter -- --ignored --nocapture "$@"
