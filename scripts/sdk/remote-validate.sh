#!/usr/bin/env bash
# No Rust compiler is invoked on the calling computer.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MODE="${1:-sdk}"
case "$MODE" in sdk|regression) ;; *) echo "usage: $0 [sdk|regression]" >&2; exit 2;; esac
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
EVIDENCE="$ROOT/.sdk-evidence/$STAMP-$MODE"
mkdir -p "$EVIDENCE"
SSH=(ssh -o BatchMode=yes -o ForwardAgent=no -o ConnectTimeout=15 WZU_Server)
"${SSH[@]}" 'bash -s' <<'OWNERSHIP'
set -euo pipefail
dir="$HOME/codex-build/xharness-runtime-sdk"
owner='123123213weqw/xharness-runtime:sdk-embedding-v1'
test ! -L "$dir"
if test -d "$dir" && ! test -f "$dir/.xharness-sdk-owner"; then
  test -z "$(find "$dir" -mindepth 1 -maxdepth 1 -print -quit)" || {
    echo 'Refusing a non-owned remote build directory' >&2; exit 2;
  }
fi
mkdir -p "$dir"
if test -f "$dir/.xharness-sdk-owner"; then
  test ! -L "$dir/.xharness-sdk-owner"
  test "$(cat "$dir/.xharness-sdk-owner")" = "$owner"
else
  printf '%s\n' "$owner" > "$dir/.xharness-sdk-owner"
fi
OWNERSHIP
rsync -az --delete -e 'ssh -o BatchMode=yes -o ForwardAgent=no -o ConnectTimeout=15' \
  --exclude='.git' --exclude='target/' --exclude='node_modules/' --exclude='__pycache__/' \
  --exclude='.sdk-evidence/' --exclude='.alignment-evidence/' --exclude='.xharness-sdk-owner' \
  --exclude='.env' --exclude='.env.*' --exclude='*.pem' --exclude='*.key' \
  --exclude='credentials.json' --exclude='credentials.toml' --exclude='cookies.txt' \
  "$ROOT/" 'WZU_Server:~/codex-build/xharness-runtime-sdk/'
git -C "$ROOT" rev-parse HEAD > "$EVIDENCE/git-head.txt"
git -C "$ROOT" status --short > "$EVIDENCE/git-status.txt"
git -C "$ROOT" diff --binary > "$EVIDENCE/tracked-diff.patch"
"${SSH[@]}" "bash -s -- '$MODE'" <<'VALIDATION' 2>&1 | tee "$EVIDENCE/full.log"
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH" LC_ALL=C LANG=C
export CARGO_TARGET_DIR="$HOME/codex-build/xharness-runtime-sdk/target"
export CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cd "$HOME/codex-build/xharness-runtime-sdk"
test "$(cat .xharness-sdk-owner)" = '123123213weqw/xharness-runtime:sdk-embedding-v1'
printf 'HOST=%s UTC=%s MODE=%s\n' "$(hostname)" "$(date -u +%FT%TZ)" "$1"
cargo fmt --all -- --check
cargo check --locked --offline -p xharness-sdk --all-targets
cargo clippy --locked --offline -p xharness-sdk --all-targets -- -D warnings
cargo test --locked --offline -p xharness-sdk --all-targets
RUSTDOCFLAGS='-D warnings' cargo doc --locked --offline -p xharness-sdk --no-deps
demo="$(mktemp -d "$PWD/.sdk-evidence-demo-XXXXXX")"
cargo run --locked --offline -p xharness-sdk --example offline_echo -- "$demo"
if test "$1" = regression; then
  cargo test --locked --offline -p xharness-host -p xharness-agent -p xharness-core \
    -p xharness-session -p xharness-session-jsonl -p xharness-tools --all-targets --no-fail-fast
fi
VALIDATION
printf '\nFull remote output: %s/full.log\n' "$EVIDENCE"
