#!/usr/bin/env bash
set -euo pipefail

detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *)
      echo "暂不支持的操作系统: $os" >&2
      exit 1
      ;;
  esac

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="arm64" ;;
    *)
      echo "暂不支持的 CPU 架构: $arch" >&2
      exit 1
      ;;
  esac

  printf 'cx-%s-%s' "$os" "$arch"
}

normalize_channel() {
  case "${1:-main}" in
    main|rolling) printf 'main' ;;
    release|stable|tag) printf 'release' ;;
    *)
      echo "不支持的安装通道: ${1}" >&2
      echo "支持的通道: main, release" >&2
      exit 1
      ;;
  esac
}

normalize_tag() {
  local version="$1"

  if [[ -z "$version" ]]; then
    echo "CX_VERSION 不能为空" >&2
    exit 1
  fi

  case "$version" in
    v*) printf '%s' "$version" ;;
    *) printf 'v%s' "$version" ;;
  esac
}

resolve_download_urls() {
  local channel="$1"
  local version="$2"
  local base_url="$3"
  local project_path="$4"
  local main_ref="$5"
  local main_job="$6"
  local asset_name="$7"
  local release_base

  case "$channel" in
    main)
      if [[ -n "$version" ]]; then
        echo "main 通道不支持 CX_VERSION；如需稳定版，请设置 CX_CHANNEL=release。" >&2
        exit 1
      fi
      RESOLVED_BINARY_URL="${base_url}/${project_path}/-/jobs/artifacts/${main_ref}/raw/dist/${asset_name}?job=${main_job}"
      RESOLVED_CHECKSUM_URL="${base_url}/${project_path}/-/jobs/artifacts/${main_ref}/raw/dist/SHA256SUMS?job=${main_job}"
      ;;
    release)
      if [[ -n "$version" ]]; then
        release_base="${base_url}/${project_path}/-/releases/$(normalize_tag "$version")/downloads"
      else
        release_base="${base_url}/${project_path}/-/releases/permalink/latest/downloads"
      fi
      RESOLVED_BINARY_URL="${release_base}/binaries/${asset_name}"
      RESOLVED_CHECKSUM_URL="${release_base}/checksums/SHA256SUMS"
      ;;
  esac
}

path_contains() {
  case ":${PATH:-}:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

main() {
  local base_url project_path install_dir target_bin channel version main_ref main_job
  local asset_name tmp_dir binary_path checksum_path expected_sum actual_sum

  base_url="${CX_GITLAB_BASE_URL:-https://git.huayi.tech}"
  project_path="${CX_GITLAB_PROJECT_PATH:-awesome/cx}"
  install_dir="${CX_INSTALL_DIR:-$HOME/.local/bin}"
  target_bin="${install_dir}/cx"
  channel="$(normalize_channel "${CX_CHANNEL:-main}")"
  version="${CX_VERSION:-}"
  main_ref="${CX_GITLAB_MAIN_REF:-main}"
  main_job="${CX_GITLAB_MAIN_JOB:-publish-main}"

  asset_name="$(detect_platform)"
  resolve_download_urls "$channel" "$version" "$base_url" "$project_path" "$main_ref" "$main_job" "$asset_name"

  tmp_dir="$(mktemp -d)"
  trap "rm -rf '$tmp_dir'" EXIT

  binary_path="${tmp_dir}/${asset_name}"
  checksum_path="${tmp_dir}/SHA256SUMS"

  curl --fail --show-error --silent --location \
    --output "$binary_path" \
    "$RESOLVED_BINARY_URL"

  curl --fail --show-error --silent --location \
    --output "$checksum_path" \
    "$RESOLVED_CHECKSUM_URL"

  expected_sum="$(grep " ${asset_name}$" "$checksum_path" | awk '{print $1}')"
  if [[ -z "$expected_sum" ]]; then
    echo "未在 SHA256SUMS 中找到 ${asset_name} 的校验值。" >&2
    exit 1
  fi

  if command -v sha256sum >/dev/null 2>&1; then
    actual_sum="$(sha256sum "$binary_path" | awk '{print $1}')"
  else
    actual_sum="$(shasum -a 256 "$binary_path" | awk '{print $1}')"
  fi

  if [[ "$expected_sum" != "$actual_sum" ]]; then
    echo "校验失败: 期望 ${expected_sum}，实际 ${actual_sum}" >&2
    exit 1
  fi

  mkdir -p "$install_dir"
  install "$binary_path" "$target_bin"

  echo "已安装到: $target_bin"
  if ! path_contains "$install_dir"; then
    echo "提示: $install_dir 当前不在 PATH 中。"
    echo "可先执行："
    echo "  export PATH=\"$install_dir:\$PATH\""
  fi
}

main "$@"
