# Rinflow Core

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

Rinflow shares one keyboard and mouse between computers on a local network. Move the pointer past a screen edge to control the next device.

This repository is the Rust library the clients build on. It handles device discovery, the wire protocol, encrypted connections, pairing, and clipboard and file transfer, and exposes a C ABI. Each client captures and injects input natively:

- [rinflow-macos](https://github.com/rinflow-dev/rinflow-macos)
- [rinflow-windows](https://github.com/rinflow-dev/rinflow-windows)

**Status:** early development. There are no binary releases yet.

## Building

Requires Rust 1.88 or newer.

```sh
cargo build --release
cargo test
```

The build produces a Rust library, a static library and a dynamic library under `target/`. The clients expect this repository in a folder next to theirs, named `rinflow-core`.

Some tests start real cores that find each other over UDP multicast, so they need a network interface that allows multicast loopback.

## Documentation

- [Architecture](docs/architecture.md): discovery, sessions, input and arrangement
- [C API](docs/c-api.md): calls, configuration, events and input conventions
- [Wire protocol](docs/protocol.md)
- [Security design](docs/security.md)
- [Clipboard and files](docs/clipboard-and-files.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). To report a vulnerability, see [SECURITY.md](SECURITY.md).

## License

[Mozilla Public License 2.0](LICENSE)
