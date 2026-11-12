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
- Smoother pointer on busy links: while a link is in trouble, motion is sent at most every 4 ms (merged in between) instead of as it comes; acknowledgements of input are fewer and sooner; and the core's threads run above normal priority
- Key repeats held up on the way for 100 ms or more are not played, so a held Backspace or arrow key no longer overshoots after a stall
- `link_quality`: an event for every second in which packets were lost or input arrived late, to tell why a pointer stuttered; `input_latency`: how long input takes to a driven peer and back
- A stable `reason_code` beside every event `reason` and `error` message, so shells can show them in the user's language
- Pictures: a device with one (`media_source`, such as a KVM) sends encoded frames to the peers that watch it (`media`) over a media link of its own, where late frames are dropped rather than queued
- ChaCha20-Poly1305 is preferred on processors without AES instructions

### Known issues

- Clipboard pushes are limited to 8 MiB. Both ends must agree on the limit, so raising it requires updating every device together.
- Tests that start real cores can occasionally time out during discovery.
