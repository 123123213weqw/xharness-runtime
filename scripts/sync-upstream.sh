#!/usr/bin/env bash
# 从上游 x-harness-rs 重新抽取运行时闭包。
# 只做文件系统复制 + cargo check;git 操作(分支/提交/PR)由调用方(workflow)负责。
# 输出:SYNC_SHA(上游基线)、SYNC_CHANGED(true/false);在 Actions 中同时写入 $GITHUB_OUTPUT。
set -euo pipefail

UPSTREAM_REPO="${UPSTREAM_REPO:-123123213weqw/x-harness-rs}"
UPSTREAM_URL="https://github.com/${UPSTREAM_REPO}.git"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git clone --quiet --depth 1 "$UPSTREAM_URL" "$WORK/upstream"
BASE_SHA="$(git -C "$WORK/upstream" rev-parse HEAD)"
echo "上游基线: $BASE_SHA"

# 闭包清单以本仓库 Cargo.toml 的 members 为准(自描述,不硬编码)
MEMBERS="$(python3 -c "
import re
print(' '.join(re.findall(r'\"crates/([a-z0-9-]+)\"', open('Cargo.toml').read())))
")"
echo "闭包 $(echo "$MEMBERS" | wc -w | tr -d ' ') 个 crate: $MEMBERS"

# 本仓库自有 crate(非上游闭包),同步时跳过
OWNED_CRATES="xharness-gh xharness-feishu"
for c in $MEMBERS; do
  case " $OWNED_CRATES " in
    *" $c "*) continue ;;
  esac
  if [ ! -d "$WORK/upstream/crates/$c" ]; then
    echo "错误: 上游已不存在 crates/$c,闭包需要人工调整" >&2
    exit 1
  fi
  rm -rf "crates/$c"
  cp -R "$WORK/upstream/crates/$c" "crates/$c"
done

rm -rf runtime scripts/fixtures
cp -R "$WORK/upstream/runtime" runtime
mkdir -p scripts config
cp -R "$WORK/upstream/scripts/fixtures" scripts/fixtures
cp "$WORK/upstream/config/providers.local-reasoning.example.json" config/
cp "$WORK/upstream/rust-toolchain.toml" rust-toolchain.toml
cp "$WORK/upstream/THIRD_PARTY_NOTICES.md" THIRD_PARTY_NOTICES.md

# 根 Cargo.toml:用上游原文,仅 members 换成本仓库闭包
python3 - "$WORK/upstream/Cargo.toml" Cargo.toml <<'PY'
import re, sys
upstream = open(sys.argv[1]).read()
ours = open(sys.argv[2]).read()
members = re.findall(r'"crates/([a-z0-9-]+)"', ours)
block = '\n' + ''.join(f'    "crates/{m}",\n' for m in members)
new = re.sub(r'(members\s*=\s*\[)\s*.*?(\s*\])',
             lambda m: m.group(1) + block + m.group(2), upstream, count=1, flags=re.S)
open(sys.argv[2], 'w').write(new)
PY

# README 溯源的基线提交行
python3 - "$BASE_SHA" <<'PY'
import re, sys
sha = sys.argv[1]
t = open('README.md').read()
t2 = re.sub(r'(基线提交:`)[0-9a-f]{40}(`)', lambda m: m.group(1) + sha + m.group(2), t)
open('README.md', 'w').write(t2)
PY

# 更新 Cargo.lock 并做编译底线验证
cargo check --workspace --all-targets --quiet

CHANGED="$(git status --porcelain | wc -l | tr -d ' ')"
if [ "$CHANGED" = "0" ]; then
  echo "SYNC_CHANGED=false"
  echo "与上游一致,无需同步"
else
  echo "SYNC_CHANGED=true"
  git status --porcelain | head -20
fi
echo "SYNC_SHA=$BASE_SHA"

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  echo "sha=$BASE_SHA" >> "$GITHUB_OUTPUT"
  echo "changed=$([ "$CHANGED" != "0" ] && echo true || echo false)" >> "$GITHUB_OUTPUT"
fi
