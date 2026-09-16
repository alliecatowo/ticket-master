verify:
    cargo run -p xtask -- verify

test:
    cargo test --workspace

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

hygiene:
    cargo run -p xtask -- hygiene
