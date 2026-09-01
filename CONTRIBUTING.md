# Contributing to bork

Thanks for wanting to contribute! Contributions of all sizes are welcome, from typo fixes to new features.

## Getting started

bork is a Rust TUI, built with ratatui. You need a recent stable Rust toolchain and, to actually run it, `tmux` and `git` on your PATH.

```bash
git clone https://github.com/villads-valur/bork.git
cd bork
cargo build
cargo test
```

## Before opening a PR

CI runs these, so save yourself a round trip and run them locally first:

```bash
cargo test --all-targets
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Guidelines

- **Conventional commits**: prefix commit messages and PR titles with `fix:`, `feat:`, `docs:`, `refactor:`, etc.
- **Keep PRs focused**: one fix or feature per PR makes review much faster.
- **Explain the why**: a short problem/fix description in the PR body (like [#102](https://github.com/villads-valur/bork/pull/102)) goes a long way.
- **Add a test** when fixing a bug, so it stays fixed.
- For larger changes, open an issue first so we can agree on the approach before you invest the time.

PRs are squash-merged, so don't worry about keeping your branch history tidy.

## Reporting bugs

Open an issue with your OS, tmux version (`tmux -V`), how you installed bork, and steps to reproduce. Terminal output or a screenshot helps a lot.
