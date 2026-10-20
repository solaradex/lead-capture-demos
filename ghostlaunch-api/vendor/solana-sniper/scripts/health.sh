#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
printf 'Project: %s\n' "$PWD"
command -v cargo >/dev/null && cargo --version || { echo 'cargo missing'; exit 1; }
test -f Cargo.toml && echo 'Cargo.toml: OK'
test -f .env.example && echo '.env.example: OK'
test -f fixtures/events.jsonl && echo 'Replay fixture: OK'
grep -q '^EXECUTION_ENABLED=false$' .env.example && echo 'Safe default: OK'
