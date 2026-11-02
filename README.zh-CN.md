# Rinflow Core

[English](README.md) | 简体中文 | [日本語](README.ja.md)

Rinflow 让局域网内的多台电脑共用一套键盘和鼠标：把指针移过屏幕边缘，就能控制下一台设备。

本仓库是各客户端共用的 Rust 库，负责设备发现、通信协议、加密连接、配对以及剪贴板和文件传输，并提供 C ABI。各客户端以原生方式捕获和注入输入：

- [rinflow-macos](https://github.com/rinflow-dev/rinflow-macos)
- [rinflow-windows](https://github.com/rinflow-dev/rinflow-windows)

**状态：** 早期开发阶段，暂无二进制发布版本。

## 构建

需要 Rust 1.88 或更高版本。

```sh
cargo build --release
cargo test
```

构建产物包括 Rust 库、静态库和动态库，位于 `target/` 下。客户端要求本仓库放在其同级目录，文件夹名为 `rinflow-core`。

部分测试会启动真实的核心实例，通过 UDP 组播互相发现，因此需要支持组播回环的网络接口。

## 文档（英文）

- [架构](docs/architecture.md)：设备发现、会话、输入和设备排列
- [C API](docs/c-api.md)：调用、配置、事件和输入约定
- [通信协议](docs/protocol.md)
- [安全设计](docs/security.md)
- [剪贴板和文件](docs/clipboard-and-files.md)（中文）

## 参与贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)。报告安全漏洞见 [SECURITY.md](SECURITY.md)。

## 许可证

[Mozilla Public License 2.0](LICENSE)
