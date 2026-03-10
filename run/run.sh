#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "${SCRIPT_DIR}")"

MAILMUX_MANIFEST="${REPO_DIR}/mailmux/Cargo.toml"
MAILTX_MANIFEST="${REPO_DIR}/mailtx/Cargo.toml"

mkdir -p "${SCRIPT_DIR}/bin"

cargo build --release --manifest-path "${MAILMUX_MANIFEST}"
cp "${REPO_DIR}/target/release/mailmux" "${SCRIPT_DIR}/bin/mailmux"

cargo build --release --manifest-path "${MAILTX_MANIFEST}"
cp "${REPO_DIR}/target/release/mailtx" "${SCRIPT_DIR}/bin/mailtx"

source "${SCRIPT_DIR}/.env"

"${SCRIPT_DIR}/bin/mailmux" -c "${SCRIPT_DIR}/config.toml"
