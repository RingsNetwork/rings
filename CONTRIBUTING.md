# Contributing to Rings

Thank you for helping improve Rings. Contributions can include bug reports, design
discussion, documentation, tests, and code.

Participation in this project is governed by our [Code of Conduct](./CODE_OF_CONDUCT.md).

## Before You Start

- Search existing [issues](https://github.com/RingsNetwork/rings/issues) and
  [pull requests](https://github.com/RingsNetwork/rings/pulls) before opening a
  duplicate.
- Use an issue to discuss large changes to protocols, public APIs, compatibility,
  or architecture before investing in an implementation.
- Report vulnerabilities according to [SECURITY.md](./SECURITY.md). Do not disclose
  security-sensitive details in a public issue.

## Development Setup

Clone the repository and let `rustup` install the toolchain pinned in
[`rust-toolchain.toml`](./rust-toolchain.toml):

```sh
git clone https://github.com/RingsNetwork/rings.git
cd rings
rustup show
cargo build --all
```

Changes to the browser or extension code also require Node.js. Install its locked
dependencies without running the package build during installation:

```sh
npm ci --ignore-scripts
```

The exact tool versions used by CI are defined in
[`.github/workflows/qaci.yml`](./.github/workflows/qaci.yml).

## Making Changes

- Keep each pull request focused on one problem.
- Add or update tests for behavior changes and bug fixes.
- Update public API documentation and user-facing documentation when behavior
  changes.
- Preserve the security boundaries documented in [SECURITY.md](./SECURITY.md), or
  update that document when a boundary intentionally changes.
- Do not commit secrets, local configuration, build output, or generated artifacts
  unless the repository explicitly tracks them.

## Validation

Run focused tests while iterating. Before opening a pull request, run the relevant
checks below when practical.

For Rust changes:

```sh
cargo +nightly-2026-07-02 fmt --all -- --check
cargo clippy --all --tests -- -D warnings
cargo test --release --all --all-targets
```

For browser or extension changes:

```sh
npm ci --ignore-scripts
npm run build:frontend-extension-scripts
```

For documentation-book changes:

```sh
mdbook build docs
```

If a change touches TOML files or prose, also run `taplo format --check` or `typos`
when those tools are installed. GitHub Actions runs the complete cross-platform,
WASM, security, and release-target matrix; all required checks must pass before a
change can be merged.

## Commits and Pull Requests

Create a branch from `master` and use descriptive commit messages. Keep unrelated
formatting or refactoring out of the same change.

When opening a pull request:

- Explain the problem and the reason for the chosen approach.
- Describe the old and new behavior.
- List the tests and checks you ran.
- Call out compatibility or breaking changes explicitly.
- Link the relevant issue when one exists.
- Use a draft pull request if the change is not ready for review.

Respond to review feedback with follow-up commits or a clear explanation. Resolve
review threads only after the concern has been addressed or agreement has been
reached.

## Licensing

Review the project's [AGPL-3.0-only license](./LICENSE) before contributing. By
submitting a contribution, you represent that you have the right to provide it for
inclusion in the project. Accepted contributions are distributed as part of Rings
under the repository's license.
