# conductor

Multi-agent orchestrator for Claude Code and herdr

A conductor Claude session briefs tasks and runs each one in a worker session in its own worktree
and herdr workspace. The workers' hooks record what they're doing, and the conductor is woken
when one needs it. See [docs/design.md](docs/design.md).

## Requirements

- [herdr](https://herdr.dev) 0.9.4 or later. Run conductor inside it. The
  [conductor-deck](https://github.com/erd/conductor-deck) herdr plugin is optional; when it's
  installed, the deck opens beside the conductor.
- [worktrunk](https://github.com/max-sixty/worktrunk) (`wt`), which creates the workers'
  worktrees.
- [`gh`](https://cli.github.com), logged in, for PRs.
- Claude Code with auto mode (`--permission-mode auto`) available on your account. The
  conductor and every worker run in it.
- A Rust toolchain, to install the CLI with `cargo`.

## Install

1. Install the `conductor` CLI into `~/.cargo/bin`:

   ```sh
   cargo install --git https://github.com/erd/conductor conductor
   ```

2. Add this repo as a Claude Code marketplace and install the plugin from it:

   ```sh
   claude plugin marketplace add erd/conductor
   claude plugin install conductor@conductor
   ```

The plugin is installed at user scope, so its hooks run in every Claude session. They do
nothing outside conductor sessions. If the CLI isn't in `~/.cargo/bin`, set `CONDUCTOR_BIN` to
its path.

## Usage

From a repo's main checkout (or any worktree of it), inside herdr:

```sh
conductor start
```

This records the repo in `~/.config/conductor` and starts the conductor's Claude session.
`conductor --help` lists the other commands.

## Development

The toolchain is pinned in `rust-toolchain.toml`; use rustup so it is picked up automatically.

```sh
cargo install --path crates/conductor   # installs `conductor` into ~/.cargo/bin

cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

To try plugin changes, use this checkout as the marketplace, or load it for one session:

```sh
claude plugin marketplace add ./
claude plugin install conductor@conductor

claude --plugin-dir .
```

CI checks the plugin and marketplace manifests with `claude plugin validate .`. It warns that
`plugin.json` has no `version`; that's intended, so installs follow the latest commit.
