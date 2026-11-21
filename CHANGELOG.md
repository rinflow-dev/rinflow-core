# Changelog

Notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Pictures: a device with one (`media_source`, such as a KVM) sends encoded frames to the peers that watch it (`media`) over a media link of its own, where frames late by 100 ms are dropped rather than queued, and each link is reported four times a second (`media_link`) so the source can fit its encoder to it
- Sound with a picture, either way, over the media link in datagrams (`send_media_audio`, `send_media_audio_to`, `set_media_audio_sink`; in the C ABI the watching side's `lc_core_set_media_audio_callback` and `lc_core_send_media_audio_to`): what the computer behind a KVM plays, and a microphone for it
- ChaCha20-Poly1305 is preferred on processors without AES instructions
- A frame of a picture is shared with QUIC rather than copied (`MediaFrame::data` is `Bytes`), and a device with one processor runs one I/O thread instead of two: 15 % less processor time for the same picture on a 1 GHz RISC-V core
- Attached screens: a KVM (`attachable`) placed right beside a device counts among that device's displays for every other peer, and while a peer drives the device, input on that screen is passed on to the KVM without giving up control (`lc_core_set_relay_target`, `lc_core_relay_input`)
- Device settings: a device without a screen of its own (a KVM) says what it is set to (`lc_core_set_device_settings`, event `peer_settings`), and a paired peer asks it to change (`lc_core_change_device_settings`, event `settings_change_requested`); the core carries the JSON unread
- Identify displays: the device where the user asks numbers the displays of its arrangement, and each connected peer shows its numbers on its own screens (`lc_core_identify_displays`, event `identify_displays`)
- Waking a device over the network (Wake-on-LAN): devices tell each other the hardware addresses that wake them (`lc_core_set_wake_addresses`, event `peer_wake`), `lc_wake_send` sends the magic packets out of every interface, and a device going to sleep says so (event `peer_asleep`, before the usual `disconnected`)

## [0.0.1] - 2026-11-16

First public version.

### Added

- LAN discovery over UDP multicast and broadcast, and devices found by address
- QUIC transport with TLS 1.3, and pairing with a six-digit code
- Several sessions at once, input batching and release of held keys on disconnect
- Arrangement sync: placements, portals and chained devices
- Clipboard and on-demand file transfer over a separate data link
- C ABI (version 11) and Android JNI bindings
- Smoother pointer on busy links: while a link is in trouble, motion is sent at most every 4 ms (merged in between) instead of as it comes; acknowledgements of input are fewer and sooner; and the core's threads run above normal priority
- Key repeats held up on the way for 100 ms or more are not played, so a held Backspace or arrow key no longer overshoots after a stall
- `link_quality`: an event for every second in which packets were lost or input arrived late, to tell why a pointer stuttered; `input_latency`: how long input takes to a driven peer and back
- A stable `reason_code` beside every event `reason` and `error` message, so shells can show them in the user's language
- On-demand files in the C ABI (version 11): with `on_demand_files`, a peer's copied files come as `files_available` and are read only when pasted (`lc_core_open_remote_files`, `lc_file_lease_read`, `lc_file_lease_close`, `lc_core_revoke_file_streams`), as the Rust API already allowed

### Changed

- A message type from a newer peer is skipped and fields it appended to a message are ignored, instead of closing the connection, so a newer device never cuts off an older one (docs/protocol.md, Compatibility)
- Discovery announces out of every IPv4 interface and listens on each, instead of only the one the system routes the group to, and replaces its socket when it stops working (`discovery_reset`): a Mac with Ethernet, Wi-Fi and a VPN no longer ended up unable to find or be found by anyone after a network change, until restarted

### Known issues

- Clipboard pushes are limited to 8 MiB. Both ends must agree on the limit, so raising it requires updating every device together.
- Tests that start real cores can occasionally time out during discovery.
