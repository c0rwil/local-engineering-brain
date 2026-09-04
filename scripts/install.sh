#!/bin/sh

set -eu
umask 077

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
INSTALL_BIN_DIR=${INSTALL_BIN_DIR:-"${HOME}/.local/bin"}
INSTALL_STATE_DIR=${INSTALL_STATE_DIR:-"${HOME}/.local/share/engram"}
LOCK_DIR="${INSTALL_STATE_DIR}/install.lock"

command -v cargo >/dev/null 2>&1 || {
  echo "install: cargo is required" >&2
  exit 1
}
command -v shasum >/dev/null 2>&1 || {
  echo "install: shasum is required" >&2
  exit 1
}

mkdir -p "${INSTALL_STATE_DIR}" "${INSTALL_BIN_DIR}"
if ! mkdir "${LOCK_DIR}" 2>/dev/null; then
  echo "install: another Engram installation is in progress" >&2
  exit 1
fi
trap 'rmdir "${LOCK_DIR}" 2>/dev/null || true' EXIT INT TERM

SOURCE_HASH=$(
  find "${PROJECT_ROOT}/src" "${PROJECT_ROOT}/scripts" "${PROJECT_ROOT}/assets" \
    "${PROJECT_ROOT}/Cargo.toml" "${PROJECT_ROOT}/Cargo.lock" \
    -type f -print0 \
    | sort -z \
    | xargs -0 shasum -a 256 \
    | awk '{print $1}' \
    | shasum -a 256 \
    | awk '{print $1}'
)

if [ -x "${INSTALL_BIN_DIR}/engram" ]; then
  "${INSTALL_BIN_DIR}/engram" daemon stop >/dev/null 2>&1 || true
fi

echo "Building Engram release ${SOURCE_HASH}..." >&2
(cd "${PROJECT_ROOT}" && ENGRAM_BUILD_ID="${SOURCE_HASH}" cargo build --release --locked)

RELEASE_DIR="${INSTALL_STATE_DIR}/releases/${SOURCE_HASH}"
STAGING_DIR="${INSTALL_STATE_DIR}/releases/.${SOURCE_HASH}.staging.$$"
mkdir -p "${STAGING_DIR}/bin"
for binary in engram engramd engram-proxy engram-mcp; do
  cp "${PROJECT_ROOT}/target/release/${binary}" "${STAGING_DIR}/bin/${binary}"
  chmod 755 "${STAGING_DIR}/bin/${binary}"
done
for launcher in claude-mtplx claude-mtplx-quality; do
  cp "${PROJECT_ROOT}/scripts/${launcher}" "${STAGING_DIR}/bin/${launcher}"
  chmod 755 "${STAGING_DIR}/bin/${launcher}"
done
mkdir -p "${INSTALL_STATE_DIR}/releases"
if [ -d "${RELEASE_DIR}" ]; then
  rm -rf "${STAGING_DIR}"
else
  mv "${STAGING_DIR}" "${RELEASE_DIR}"
fi

for binary in engram engramd engram-proxy engram-mcp claude-mtplx claude-mtplx-quality; do
  temporary="${INSTALL_BIN_DIR}/.${binary}.new.$$"
  ln -s "${RELEASE_DIR}/bin/${binary}" "${temporary}"
  mv -f "${temporary}" "${INSTALL_BIN_DIR}/${binary}"
done
# Legacy names keep working: hooks, MCP configs, and launchers written
# before the Engram rename resolve to the same release.
for pair in leb:engram lebd:engramd leb-proxy:engram-proxy leb-mcp:engram-mcp; do
  legacy=${pair%%:*}; current=${pair##*:}
  temporary="${INSTALL_BIN_DIR}/.${legacy}.new.$$"
  ln -s "${RELEASE_DIR}/bin/${current}" "${temporary}"
  mv -f "${temporary}" "${INSTALL_BIN_DIR}/${legacy}"
done

MANIFEST_TMP="${INSTALL_STATE_DIR}/.install-manifest.json.$$"
{
  printf '{\n  "version": "0.1.0",\n  "source_hash": "%s",\n' "${SOURCE_HASH}"
  printf '  "source_dir": "%s",\n' "${PROJECT_ROOT}"
  printf '  "release_dir": "%s",\n  "sha256": {\n' "${RELEASE_DIR}"
  first=1
  for binary in engram engramd engram-proxy engram-mcp claude-mtplx claude-mtplx-quality; do
    digest=$(shasum -a 256 "${RELEASE_DIR}/bin/${binary}" | awk '{print $1}')
    if [ "${first}" -eq 0 ]; then
      printf ',\n'
    fi
    printf '    "%s": "%s"' "${binary}" "${digest}"
    first=0
  done
  printf '\n  }\n}\n'
} >"${MANIFEST_TMP}"
mv "${MANIFEST_TMP}" "${INSTALL_STATE_DIR}/install-manifest.json"

echo "Installed Engram ${SOURCE_HASH} in ${INSTALL_BIN_DIR}." >&2
