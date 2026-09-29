#!/usr/bin/env bash
# Rebuild contracts and rewrite wasm-hashes.txt with the resulting sha256 digests.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ -f .cargo/config.toml ]]; then
  echo "ERROR: .cargo/config.toml is present; remove it for a reproducible crates.io build." >&2
  exit 1
fi

# shellcheck disable=SC1091
source "$ROOT/versions.env"

echo "Rust (want $RUST_VERSION): $(rustc --version)"
echo "Stellar CLI (want $STELLAR_CLI_VERSION): $(stellar --version | head -n1)"

stellar contract build

OUT="$ROOT/wasm-hashes.txt"
{
  cat <<'EOF'
# Expected sha256 of optimized WASM from `stellar contract build`
# with the pinned Rust + Stellar CLI in versions.env / rust-toolchain.toml,
# Cargo.lock, and no local .cargo/config.toml override.
#
# Regenerate after an intentional contract change:
#   ./scripts/record-wasm-hashes.sh
#
# Verify:
#   ./scripts/check-wasm-hashes.sh

EOF
  for name in dummy_adapter.wasm mock_validation_engine.wasm trustline_vault.wasm; do
    path="$ROOT/target/wasm32v1-none/release/$name"
    hash="$(sha256sum "$path" | awk '{print $1}')"
    echo "$name $hash"
  done
} > "$OUT"

echo "Wrote $OUT"
cat "$OUT"
