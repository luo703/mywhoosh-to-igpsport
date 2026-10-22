#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
asset_dir="$project_dir/assets"
expected='7b9d7936b75afc21d0cca11a3eddddea09ecdd782c6064dd1f65d6e674313bfb'
url='https://app.igpsport.cn/assets/edge_core_wasm_bg-GHC38n8Q.wasm'

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

mkdir -p "$asset_dir"
if [[ -f "$asset_dir/edge_core.wasm" ]] && [[ "$(checksum "$asset_dir/edge_core.wasm")" == "$expected" ]]; then
    echo '签名模块已存在，SHA-256 校验通过。'
    exit 0
fi

temporary="$(mktemp "$asset_dir/.edge_core.XXXXXX")"
trap 'rm -f "$temporary"' EXIT
echo '正在从 iGPSPORT 官网下载签名模块……'
curl --fail --location --proto '=https' --tlsv1.2 --connect-timeout 20 --max-time 120 --output "$temporary" "$url"
if [[ "$(checksum "$temporary")" != "$expected" ]]; then
    echo '签名模块 SHA-256 不匹配，未替换文件。' >&2
    exit 1
fi
chmod 644 "$temporary"
mv -- "$temporary" "$asset_dir/edge_core.wasm"
echo '签名模块下载完成，SHA-256 校验通过。'
