# Changelog

Notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

First public version.

### Added

- LAN discovery over UDP multicast and broadcast, and devices found by address
- QUIC transport with TLS 1.3, and pairing with a six-digit code
- Several sessions at once, input batching and release of held keys on disconnect
- Arrangement sync: placements, portals and chained devices
- Clipboard and on-demand file transfer over a separate data link
- C ABI (version 10) and Android JNI bindings
- A stable `reason_code` beside every event `reason` and `error` message, so shells can show them in the user's language

### Known issues

- Clipboard pushes are limited to 8 MiB. Both ends must agree on the limit, so raising it requires updating every device together.
- Tests that start real cores can occasionally time out during discovery.
