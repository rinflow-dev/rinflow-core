# Pictures

A device with a picture sends it to the peers that watch it. The first is a KVM (`rinflow-nanokvm`): the picture is the screen of the computer behind it. The core carries encoded frames between the shells and does not look inside; the module comment of [media.rs](../src/media.rs) has the detail.

| | Config | Calls | Events |
| --- | --- | --- | --- |
| **Source** | `media_source` | `Core::send_media_frame(key_frame, timestamp_micros, data)` | `media_viewers` (`peer_ids`: who watches now), `media_key_frame_wanted` |
| **Watching** | `media` (capability bit 14, `MEDIA`) | `Core::set_media_sink`, `watch`, `unwatch`, `request_media_key_frame` | `media_available` / `media_unavailable` (`peer_id`), `media_started`, `media_ended` (`peer_id`, `reason`) |

Both are off by default, and a core without either binds no third port and behaves as before. The C ABI has the same calls: `lc_core_set_media_callback`, `lc_core_watch`, `lc_core_unwatch`, `lc_core_request_media_key_frame` and `lc_core_send_media_frame` (see the header); `media` and `media_source` are config keys.

## The media link

A third QUIC connection per watching peer, to a third endpoint on a UDP port of the system's choosing, tagged as video for the network (after input, before clipboards and files: see `qos.rs`).

1. A source sends `MediaOffer` (type 30: token `u64`, port `u16`) on the control connection to every established peer with capability bit 14.
2. To watch, the peer dials that port (same ALPN and certificates as every connection) and opens one bidirectional stream: the token (`u64`, little endian), the length of its device ID (`u16`, little endian) and the ID.
3. The source accepts only a token it gave out, from the device it gave it to, on a connection showing that session's certificate, and answers one byte (1). The watching side likewise checks the certificate against the session's.
4. Frames follow, each on a unidirectional stream of its own: one byte of flags (bit 0: key frame), the sequence (`u64`) and the time stamp in microseconds (`u64`), both little endian, then the frame up to the end of the stream (at most 8 MiB).
5. The watching side writes `K` on its stream to ask for a key frame. Closing the connection ends watching; so does the end of the session.

The watching side allows 16 unidirectional streams at once and 8 MiB of unread data; the source allows the one bidirectional stream and nothing else.

## Late frames are dropped, not queued

A frame waits while the link is busy with the one before. When three wait for one peer, all are dropped and frames are skipped up to the next key frame, which the source's shell is asked for (`media_key_frame_wanted`). A new viewer also starts at a key frame. So a link that carries less than the encoder puts out shows a picture that stutters but never falls behind, and a decoder never gets a frame whose predecessor it missed.

## Encryption on small processors

On a processor without AES instructions (RISC-V, 32-bit ARM) ChaCha20-Poly1305 is preferred for every connection, as dialer and as accepting side. Measured on a 1 GHz RISC-V core (a NanoKVM) sending 4 Mbit/s of frames: 13 % of the core, against 23 % with AES-128-GCM. Nothing changes on x86-64 and ARM64.
