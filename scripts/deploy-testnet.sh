#!/usr/bin/env bash
# Deploy a client TrustlineOracleVE + Trustline Vault + Dummy Adapter (testnet).
#
# Prerequisites:
#   - Trustline core already ran (registry + VE WASM upload) via
#     stellar-validation-engine/scripts/deploy-testnet.sh
#   - Pass the uploaded WASM hash (registry is baked into that WASM)
#
# Flow:
#   1. Deploy VE (from VE_WASM_HASH), vault, dummy adapter
#   2. Disable Trustline on the VE (so role/config calls do not need WebSDK)
#   3. Bootstrap vault + adapter (curator, allocators, register adapter, caps)
#   4. Re-enable Trustline
#
# Env:
#   STELLAR_ACCOUNT     CLI identity (funded) — VE admin / vault+adapter owner (required)
#   VE_WASM_HASH        uploaded TrustlineOracleVE WASM hash (required)
#   STELLAR_NETWORK     default: testnet
#   DEFAULT_TIMELOCK    must be 0 for submit→execute bootstrap (default: 0)
#
# Writes:
#   scripts/last-deploy.env  (gitignored) — contract ids + WASM hashes from this run
#
# Build uses crates.io trustline-sdk 0.2.0 unless you manually copy
# .cargo/config.toml.example → .cargo/config.toml (local path override).
# For a hash that matches wasm-hashes.txt, use the pins in versions.env and no config.toml.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck disable=SC1091
source "$ROOT/versions.env"
NETWORK="${STELLAR_NETWORK:-testnet}"
SOURCE="${STELLAR_ACCOUNT:?Set STELLAR_ACCOUNT to a funded stellar CLI identity}"
VE_WASM_HASH="${VE_WASM_HASH:?Set VE_WASM_HASH to the uploaded TrustlineOracleVE WASM hash}"
DEFAULT_TIMELOCK="${DEFAULT_TIMELOCK:-0}"
LAST_ENV="$ROOT/scripts/last-deploy.env"

if [[ "$DEFAULT_TIMELOCK" != "0" ]]; then
  echo "ERROR: DEFAULT_TIMELOCK=$DEFAULT_TIMELOCK — bootstrap needs 0 so submit→execute is immediate." >&2
  exit 1
fi

ADDR="$(stellar keys address "$SOURCE")"
echo "Deployer / VE admin / vault+adapter owner: $ADDR"
echo "Network: $NETWORK"
echo "VE_WASM_HASH=$VE_WASM_HASH"
echo "Pinned tools: Rust $RUST_VERSION, Stellar CLI $STELLAR_CLI_VERSION"
if [[ -f "$ROOT/.cargo/config.toml" ]]; then
  echo "SDK: local path override (.cargo/config.toml) — WASM may not match wasm-hashes.txt"
else
  echo "SDK: trustline-sdk 0.2.0 from crates.io"
fi
CLI_VER="$(stellar --version 2>/dev/null | head -n1 || true)"
if [[ "$CLI_VER" != *"$STELLAR_CLI_VERSION"* ]]; then
  echo "WARNING: stellar CLI is '$CLI_VER' (want $STELLAR_CLI_VERSION from versions.env)" >&2
fi

strip_quotes() {
  tr -d '"'
}

echo "==> Building vault contracts"
(cd "$ROOT" && stellar contract build >/dev/null)

VAULT_WASM="$ROOT/target/wasm32v1-none/release/trustline_vault.wasm"
ADAPTER_WASM="$ROOT/target/wasm32v1-none/release/dummy_adapter.wasm"

echo "==> Uploading vault + adapter WASM"
VAULT_HASH="$(stellar contract upload --wasm "$VAULT_WASM" --network "$NETWORK" --source-account "$SOURCE" | strip_quotes)"
ADAPTER_HASH="$(stellar contract upload --wasm "$ADAPTER_WASM" --network "$NETWORK" --source-account "$SOURCE" | strip_quotes)"
echo "VAULT_WASM_HASH=$VAULT_HASH"
echo "ADAPTER_WASM_HASH=$ADAPTER_HASH"

echo "==> Deploying TrustlineOracleVE"
VE_ID="$(stellar contract deploy \
  --wasm-hash "$VE_WASM_HASH" \
  --network "$NETWORK" \
  --source-account "$SOURCE" \
  -- \
  --admin "$ADDR" \
  --auto-validity-secs "1800" \
  --manual-validity-secs "432000" \
  --max-skew-secs "60" | strip_quotes)"
echo "VE=$VE_ID"

echo "==> VE: disabling Trustline (bootstrap without WebSDK)"
stellar contract invoke --id "$VE_ID" --network "$NETWORK" --source-account "$SOURCE" -- \
  set_validation_configuration \
  --trustline_enabled false \
  --sanctions_enabled false \
  --sanctions_key null

