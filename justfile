release:
    cd phodex-bridge && ./scripts/release.sh

fmt:
    cargo fmt --manifest-path phodex-bridge/Cargo.toml

clippy:
    cargo clippy --manifest-path phodex-bridge/Cargo.toml --all-targets --all-features

test:
    cargo test --manifest-path phodex-bridge/Cargo.toml

alias r := release
