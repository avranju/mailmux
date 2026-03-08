#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DB_NAME="mailmux"

source "${SCRIPT_DIR}/.env"

echo "Dropping database: ${DB_NAME} (if it exists)..."
dropdb --if-exists "${DB_NAME}"

echo "Creating database: ${DB_NAME}..."
createdb "${DB_NAME}"

echo "Clearing email cache 'data' folder..."
rm -rf "${SCRIPT_DIR}/data" && mkdir "${SCRIPT_DIR}/data"

echo "Done."
