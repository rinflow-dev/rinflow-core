# Contributing to Rinflow Core

Thanks for your interest. This repository is the Rust library shared by the [macOS](https://github.com/rinflow-dev/rinflow-macos) and [Windows](https://github.com/rinflow-dev/rinflow-windows) clients.

## Building and testing

You need Rust 1.88 or newer ([rustup](https://rustup.rs)). Before opening a pull request:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Several tests start real cores that find each other over UDP multicast, so they need a network interface that allows multicast loopback, and may occasionally time out with `peers not discovered`. Rerun before treating that as a regression.

If a dependency changes, regenerate the clients' third-party notices with [scripts/third-party-notices.ps1](scripts/third-party-notices.ps1).

## Commits and pull requests

- Keep pull requests focused, and open an issue first for larger changes.
- Use [Conventional Commits](https://www.conventionalcommits.org/) for messages: `feat: ...`, `fix: ...`, `docs: ...`, `refactor: ...`, `test: ...`, `build: ...`, `ci: ...`, `chore: ...`.
- Add user-visible changes to `CHANGELOG.md`.

## Protocol and ABI changes

The clients link `rinflow-core` from source. A change to the wire protocol or the C ABI affects every client, so bump the versions as described in [the protocol documentation](https://github.com/rinflow-dev/rinflow-core/blob/main/docs/protocol.md#changing-the-protocol-or-abi) and update the clients in step.

## License

Rinflow is licensed under the [Mozilla Public License 2.0](LICENSE). Unless you state otherwise, your contribution is licensed the same way. New source files start with this notice, in the file's comment syntax:

```
This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at https://mozilla.org/MPL/2.0/.
```

Please do not copy code from projects with incompatible licenses. Other input-sharing projects may be consulted for how they behave, but their code may not be reused unless its license allows it.

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
