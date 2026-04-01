#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd "$script_dir/.." && pwd)
destination_dir="$HOME/.local/bin"
binary_name=remodex
release_binary="$project_dir/target/release/$binary_name"
defaults_path="$project_dir/src/private-defaults.json"

mkdir -p "$destination_dir"

printf '{\n  "relayUrl": "wss://api.phodex.app/relay",\n  "pushServiceUrl": ""\n}\n' > "$defaults_path"
cargo build --release --manifest-path "$project_dir/Cargo.toml" --bin "$binary_name"
mv "$release_binary" "$destination_dir/$binary_name"

echo "Moved $binary_name to $destination_dir/$binary_name"
