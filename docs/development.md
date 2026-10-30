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

CI (`.github/workflows/ci.yml`) runs these on every push to `main` and every
pull request:

```sh
rustup component add rustfmt clippy llvm-tools-preview
rustup target add wasm32-wasip1
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
```

then every test under coverage (below). Locally, `cargo test --workspace`
runs the same tests without it.

Tests use the fixture transcripts in `crates/cli/tests/fixtures/` and never
read the real `~/.claude`, `~/.codex` or `~/.ssh`. See
[architecture.md](architecture.md) for the design.

Two end-to-end suites run the real programs in a private `HOME` and
`TMPDIR`: `crates/cli/tests/e2e_cli.rs` drives the `zj-herdr` binary and
its daemon over a real socket, and `crates/plugin/tests/e2e_zellij.rs`
starts a headless Zellij session (named `zj-herdr-e2e`, in its own socket
directory; CI installs Zellij 0.45.1), loads the built `zj_herdr.wasm` with
its permissions granted, pipes it a state and reads the rail back with
`zellij action dump-screen`.
Where Unix sockets cannot be bound (some sandboxes), they skip. The Zellij
one also skips when Zellij or the built `zj_herdr.wasm` is missing, so build
the plugin first; `ZJ_HERDR_E2E_ZELLIJ=1` makes it fail instead.

The plugin's behaviour lives in `crates/plugin/src/app.rs`, which reaches
Zellij only through a `Host` trait, so it is tested natively;
`crates/plugin/src/main.rs` only implements that trait on Zellij's API and
is left out of the coverage figure. CI (`.github/workflows/ci.yml`) holds
the rest to 100% of lines (it needs `cargo-llvm-cov`):

```sh
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
ZJ_HERDR_E2E_ZELLIJ=1 cargo llvm-cov --workspace \
  --ignore-filename-regex 'plugin/src/main\.rs' --fail-under-lines 100
```

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`: it creates the
release, uploads the CLI as `zj-herdr-<target>.tar.gz` (binary at the archive
root) for `x86_64` and `aarch64` Linux (musl) and macOS, and uploads
`zj_herdr.wasm`, so `releases/latest/download/zj_herdr.wasm` always points at
the newest plugin.
