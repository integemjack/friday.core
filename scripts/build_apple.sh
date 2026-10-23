#!/bin/bash
# 给 Swift 版（Xcode）编 friday.core：每个架构编一份 libfridaycore.a，再 lipo 合成一个。
# Xcode 的构建阶段调它（Friday/Tools/CI/build_rust_core.sh），按 Xcode 给的 ARCHS 编（Debug 只有本机的 arm64，Release 是 arm64 x86_64）；
# 不在 Xcode 里跑时默认 arm64。一律编 release（Rust 的 debug 慢得多，增量编译几秒就过去）。
# 环境变量：
#   ARCHS                     要编的架构（arm64、x86_64，空格分开）
#   FRIDAY_CORE_OUTPUT_DIR    合成的 libfridaycore.a 放哪，默认 <仓库>/target/apple
#   CARGO                     cargo 的路径，默认 PATH 里的或 ~/.cargo/bin/cargo
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${FRIDAY_CORE_OUTPUT_DIR:-${ROOT}/target/apple}"
OUTPUT="${OUTPUT_DIR}/libfridaycore.a"
ARCH_LIST="${ARCHS:-arm64}"

# Xcode 构建阶段里的 PATH 不带 ~/.cargo/bin
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"
CARGO="${CARGO:-$(command -v cargo || true)}"
[ -n "${CARGO}" ] || { echo "error: 编 friday.core 要 Rust（https://rustup.rs，装好后 ~/.cargo/bin 里有 cargo）" >&2; exit 1; }

PARTS=()
for ARCH in ${ARCH_LIST}; do
  case "${ARCH}" in
    arm64) TARGET=aarch64-apple-darwin ;;
    x86_64) TARGET=x86_64-apple-darwin ;;
    *) echo "error: friday.core 不认识的架构 ${ARCH}" >&2; exit 1 ;;
  esac
  if command -v rustup >/dev/null && ! rustup target list --installed 2>/dev/null | grep -qx "${TARGET}"; then
    (cd "${ROOT}" && rustup target add "${TARGET}")
  fi
  (cd "${ROOT}" && "${CARGO}" build --release --locked -p friday-core-ffi --target "${TARGET}" --quiet) \
    || { echo "error: friday.core 编译失败（${ARCH}）" >&2; exit 1; }
  PARTS+=("${ROOT}/target/${TARGET}/release/libfridaycore.a")
done

# 合成 Xcode 链的那一个：哪一份比它新才重做（不然每次构建都要重新链 App）；架构变了也重做
mkdir -p "${OUTPUT_DIR}"
STALE=0
[ -f "${OUTPUT}" ] || STALE=1
for PART in "${PARTS[@]}"; do
  [ "${PART}" -nt "${OUTPUT}" ] && STALE=1
done
if [ "${STALE}" = "0" ]; then
  HAVE="$(lipo -archs "${OUTPUT}" 2>/dev/null | tr ' ' '\n' | sort | xargs)"
  WANT="$(echo "${ARCH_LIST}" | tr ' ' '\n' | sort | xargs)"
  [ "${HAVE}" = "${WANT}" ] || STALE=1
fi
if [ "${STALE}" = "1" ]; then
  if [ "${#PARTS[@]}" -eq 1 ]; then
    cp -f "${PARTS[0]}" "${OUTPUT}.tmp"
  else
    lipo -create "${PARTS[@]}" -output "${OUTPUT}.tmp"
  fi
  mv -f "${OUTPUT}.tmp" "${OUTPUT}"
  echo "▶ friday.core：${OUTPUT}（${ARCH_LIST}）"
fi
