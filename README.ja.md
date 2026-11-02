# Rinflow Core

[English](README.md) | [简体中文](README.zh-CN.md) | 日本語

Rinflow は、ローカルネットワーク上の複数のコンピューターで 1 組のキーボードとマウスを共有するソフトウェアです。ポインターを画面の端の外へ動かすと、隣のデバイスを操作できます。

このリポジトリは、各クライアントが共通で使う Rust ライブラリです。デバイスの検出、通信プロトコル、暗号化接続、ペアリング、クリップボードとファイルの転送を担当し、C ABI を提供します。入力のキャプチャと注入は各クライアントがネイティブに行います。

- [rinflow-macos](https://github.com/rinflow-dev/rinflow-macos)
- [rinflow-windows](https://github.com/rinflow-dev/rinflow-windows)

**状態：** 開発初期の段階です。バイナリのリリースはまだありません。

## ビルド

Rust 1.88 以降が必要です。

```sh
cargo build --release
cargo test
```

ビルドすると、Rust ライブラリ、静的ライブラリ、動的ライブラリが `target/` 以下に生成されます。クライアントは、このリポジトリが隣のフォルダーに `rinflow-core` という名前で置かれていることを前提としています。

一部のテストは実際のコアを起動し、UDP マルチキャストで互いを検出します。そのため、マルチキャストのループバックに対応したネットワークインターフェースが必要です。

## ドキュメント（英語）

- [アーキテクチャ](docs/architecture.md)：デバイスの検出、セッション、入力、デバイスの配置
- [C API](docs/c-api.md)：呼び出し、設定、イベント、入力の規約
- [通信プロトコル](docs/protocol.md)
- [セキュリティ設計](docs/security.md)
- [クリップボードとファイル](docs/clipboard-and-files.md)（中国語）

## コントリビュート

[CONTRIBUTING.md](CONTRIBUTING.md) をご覧ください。脆弱性の報告については [SECURITY.md](SECURITY.md) をご覧ください。

## ライセンス

[Mozilla Public License 2.0](LICENSE)
