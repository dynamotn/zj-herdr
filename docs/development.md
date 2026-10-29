# Development

## Building from source

```sh
cargo install --path crates/cli          # the zj-herdr binary
rustup target add wasm32-wasip1
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
mkdir -p ~/.config/zellij/plugins
cp target/wasm32-wasip1/release/zj_herdr.wasm ~/.config/zellij/plugins/
```

Point the plugin alias at `file:~/.config/zellij/plugins/zj_herdr.wasm`, as
[`zellij.kdl`](zellij.kdl) does.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
```

Tests use the fixture transcripts in `crates/cli/tests/fixtures/` and never
read the real `~/.claude`, `~/.codex` or `~/.ssh`. See
[docs/plan.md](plan.md) for the design.

Two end-to-end suites run the real programs in a private `HOME` and
`TMPDIR`: `crates/cli/tests/e2e_cli.rs` drives the `zj-herdr` binary and
its daemon over a real socket, and `crates/plugin/tests/e2e_zellij.rs`
starts a headless Zellij session (named `zj-herdr-e2e`, in its own socket
directory), loads the built `zj_herdr.wasm` with its permissions granted,
pipes it a state and reads the rail back with `zellij action dump-screen`.
Where Unix sockets cannot be bound (some sandboxes) or Zellij is missing,
they skip; `ZJ_HERDR_E2E_ZELLIJ=1` makes the Zellij one fail instead.

The plugin's behaviour lives in `crates/plugin/src/app.rs`, which reaches
Zellij only through a `Host` trait, so it is tested natively;
`crates/plugin/src/main.rs` only implements that trait on Zellij's API and
is left out of the coverage figure. CI (`.github/workflows/ci.yml`) holds
the rest to 95% of lines:

```sh
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
ZJ_HERDR_E2E_ZELLIJ=1 cargo llvm-cov --workspace \
  --ignore-filename-regex 'plugin/src/main\.rs' --fail-under-lines 95
```
