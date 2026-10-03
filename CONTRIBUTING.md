# Contributing

Thanks for taking a look. The short version:

1. Install the toolchain with [`mise`](https://mise.jdx.dev/) (`mise install`). You also need a C
   and a C++ compiler.
2. Branch from `main`, make the change, and run the full gate before opening a PR:

   ```sh
   mise run verify     # fmt check + clippy -D warnings + cargo test --workspace + hygiene
   ```

3. For the TypeScript clients (`clients/ts`, `clients/vscode`, `clients/web`) use `pnpm`:
   `pnpm -C clients/<name> install && pnpm -C clients/<name> test`.
4. Open a PR. CI runs Rust tests on Linux and macOS, `cargo audit`, the three TypeScript clients
   and the Swift package; all must pass.

Conventions, decision records (`docs/decisions/`) and the workspace layout are described in
[`CLAUDE.md`](CLAUDE.md) and [`docs/`](docs/). Security issues: please open a private security
advisory on GitHub rather than a public issue.

By contributing you agree that your work is licensed under the repository's
MIT OR Apache-2.0 terms.
