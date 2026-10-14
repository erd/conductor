# conductor

Multi-agent orchestrator for Claude Code and herdr


## Development

The toolchain is pinned in `rust-toolchain.toml`; use rustup so it is picked up automatically.

```sh
cargo install --path crates/conductor   # installs `conductor` into ~/.cargo/bin

cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```
