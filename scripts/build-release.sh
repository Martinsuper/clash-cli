#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
PACKAGE_NAME="clash-cli"
TARGET="${1:-}"

cd "${ROOT_DIR}"
mkdir -p "${DIST_DIR}"

if [[ -n "${TARGET}" ]]; then
  echo "==> Building ${PACKAGE_NAME} for ${TARGET}"
  cargo build --release --target "${TARGET}"
  TARGET_DIR="${ROOT_DIR}/target/${TARGET}/release"
  PLATFORM="${TARGET}"
else
  echo "==> Building ${PACKAGE_NAME} for current platform"
  cargo build --release
  TARGET_DIR="${ROOT_DIR}/target/release"
  PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
fi

BIN_NAME="${PACKAGE_NAME}"
if [[ "${TARGET}" == *"windows"* ]]; then
  BIN_NAME="${PACKAGE_NAME}.exe"
fi

SRC_BIN="${TARGET_DIR}/${BIN_NAME}"
if [[ ! -f "${SRC_BIN}" ]]; then
  echo "build output not found: ${SRC_BIN}" >&2
  exit 1
fi

OUT_BIN="${DIST_DIR}/${PACKAGE_NAME}-${PLATFORM}"
if [[ "${BIN_NAME}" == *.exe ]]; then
  OUT_BIN="${OUT_BIN}.exe"
fi

cp "${SRC_BIN}" "${OUT_BIN}"
chmod +x "${OUT_BIN}" 2>/dev/null || true

echo "==> Built executable:"
echo "${OUT_BIN}"