ASSET_ID="$(stellar contract id asset --asset native --network "$NETWORK" 2>/dev/null | strip_quotes || true)"
if [[ -z "${ASSET_ID}" ]]; then
  echo "==> Deploying native SAC wrapper"
  ASSET_ID="$(stellar contract asset deploy --asset native --network "$NETWORK" --source-account "$SOURCE" | strip_quotes)"
fi
echo "ASSET=$ASSET_ID"

echo "==> Deploying Trustline Vault (owner=$ADDR, fees zeroed, default_timelock=0)"
VAULT_ID="$(stellar contract deploy \
  --wasm-hash "$VAULT_HASH" \
  --network "$NETWORK" \
  --source-account "$SOURCE" \
  -- \
  --validation-engine "$VE_ID" \
  --asset "$ASSET_ID" \
  --owner "$ADDR" \
  --decimals "13" \
  --name "Trustline Vault" \
  --symbol "tVAULT" \
  --fees "{\"receiver\":\"$ADDR\",\"management_bps\":0,\"performance_bps\":0,\"penalties\":[]}" \
  --default-timelock "$DEFAULT_TIMELOCK" | strip_quotes)"
echo "VAULT=$VAULT_ID"

echo "==> Deploying Dummy Adapter (bound to vault, owner=$ADDR)"
ADAPTER_ID="$(stellar contract deploy \
  --wasm-hash "$ADAPTER_HASH" \
  --network "$NETWORK" \
  --source-account "$SOURCE" \
  -- \
  --validation-engine "$VE_ID" \
  --vault "$VAULT_ID" \
  --asset "$ASSET_ID" \
  --owner "$ADDR" | strip_quotes)"
echo "ADAPTER=$ADAPTER_ID"

WAD="1000000000000000000" # 1e18 = 100% relative
ABS_CAP_MAX="170141183460469231731687303715884105727" # i128::MAX

invoke_vault() {
  stellar contract invoke --id "$VAULT_ID" --network "$NETWORK" --source-account "$SOURCE" -- "$@"
}

invoke_adapter() {
  stellar contract invoke --id "$ADAPTER_ID" --network "$NETWORK" --source-account "$SOURCE" -- "$@"
}

submit_op() {
  invoke_vault submit --op "$1"
}

echo "==> Bootstrap vault (Trustline disabled)"
echo "    set_curator → $ADDR"
invoke_vault set_curator --new-curator "$ADDR"

echo "    set_is_allocator($ADDR, true)"
submit_op "{\"SetIsAllocator\":[\"$ADDR\",true]}"
invoke_vault set_is_allocator --who "$ADDR" --is-allocator true

echo "    add_adapter($ADAPTER_ID)"
submit_op "{\"AddAdapter\":\"$ADAPTER_ID\"}"
invoke_vault add_adapter --adapter "$ADAPTER_ID"

echo "    increase_absolute_cap → i128::MAX"
submit_op "{\"IncreaseAbsoluteCap\":[\"$ADAPTER_ID\",\"$ABS_CAP_MAX\"]}"
invoke_vault increase_absolute_cap --adapter "$ADAPTER_ID" --new-absolute-cap "$ABS_CAP_MAX"

echo "    increase_relative_cap → 100% (WAD)"
submit_op "{\"IncreaseRelativeCap\":[\"$ADAPTER_ID\",\"$WAD\"]}"
invoke_vault increase_relative_cap --adapter "$ADAPTER_ID" --new-relative-cap "$WAD"

echo "==> Bootstrap adapter (Trustline disabled)"
echo "    set_is_allocator($ADDR, true)"
invoke_adapter set_is_allocator --who "$ADDR" --is-allocator true

echo "==> VE: re-enabling Trustline"
stellar contract invoke --id "$VE_ID" --network "$NETWORK" --source-account "$SOURCE" -- \
  set_validation_configuration \
  --trustline_enabled true \
  --sanctions_enabled false \
  --sanctions_key null

cat > "$LAST_ENV" <<EOF
# Generated by scripts/deploy-testnet.sh — do not commit.
STELLAR_NETWORK=$NETWORK
VE_WASM_HASH=$VE_WASM_HASH
VAULT_WASM_HASH=$VAULT_HASH
ADAPTER_WASM_HASH=$ADAPTER_HASH
VE_ID=$VE_ID
VAULT_ID=$VAULT_ID
ADAPTER_ID=$ADAPTER_ID
ASSET_ID=$ASSET_ID
EOF

echo ""
echo "Deployed and bootstrapped (Trustline back on)."
echo "  VE=$VE_ID"
echo "  VAULT=$VAULT_ID"
echo "  ADAPTER=$ADAPTER_ID"
echo "  ASSET=$ASSET_ID"
echo "  VE_WASM_HASH=$VE_WASM_HASH"
echo "  VAULT_WASM_HASH=$VAULT_HASH"
echo "  ADAPTER_WASM_HASH=$ADAPTER_HASH"
echo "  Recorded: $LAST_ENV"
echo ""
echo "Next: register VAULT (+ ADAPTER for adapter setters) with Trustline for your chain id,"
echo "then call Trustline-gated entrypoints via WebSDK / Freighter."
