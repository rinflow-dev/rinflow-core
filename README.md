# Rinflow Core

[![License: MPL-2.0](https://img.shields.io/badge/License-MPL_2.0-brightgreen.svg)](LICENSE)

Rinflow lets several computers on the same local network share one keyboard and mouse. Tell Rinflow which side of your screen the other machine is on, push the cursor past that edge, and your input goes to the other computer.

This repository is the Rust core: LAN discovery, the wire protocol, QUIC transport with TLS 1.3 and pairing, input batching, clipboard and file transfer. It exposes a C ABI and Android JNI bindings. A thin native app on each platform captures and injects input.

> **Status: early MVP (0.1.0).** The macOS and Windows clients connect to each other and share input, the arrangement and, optionally, the clipboard. Expect rough edges; see each client's changelog for known issues.

| Repository | What it is |
| --- | --- |
| [rinflow-core](https://github.com/rinflow-dev/rinflow-core) | This library. |
| [rinflow-macos](https://github.com/rinflow-dev/rinflow-macos) | SwiftUI window and menu bar app for macOS 14 or newer. Captures input with a `CGEvent` tap, injects remote input and statically links the core. |
| [rinflow-windows](https://github.com/rinflow-dev/rinflow-windows) | Windows client: a Rust background agent (core, input capture and injection, edge effect, tray icon, named-pipe IPC) and a C++/WinRT WinUI 3 settings window. |

The clients build the core from source, from a folder named `rinflow-core` beside them, so name the folders when you clone:

```sh
git clone https://github.com/rinflow-dev/rinflow-core.git    rinflow-core
git clone https://github.com/rinflow-dev/rinflow-macos.git   rinflow-macos
git clone https://github.com/rinflow-dev/rinflow-windows.git rinflow-windows
```

## How it works

```
 Mac A (controlling)                                    Mac B (controlled)
┌──────────────────────┐                              ┌──────────────────────┐
│ CGEvent tap          │                              │ CGEvent injection    │
│   │ LcInputEvent     │                              │   ▲ LcInputEvent     │
│   ▼                  │    UDP 42420  discovery      │   │                  │
│ rinflow-core ─────┼──────────────────────────────┼─ rinflow-core     │
│                      │   QUIC 42421  input/control  │                      │
└──────────────────────┘                              └──────────────────────┘
```

- **Discovery.** Every 2 s each core sends a JSON announcement over UDP to the multicast group `239.255.42.99:42420`. It also sends a broadcast fallback for networks that block multicast. The announcement includes the device's displays, so a peer can be drawn to scale before anyone connects. A peer that stays silent for 7 s is dropped.
- **Sessions.** Every device is both server and client: it listens on UDP `42421` and can dial any peer. Every connection is QUIC with TLS 1.3: one bidirectional stream carries length-prefixed frames, and both sides show a certificate (see [Security](#security)). Clipboards and files go over a second QUIC connection, to a second UDP port (see [clipboard-and-files.md](docs/clipboard-and-files.md)). Each side sends a `Hello`, and the two negotiate a protocol version (currently **v4**, and only v4). A core keeps **one session per peer, several at once** (up to 8), and input flows both ways over the same connection. Local input goes to one peer at a time, the *input target*. A new connection never closes an existing session. If the same two devices end up with two connections, one is closed quietly: if the same side dialed twice (it restarted), the newer connection wins; if both sides dialed each other at once, the connection dialed by the lower device ID wins. A connection that has not completed its `Hello` within 8 s is closed.
- **Staying connected.** The core keeps the user's devices connected by itself (`auto_connect`, on by default): paired peers, and placed peers unless `require_pairing` is on. It connects when such a peer is discovered and again after its session drops, while the peer is still announced, waiting 1 s, 2 s, 4 s … up to 30 s between failed attempts; the device with the lower ID dials first. These attempts are quiet (no `connecting`, no `disconnected` unless they got as far as `connected`) and leave the input target alone. A peer the user disconnected, on either side, is left alone until someone connects on purpose: the disconnecting side closes with a code that tells the other side not to reconnect either. The same goes after declined or timed-out pairing, a changed certificate, or a refusal.
- **Sleep and network changes.** The shell reports them; the core does the rest. Before the system sleeps (`lc_core_system_will_sleep`), every session ends in order, so peers get control back and release held keys at once instead of after a heartbeat timeout, and until the system is asleep no connections are made or taken. After waking, and whenever an address comes or goes (`lc_core_network_changed`), the core announces itself, rejoins the multicast group, dials the user's devices without waiting for backoff, and ends within 3 s the sessions that no longer answer; sessions that survived (QUIC follows an address change) carry on. Joining the multicast group is not required to start: a device that starts before its network is up uses broadcast and joins later.
- **Input.** The platform app turns native events into the fixed-layout `LcInputEvent` struct and hands them to the core. The core assigns sequence numbers and batches up to 64 events per frame. It merges consecutive mouse moves and scrolls but never reorders clicks or key presses. Control messages are interleaved, so heavy mouse movement can't block heartbeats.
- **No stuck keys.** The receiver tracks which keys and buttons are held down. If the connection drops, a sequence gap appears, or the sender resets, the core generates key-up and button-up events for everything still held.
- **One controller at a time.** A device is never driven by two peers at once, and never driven while it drives another. The newest controller wins, as in lan-mouse:
  - When a peer starts driving this device, the peer that drove it before is told to stop (`ControlRevoke`), and so is this device's own control of another peer.
  - When someone uses this device's own keyboard and mouse to push the pointer into a peer, whoever drove this device is told to stop. If that is the same peer, control simply changes hands.
  - A device told to stop gives control back with its usual reset, and its shell ends the capture (`control_revoked`). Until that reset arrives, anything else it sends is ignored, and until its shell resets or picks a new target, its own input goes nowhere.
  - A peer without the `CONTROL_REVOKE` capability (an older build) is never sent the message. Its input is ignored until it gives control back by itself.
- **Devices by address.** Discovery rides on multicast (and broadcast), which guest and company networks, access points that keep clients apart, other subnets and VPNs often do not carry. A shell can give the core addresses to look at as well (`manual_addresses`, `lc_core_set_manual_addresses`): host names or IPv4 addresses, optionally with the discovery port. Every two seconds the core sends its announcement straight there, asking for an answer (`"reply": true`), and the other device answers the same way, so from then on it is found, connected and kept connected as if multicast worked; one of the two devices needing the other's address is enough. Answers are rate-limited per sender and no larger than the probe. The `manual_addresses` event reports each address as `searching`, `found`, `no_response` or `unresolved`.
- **Presence.** While someone uses a device, its shell calls `lc_core_report_user_activity` every few seconds, and peers hear `peer_active` (at most every 20 s): they can keep their screens awake meanwhile. `lc_core_lock_peers` makes peers report `lock_requested` when the user locks a device, for locking together. What to do is each receiving shell's setting. Both messages (`UserActive`, `LockScreen`) go only to peers with capability bit 9 (`PRESENCE`).
- **Pointer lock.** A game or a pointer-lock web page on a driven device hides and holds its pointer and wants relative motion with no edges. That device's shell reports it (`lc_core_set_pointer_locked`), and the device driving it hears `peer_pointer_lock`: it then sends all motion and stops crossing at that peer's edges until the lock ends. The message (`PointerLock`) goes only to peers with capability bit 10 (`POINTER_LOCK`), and is sent again whenever a new control period starts.
- **Liveness.** Each side pings every second and disconnects after 8 s without a pong. Outgoing connection attempts time out after 4 s.
- **Arrangement.** Each device places its peers around its own screens, and the core keeps both sides' views of it consistent. See [Arrangement](#arrangement).

### Arrangement

Every device describes its displays in **logical points** (macOS points, or pixels at 100 % scale) in its own desktop space. A **placement** puts a peer's whole desktop into this device's space at an offset. Both sides use the same unit, so the peer's view of the same arrangement is the offset negated: if a Windows PC puts a Mac at `(1440, -11)`, the Mac has the PC at `(-1440, 11)`.

```
 this device's space (points)
 (0,0) ┌──────────┐┌─────────────┐  ← peer placed at offset (1440, -11)
       │ display  ││ peer        │
       │ 1440×960 ││ 1512×982    │     the touching segment is a portal:
       └──────────┘└─────────────┘     the pointer crosses there, point for point
```

- **Portals.** Wherever an edge of a local display touches an edge of a placed peer's display, the shared segment is a portal. That is where the pointer crosses over, so placing a device higher or lower also chooses which part of the edge leads to it.
- **Snapping (core).** The shell only proposes a position (for example where a dragged device was dropped). The core moves it to the nearest spot where the two desktops touch with at least 64 points of shared edge, without overlapping this device's displays or other placed peers. Every peer display is tried against every local edge, so peers whose displays differ in size (an L shape) touch with the display that is really there, not their bounding box. A spot where another of the peer's displays would collide is also tried moved along the edge just clear of it, and among spots about as near, the one sharing more edge wins (snug fits over hovering gaps). Within 24 points of an alignment (edges flush, or centres lined up), it snaps to that alignment. `place_beside` centres the peer on one side.
- **Chains (ABI 9).** A peer may also sit against another placed peer instead of this device, so three devices can stand in a row whichever one has the keyboard (`layout::snap_in_arrangement`). Every placed peer must connect to this device through others: a placement whose anchor moved or went away is fitted again against what remains. The pointer crosses from one peer straight into the next where they touch (`RemotePointer::step` reports the neighbour; the shell retargets its input).
- **Sync.** Placements travel as `Placement` messages, and displays as `ScreenInfo`, when a session starts and on every change. Each placement carries the time it was made, and the more recent edit on either side wins (an exact tie goes to the lower device ID). Removing a placement syncs the same way. A chained placement is not synced: the peer it does not touch has no spot for this device, and keeps its own arrangement.
- **Display changes.** When a monitor is added, removed or rescaled, the shell passes the new list to the core. The core tells connected peers, and moves any placement that no longer touches the desktop to the nearest valid spot. A saved placement is also checked when the peer's displays first become known (for example after a restart with different monitors). Until then the peer's size is unknown, so the check waits.
- **Persistence (shell).** The core reports every change as `placement_changed`. The shell saves those reports and passes them back in the `placements` config key at the next start.

## Prerequisites

- **Rust 1.88 or newer**, installed via [rustup](https://rustup.rs). The crate uses edition 2024 and `let` chains.
- **Firewall:** allow UDP `42420` (discovery) and UDP `42421` (connections) on every machine, plus the UDP port the core binds for clipboard and file transfers, which the system picks. Rinflow uses no TCP. A rule for the program rather than for ports covers all of them.

## Building

```sh
cargo build             # debug build
cargo build --release   # optimized build (thin LTO, symbols stripped)
cargo test
```

Each build produces three outputs: a Rust library, a static library (`librinflow_core.a` / `rinflow_core.lib`) and a dynamic library (`.dylib` / `.so` / `.dll`), all under `target/`.

Several tests, such as `two_cores_discover_connect_and_exchange_input` and the pairing tests, start two real cores that find each other over UDP multicast and connect over QUIC. They need a network interface that allows multicast loopback, and the first run may trigger an OS firewall prompt. Because they depend on the machine's network, they can occasionally time out with `peers not discovered`; rerun before treating that as a regression.

## Integrating the core on a new platform

The C API lives in [include/rinflow_core.h](include/rinflow_core.h). The macOS app's [RustCore.swift](https://github.com/rinflow-dev/rinflow-macos/blob/main/Rinflow/Core/RustCore.swift) is a complete example of how to use it.

Rust programs can skip the C ABI and use [`rinflow_core::Core`](src/api.rs) with `CoreConfig` directly. Events arrive as the same JSON strings. The Windows agent's [main.rs](https://github.com/rinflow-dev/rinflow-windows/blob/main/agent/src/main.rs) is an example.

```c
if (lc_core_abi_version() != LC_CORE_ABI_VERSION) { /* mismatched library */ }

LcCore *core = lc_core_create(config_json, on_event, on_input, user_context);
// NULL means invalid JSON, a failed config check, or the core thread failed to start.

lc_core_connect(core, peer_id);      // open a session (unless one exists) and make it the input target
lc_core_send_input(core, &event);    // forward a captured local event to the input target
lc_core_reset_input(core);           // the target releases everything held: control is given back
lc_core_set_input_target(core, other_peer_id); // switch targets; the old one gets a reset first
lc_core_disconnect_peer(core, peer_id);        // close one session; others stay open
lc_core_disconnect(core);            // close the input target's session (all sessions if none is set)
lc_core_set_remote_control_allowed(core, false); // peers may not drive this device (sharing paused)

// Pairing (after "pairing_requested"; see Security)
lc_core_accept_pairing(core, peer_id);   // the user saw the same code on both devices
lc_core_reject_pairing(core, peer_id);   // declined: the connection closes
lc_core_unpair(core, peer_id);           // forget a paired peer
lc_core_set_require_pairing(core, true); // only paired peers from now on

// Sleep and network (see "Sleep and network changes")
lc_core_system_will_sleep(core);         // about to sleep: sessions end in order now
lc_core_network_changed(core);           // woke up, or an address came or went

// ABI 9
lc_core_set_manual_addresses(core, "[\"192.168.1.20\"]"); // also look for devices there
lc_core_report_user_activity(core);      // someone uses this device (every few seconds)
lc_core_lock_peers(core);                // the user locked this device
lc_core_set_pointer_locked(core, true);  // a game here holds the pointer

// ABI 10: what a Mac's modifier keys play as on a PC (or a PC's on a Mac)
lc_core_set_modifier_map(core, "{\"super\":\"control\",\"control\":\"super\",\"alt\":\"alt\"}");

// Arrangement (logical points)
lc_core_set_local_displays(core, "[{\"x\":0,\"y\":0,\"width\":1512,\"height\":982,\"scale\":2}]");
lc_core_place_beside(core, peer_id, "right");                 // centred on that side
lc_core_set_placement(core, peer_id, "{\"x\":1512,\"y\":40}"); // snapped to a valid spot
lc_core_set_placement(core, peer_id, NULL);                    // remove the placement

lc_core_destroy(core);               // stops the core and waits for its threads to exit
```

Every call except `lc_core_destroy` only queues a command and returns right away. The work happens on the core's own threads. Input, target changes and resets keep the order you call them in, so input sent right after `lc_core_set_input_target` goes to the new target. If no target was ever set, the first peer to connect becomes the target, which matches the single-session behavior of ABI version 2.

**Threading.** Both callbacks run on the core's internal threads, so hand the work to your UI thread. The `json` string and the `LcInputEvent *` are only valid for the duration of the callback, so copy them if you need them later.

### Config JSON

| Field | Default | Notes |
| --- | --- | --- |
| `device_id` | *(required)* | Stable and unique per install. The macOS app stores a UUID in `UserDefaults`. |
| `device_name` | `"Rinflow"` | Shown to peers. Set it to the computer's name. |
| `platform` | `"unknown"` | One of `android`, `macos`, `windows`, `linux`, `unknown`. |
| `control_port` | `42421` | UDP port for connections. |
| `discovery_port` | `42420` | UDP port for announcements. |
| `capabilities` | all supported | Bitmask. Peers without `INPUT_V2` (bit 0) are ignored. |
| `modifier_map` | keys keep their position | What each modifier key of a controlling peer of the other platform convention (Mac ↔ PC) plays as here, like Deskflow's per-screen modifier settings. Keys are named by position: `super` is Command on a Mac and the Windows/Super key on a PC, `alt` is Option. Each plays as `control`, `super`, `alt`, `shift` or `none`; left and right keep their side. `{"super":"control","control":"super","alt":"alt"}` swaps Command and Control: ⌘C copies on a PC and the Mac's Control is the Windows key; the other way, a PC's Ctrl is ⌘. Between two PCs or two Macs keys always keep their position. The peer's platform comes from discovery. Since ABI 10 (replacing `shortcut_remap`); also `lc_core_set_modifier_map` at run time, and keys held through a change are released. The Windows client swaps by default. |
| `manual_addresses` | `[]` | Up to 32 host names or IPv4 addresses (optionally `:port`, the discovery port) to look for devices at besides multicast. See "Devices by address" under [How it works](#how-it-works). |
| `displays` | `[]` | This device's displays: `[{"x","y","width","height","scale"}]` in logical points, with `scale` in physical pixels per point. At most 16. Update them later with `lc_core_set_local_displays`. Peers cannot be placed until this is known. |
| `placements` | `[]` | Saved placements, as reported by `placement_changed`: `[{"peer_id":"…","offset":{"x":1440,"y":-11},"updated_ms":…}]`. `offset` is `null` for a removed placement, which still counts as the latest edit when syncing. |
| `identity_path` | `null` | File for this device's private key and self-signed certificate (PEM), created on first start (mode 0600 on Unix). Keep it: paired peers know this device by its certificate. `null` makes a new identity at every start, which only suits tests. |
| `trusted_peers` | `[]` | Paired peers, as reported by `paired`: `[{"peer_id":"…","fingerprint":"<64 hex digits>"}]`. Remove an entry when `unpaired` reports it. |
| `require_pairing` | `false` | Only paired peers may connect: unknown ones pair first. Change it later with `lc_core_set_require_pairing`. |
| `auto_connect` | `true` | Keep the user's devices connected without being asked (see "Staying connected" under [How it works](#how-it-works)). The shell needs no reconnect timer of its own. |

### Events (the `json` passed to `LcEventCallback`)

Every event is a JSON object with an `"event"` field:

| `event` | Extra fields |
| --- | --- |
| `started` | `device_id`, `control_port`, `discovery_port`, `fingerprint` (this device's certificate, 64 hex digits) |
| `peer_up` / `peer_down` | `peer` (`id`, `name`, `platform`, `address`, `port`, `capabilities`, `displays`) / `peer_id` |
| `connecting` | `peer_id`. Only for connects the shell asked for (`lc_core_connect`); automatic ones are announced by `connected`. |
| `connected` | `peer`, `protocol_version`, `capabilities`, `paired` (the peer showed the certificate it was paired with), `fingerprint` (the peer's). Repeated if you call `lc_core_connect` for a peer that is already connected. |
| `disconnected` | `peer_id`, `reason`: `"local request"` when you closed it, `"disconnected on the other device"` when the peer's user did, `"peer closed the connection"` when the peer quit, `"refused by the other device"` when the peer would not let this device in, one of the security reasons below, otherwise the failure. Only sessions whose peer is known report this. A connection that never identified itself, a duplicate closed in favor of another connection to the same peer, or an automatic attempt that never reached `connected`, does not. |
| `pairing_requested` | `peer_id`, `name`, `platform`, `code` (six digits), `fingerprint`. Show the code and ask the user; answer with `lc_core_accept_pairing` or `lc_core_reject_pairing`. |
| `paired` | `peer_id`, `fingerprint`. Both users accepted; save it for `trusted_peers`. A `connected` follows. |
| `pairing_ended` | `peer_id`, `reason`. A pairing ended without `paired`: declined on either side, timed out (two minutes), or the connection was lost. Close the dialog. |
| `unpaired` | `peer_id`. After `lc_core_unpair`; remove it from `trusted_peers`. |
| `peer_displays` | `peer_id`, `displays`. A peer described its displays, from discovery or a `ScreenInfo` message, and they differ from what was known. |
| `placement_changed` | `peer_id`, `offset` (`{"x","y"}` or `null`), `updated_ms`, `source`: `"local"` for a change made on this device (including a re-snap after a display change), `"peer"` for one received from the peer. Save it for the `placements` config key. |
| `portals_changed` | `portals`: every segment where the pointer crosses into a placed peer, as `{"peer_id","side","local_display","peer_display","line","from","to"}` in this device's space. `line` is the edge's x (left/right) or y (top/bottom), and `from..to` its extent along the edge. Sent whenever placements or either side's displays change, so a shell can decide edge crossings without its own geometry. |
| `input_reset` | `peer_id`, `reason`, `released_event_count`: synthetic key-up/button-up events were delivered |
| `input_sequence_gap` | `peer_id`, `expected`, `received` |
| `remote_control_started` | `peer_id`. That peer started driving this device: the first input since its session began or since its last reset. |
| `remote_control_ended` | `peer_id`, `reason`. The peer gave control back (it sends a reset when it releases), or its session ended, or another controller took over (`"taken over by another device"`, `"taken back locally"`). Emitted after the synthetic key-ups, so every key and button has already been released. |
| `control_revoked` | `peer_id`, `reason`, `by`. This device's control of `peer_id` ended without the shell asking; the shell must end its capture. `reason` is `"taken_over"` when a peer (`by`) now drives this device and places the pointer itself, so leave the cursor where it is, or `"revoked"` when another device took `peer_id` over, so bring the pointer back as after a release. The peer has already been reset; the core drops this device's input until the shell calls `lc_core_reset_input` or sets a target. |
| `latency` | `peer_id`, `ms`: the round trip of the last ping to a connected peer (pings go out about once a second). |
| `manual_addresses` | `addresses`: one entry per address given, `{"address","status"}` with `status` `searching`, `found` (plus `peer_id`, `name`), `no_response` or `unresolved` (plus `error`). Sent when any of it changes. |
| `peer_active` | `peer_id`. Someone uses that device (see "Presence"). |
| `lock_requested` | `peer_id`. That device's user locked it. |
| `peer_pointer_lock` | `peer_id`, `locked`. A game on that peer holds its pointer (or no longer does): while driving it, send all motion and do not cross at its edges. |
| `files_receiving` / `files_sending` / `files_send_ended` | Progress of file transfers on both sides; see [clipboard-and-files.md](docs/clipboard-and-files.md) and the header. |
| `error` | `message` |
| `stopped` | n/a. Also emitted on startup if a socket can't be bound (preceded by an `error`). |

While remote control is not allowed (`lc_core_set_remote_control_allowed(core, false)`), a peer that drives this device is told to give control back (it sees `control_revoked` with `"revoked"`), and so is a peer that starts to; none of their input is played. This device can still drive peers, and sessions and the clipboard stay.

### `LcInputEvent` conventions

- Keyboard events: `code` is the **USB HID usage** (see [HIDKeyMap.swift](https://github.com/rinflow-dev/rinflow-macos/blob/main/Rinflow/Input/HIDKeyMap.swift) for the macOS mapping and [keymap.rs](https://github.com/rinflow-dev/rinflow-windows/blob/main/agent/src/keymap.rs) for Windows). `key_symbol` is an optional Unicode scalar. Keys are physical: receivers play them by key position, so the receiver's own layout and input method decide the character. Repeats are sent by the controlling side as further key-downs with `LC_INPUT_FLAG_KEY_REPEAT`.
- Mouse button events: `code` is a **1-based button number** (1 left, 2 right, 3 middle, 4 back, 5 forward; up to 32), and `buttons` has bit `n − 1` set while button `n` is held.
- Motion: `delta_x` / `delta_y` are **logical points** (pixels at 100 % scale). The receiver multiplies them by its own display scale.
- Arrival: a move with `LC_INPUT_FLAG_ABSOLUTE_POSITION` puts the pointer at `x` / `y` in the **receiver's** logical space, ignoring the deltas. The controlling side sends one when the pointer crosses a portal, so it appears at the matching point of the peer's edge. The core never merges it with other motion.
- Scroll: `scroll_x` / `scroll_y` are logical points. Positive `scroll_y` scrolls toward the top of the content, and positive `scroll_x` toward the left (the macOS signs). `LC_INPUT_FLAG_PRECISE_SCROLL` marks trackpad or smooth-wheel input; without it the values come from a notched wheel at 10 points per line.
- Leave `sequence` at zero. The core assigns it.
- Events with unknown kinds, flags or modifier bits, or with non-finite coordinates, are rejected.
- The struct is exactly 96 bytes, and both the C header and the Rust tests check this.

### Android

When built for Android, the crate also exports JNI functions for `com.rinflow.android.core.runtime.RustRinflowCore`: `nativeCreate`, `nativeDestroy`, `nativeConnect`, `nativeDisconnect`, `nativeSendInput` and `nativeResetInput`. That class has to provide two callbacks: `onNativeEvent(String)` and `onNativeInput(int kind, int flags, int code, int keySymbol, int repeatCount, int modifiers, int buttons, long timestampMicros, double x, double y, double dx, double dy, double scrollX, double scrollY)`. The Android app is not part of this repository.

## Wire protocol (reference)

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

Types 8–13 are the clipboard and file messages of the data link ([clipboard-and-files.md](docs/clipboard-and-files.md)). `DataInvite` (type 9) carries the token (`u64`) and the UDP port (`u16`, never 0) of the core's second endpoint, where the data link goes.

**QUIC**: ALPN `rinflow/1`, TLS 1.3 only, server name `rinflow`. Both sides present a self-signed certificate; the TLS signatures are checked, the certificate chain is not, and the peer is identified by the SHA-256 of its certificate. The dialing side opens one bidirectional stream for the frames, and that is all a peer may open: the accepting side allows one incoming bidirectional stream, the dialing side none, neither allows unidirectional streams or datagrams, and the receive window is 1.25 MB for the stream and for the connection as a whole (anyone can complete the handshake, so nothing a peer sends may pile up unread). Idle timeout 10 s, keep-alive 2 s. A side that closes the connection because its user declined pairing closes it with application error code 1; code 0 is an ordinary close.

Decoding is strict: truncated frames, extra bytes after a message, non-zero reserved fields or any message before `Hello` all close the connection. The encoder and decoder live in [protocol.rs](src/protocol.rs).

### Changing the protocol or ABI

- The **protocol version** is set by `PROTOCOL_MIN_VERSION` / `PROTOCOL_MAX_VERSION` in [model.rs](src/model.rs).
- The **C ABI version** is written by hand in two places: `lc_core_abi_version()` in [ffi.rs](src/ffi.rs) and `LC_CORE_ABI_VERSION` in the header. Bump both together. It is currently **10**:
  - Version 3 added multiple sessions, `lc_core_disconnect_peer`, `lc_core_set_input_target` and the `peer_id` fields.
  - Version 4 added the arrangement: `lc_core_set_local_displays`, `lc_core_set_placement`, `lc_core_place_beside`, the `displays` / `placements` config keys and the `peer_displays` / `placement_changed` events. It came with wire protocol v3, which does not interoperate with v2, so every device needs a build of this core.
  - Versions 5 and 6 added the clipboard, the data link and files (see [clipboard-and-files.md](docs/clipboard-and-files.md)).
  - Version 7 added QUIC with TLS 1.3 and pairing: the `identity_path` / `trusted_peers` / `require_pairing` config keys, `lc_core_accept_pairing`, `lc_core_reject_pairing`, `lc_core_unpair`, `lc_core_set_require_pairing`, the `pairing_requested` / `paired` / `pairing_ended` / `unpaired` events and the new fields of `started` and `connected`. It came with wire protocol v4: QUIC only, no TCP, which does not interoperate with v3.
  - Version 8 keeps the user's devices connected (`auto_connect`, on by default), adds the `disconnected` reasons `"disconnected on the other device"`, `"refused by the other device"` and `"going to sleep"`, and `lc_core_system_will_sleep` / `lc_core_network_changed`. Shells drop their own reconnect timers.
  - Version 9 finds devices by address (`manual_addresses`), switches shortcut translation at run time, adds presence and pointer lock (capability bits 9 and 10, wire messages 17–19), file progress on both sides (`files_receiving`, `files_sending`, `files_send_ended`; `lc_core_cancel_files` also stops sending), and chained placements. Version 8 cores still connect: the new messages go only to peers that announce them, and older cores ignore the `reply` field of a probe (they do not answer it, but are found through their own multicast).
  - Version 10 makes the modifier keys of a peer of the other platform convention configurable key by key: `modifier_map` and `lc_core_set_modifier_map` replace `shortcut_remap` and `lc_core_set_shortcut_remap`. Nothing changes on the wire.
  - Earlier calls keep their meaning.
- The header is maintained by hand (no cbindgen). Any change to `LcInputEvent` or the flag/modifier constants in `model.rs` must be copied into it.

## Security

A connected peer can **inject keyboard and mouse input**, so the core encrypts and authenticates connections, and can limit them to devices the user paired.

- **Identity.** Each device has a self-signed certificate and key (`identity_path`), made on first start. Peers know it by the SHA-256 fingerprint of the certificate. There is no certificate authority: trust comes from pairing.
- **Encryption.** Every connection is QUIC with TLS 1.3, for input and control as well as clipboard and files. Each side learns the other's fingerprint during the handshake. A data link is attached to a session only if it shows the same certificate.
- **Admission.** After the peer's `Hello`:

  | Peer | Result |
  | --- | --- |
  | In `trusted_peers` with the same fingerprint | Let in (`paired: true`). |
  | In `trusted_peers` with another fingerprint | Refused: `"the device's identity changed"`. |
  | Unknown, `require_pairing` on | Pairs first. |
  | Unknown, `require_pairing` off | Let in, encrypted but not authenticated. It can still ask to pair. |

- **Pairing.** The side that wants it sends `PairRequest`, and both sides pair, also one that already trusts the other: a user can only check a code against the other screen. The code comes from both fingerprints and a random 32-byte nonce from each side. The side that dialed the connection commits to its nonce first (`PairCommit`), the other side answers with its own (`PairNonce`), and the dialer then reveals its nonce (`PairReveal`), which has to match the commitment. The code is SHA-256 of `"rinflow pairing code v2\0"`, the dialer's fingerprint, `\0`, the other side's fingerprint, `\0`, the dialer's nonce and the other side's nonce; the first four bytes as a number modulo 10⁶. Since no side can change its nonce after seeing the other's, a device in the middle, which has to run a pairing with each side under its own certificate, gets the same code on both screens only one time in a million. Each shell shows the code (`pairing_requested`), and the user accepts once the other screen shows the same code. When both sides have accepted, each reports `paired` and lets the peer in. A peer without capability bit 11 (`PAIRING_V2`, an older build, whose code came from the fingerprints alone and could be forged) cannot pair (`"the other device needs a newer Rinflow to pair"`), though devices paired before still connect. A pairing message out of turn or a reveal that does not match its commitment ends the connection (`"pairing failed"`). Declining (`"pairing declined"`, and `"pairing declined on the other device"` on the other side) or two minutes without an answer (`"pairing timed out"`) closes the connection. A device that sends input before it is let in is ignored.
- **Turning `require_pairing` on** closes sessions with peers that are not paired (`"pairing required"`). With it on, `lc_core_unpair` closes the peer's session too (`"unpaired"`).
- **What is still exposed.** Discovery announcements are plain UDP and tell the whole network each device's name, ID and fingerprint. With `require_pairing` off, any Rinflow device on the LAN can connect (encrypted, but nobody checked who it is). The Windows client turns `require_pairing` on by default.

## Not built yet

- Pointer motion as unreliable QUIC datagrams, so a lost packet never holds up later ones
- An Android app on top of the existing JNI bindings

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Please report security problems as described in [SECURITY.md](SECURITY.md).

## License

Rinflow is licensed under the [Mozilla Public License 2.0](LICENSE).
