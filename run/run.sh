#!/usr/bin/env zsh
set -euo pipefail

SCRIPT_DIR="${0:a:h}"
REPO_DIR="${SCRIPT_DIR:h}"

MAILMUX_MANIFEST="${REPO_DIR}/mailmux/Cargo.toml"
BANK_TX_MANIFEST="${REPO_DIR}/bank-tx-processor/Cargo.toml"

mkdir -p "${SCRIPT_DIR}/bin"

cargo build --release --manifest-path "${MAILMUX_MANIFEST}"
cp "${REPO_DIR}/mailmux/target/release/mailmux" "${SCRIPT_DIR}/bin/mailmux"

cargo build --release --manifest-path "${BANK_TX_MANIFEST}"
cp "${REPO_DIR}/bank-tx-processor/target/release/bank-tx-processor" "${SCRIPT_DIR}/bin/bank-tx-processor"

source "${SCRIPT_DIR}/.env"

"${SCRIPT_DIR}/bin/mailmux" -c "${SCRIPT_DIR}/config.toml"
