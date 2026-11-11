# Wire protocol

- **Discovery** (UDP, JSON): `{"product":"rinflow","min_protocol_version":4,"max_protocol_version":4,"capabilities":…,"device_id":…,"device_name":…,"platform":…,"control_port":42421,"displays":[{"x":0,"y":0,"width":1512,"height":982,"scale":2}],"fingerprint":"5803…"}`. `fingerprint` is only a hint: the handshake proves the real one.
- **Session** (the QUIC stream; see below): each frame is a big-endian `u32` length (at most 1 MiB) followed by a payload. The payload starts with an 8-byte header: magic `"LC"`, format `1`, message type, then two zero `u16` fields. All fields after that are big-endian.

| Type | Message | Payload after header |
| --- | --- | --- |
| 1 | `Hello` | min/max protocol version, control port, capabilities, device id, device name |
| 2 | `InputBatch` | first sequence (`u64`), count (`u16`, 1–64), then that many 88-byte events |
| 3 | `InputReset` | sequence |
| 4 / 5 | `Ping` / `Pong` | sequence |
| 6 | `ScreenInfo` | count (`u16`, at most 16), then per display `x`, `y`, `width`, `height`, `scale` as `f64` (logical points) |
| 7 | `Placement` | `u8` 1 followed by the offset `x`, `y` (`f64`) of the *receiver* in the sender's space, or `u8` 0 for no placement; then `updated_ms` (`u64`, Unix milliseconds) |
| 14 | `ControlRevoke` | `u8` handoff (0 or 1). Stop driving the sender, and give control back with an `InputReset`. With 1 the sender drives the receiver next. Sent only to peers with capability bit 8 (`CONTROL_REVOKE`). |
| 15 / 16 | `PairRequest` / `PairAccept` | Empty. Declining closes the connection instead (application error code 1). |
| 17 / 18 | `UserActive` / `LockScreen` | Empty. Only to peers with capability bit 9 (`PRESENCE`). |
| 19 | `PointerLock` | `u8` locked (0 or 1), from a driven device to the one driving it. Only to peers with capability bit 10 (`POINTER_LOCK`). |
| 20 | `PairCommit` | 32 bytes: SHA-256 of `"rinflow pairing commitment v2\0"`, the dialer's fingerprint, `\0`, the other side's fingerprint, `\0`, then the dialer's 32-byte nonce. From the side that dialed the connection, while pairing. Capability bit 11 (`PAIRING_V2`). |
| 21 | `PairNonce` | 32 bytes: the other side's nonce, sent once it holds the commitment. |
| 22 | `PairReveal` | 32 bytes: the dialer's nonce, sent once it holds the other side's. |
| 29 | `InputEcho` | The `timestamp_micros` (`u64`) of the newest input event the sender just received, unchanged: from a driven device to the one driving it, at most every 100 ms. Only to peers with capability bit 13 (`INPUT_ECHO`). |

Types 8–13 are the clipboard and file messages of the data link ([clipboard-and-files.md](clipboard-and-files.md)). `DataInvite` (type 9) carries the token (`u64`) and the UDP port (`u16`, never 0) of the core's second endpoint, where the data link goes.

**QUIC**: ALPN `rinflow/1`, TLS 1.3 only, server name `rinflow`. Both sides present a self-signed certificate; the TLS signatures are checked, the certificate chain is not, and the peer is identified by the SHA-256 of its certificate. The dialing side opens one bidirectional stream for the frames, and that is all a peer may open: the accepting side allows one incoming bidirectional stream, the dialing side none, neither allows unidirectional streams or datagrams, and the receive window is 1.25 MB for the stream and for the connection as a whole (anyone can complete the handshake, so nothing a peer sends may pile up unread). Idle timeout 10 s, keep-alive 2 s. On the control connection each side asks the other, with the ACK frequency extension, to acknowledge every 10 packets or after 8 ms (instead of every 2 or after 25 ms): fewer acknowledgements competing with input for air time on Wi-Fi, and a lost last packet of a burst is sent again sooner. A side that closes the connection because its user declined pairing closes it with application error code 1; code 0 is an ordinary close.

Decoding is strict: truncated frames, extra bytes after a message, non-zero reserved fields or any message before `Hello` all close the connection. The encoder and decoder live in [protocol.rs](../src/protocol.rs).

## Changing the protocol or ABI

- The **protocol version** is set by `PROTOCOL_MIN_VERSION` / `PROTOCOL_MAX_VERSION` in [model.rs](../src/model.rs).
- The **C ABI version** is written by hand in two places: `lc_core_abi_version()` in [ffi.rs](../src/ffi.rs) and `LC_CORE_ABI_VERSION` in the header. Bump both together. It is currently **10**:
  - Version 3 added multiple sessions, `lc_core_disconnect_peer`, `lc_core_set_input_target` and the `peer_id` fields.
  - Version 4 added the arrangement: `lc_core_set_local_displays`, `lc_core_set_placement`, `lc_core_place_beside`, the `displays` / `placements` config keys and the `peer_displays` / `placement_changed` events. It came with wire protocol v3, which does not interoperate with v2, so every device needs a build of this core.
  - Versions 5 and 6 added the clipboard, the data link and files (see [clipboard-and-files.md](clipboard-and-files.md)).
  - Version 7 added QUIC with TLS 1.3 and pairing: the `identity_path` / `trusted_peers` / `require_pairing` config keys, `lc_core_accept_pairing`, `lc_core_reject_pairing`, `lc_core_unpair`, `lc_core_set_require_pairing`, the `pairing_requested` / `paired` / `pairing_ended` / `unpaired` events and the new fields of `started` and `connected`. It came with wire protocol v4: QUIC only, no TCP, which does not interoperate with v3.
  - Version 8 keeps the user's devices connected (`auto_connect`, on by default), adds the `disconnected` reasons `"disconnected on the other device"`, `"refused by the other device"` and `"going to sleep"`, and `lc_core_system_will_sleep` / `lc_core_network_changed`. Shells drop their own reconnect timers.
  - Version 9 finds devices by address (`manual_addresses`), switches shortcut translation at run time, adds presence and pointer lock (capability bits 9 and 10, wire messages 17–19), file progress on both sides (`files_receiving`, `files_sending`, `files_send_ended`; `lc_core_cancel_files` also stops sending), and chained placements. Version 8 cores still connect: the new messages go only to peers that announce them, and older cores ignore the `reply` field of a probe (they do not answer it, but are found through their own multicast).
  - Version 10 makes the modifier keys of a peer of the other platform convention configurable key by key: `modifier_map` and `lc_core_set_modifier_map` replace `shortcut_remap` and `lc_core_set_shortcut_remap`. Nothing changes on the wire.
  - Earlier calls keep their meaning.
- The header is maintained by hand (no cbindgen). Any change to `LcInputEvent` or the flag/modifier constants in `model.rs` must be copied into it.
