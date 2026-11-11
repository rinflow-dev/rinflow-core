# Integrating the core

The C API lives in [include/rinflow_core.h](../include/rinflow_core.h). The macOS app's [RustCore.swift](https://github.com/rinflow-dev/rinflow-macos/blob/main/Rinflow/Core/RustCore.swift) is a complete example of how to use it.

Rust programs can skip the C ABI and use [`rinflow_core::Core`](../src/api.rs) with `CoreConfig` directly. Events arrive as the same JSON strings. The Windows agent's [main.rs](https://github.com/rinflow-dev/rinflow-windows/blob/main/agent/src/main.rs) is an example.

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

## Config JSON

| Field | Default | Notes |
| --- | --- | --- |
| `device_id` | *(required)* | Stable and unique per install. The macOS app stores a UUID in `UserDefaults`. |
| `device_name` | `"Rinflow"` | Shown to peers. Set it to the computer's name. |
| `platform` | `"unknown"` | One of `android`, `macos`, `windows`, `linux`, `unknown`. |
| `control_port` | `42421` | UDP port for connections. |
| `discovery_port` | `42420` | UDP port for announcements. |
| `capabilities` | all supported | Bitmask. Peers without `INPUT_V2` (bit 0) are ignored. |
| `modifier_map` | keys keep their position | What each modifier key of a controlling peer of the other platform convention (Mac ↔ PC) plays as here, like Deskflow's per-screen modifier settings. Keys are named by position: `super` is Command on a Mac and the Windows/Super key on a PC, `alt` is Option. Each plays as `control`, `super`, `alt`, `shift` or `none`; left and right keep their side. `{"super":"control","control":"super","alt":"alt"}` swaps Command and Control: ⌘C copies on a PC and the Mac's Control is the Windows key; the other way, a PC's Ctrl is ⌘. Between two PCs or two Macs keys always keep their position. The peer's platform comes from discovery. Since ABI 10 (replacing `shortcut_remap`); also `lc_core_set_modifier_map` at run time, and keys held through a change are released. The Windows client swaps by default. |
| `manual_addresses` | `[]` | Up to 32 host names or IPv4 addresses (optionally `:port`, the discovery port) to look for devices at besides multicast. See "Devices by address" under [How it works](architecture.md). |
| `displays` | `[]` | This device's displays: `[{"x","y","width","height","scale"}]` in logical points, with `scale` in physical pixels per point. At most 16. Update them later with `lc_core_set_local_displays`. Peers cannot be placed until this is known. |
| `placements` | `[]` | Saved placements, as reported by `placement_changed`: `[{"peer_id":"…","offset":{"x":1440,"y":-11},"updated_ms":…}]`. `offset` is `null` for a removed placement, which still counts as the latest edit when syncing. |
| `identity_path` | `null` | File for this device's private key and self-signed certificate (PEM), created on first start (mode 0600 on Unix). Keep it: paired peers know this device by its certificate. `null` makes a new identity at every start, which only suits tests. |
| `trusted_peers` | `[]` | Paired peers, as reported by `paired`: `[{"peer_id":"…","fingerprint":"<64 hex digits>"}]`. Remove an entry when `unpaired` reports it. |
| `require_pairing` | `false` | Only paired peers may connect: unknown ones pair first. Change it later with `lc_core_set_require_pairing`. |
| `auto_connect` | `true` | Keep the user's devices connected without being asked (see "Staying connected" under [How it works](architecture.md)). The shell needs no reconnect timer of its own. |

## Events (the `json` passed to `LcEventCallback`)

Every event is a JSON object with an `"event"` field.

Every event with a `reason`, and every `error` event, also has `reason_code`: a stable snake_case word to show the user a message in their language (for example `heartbeat_timeout`, `peer_refused`, `pairing_timed_out`, `too_deep`, `control_port_unavailable`). `reason` and `message` stay English, for logs. A reason the core cannot classify (an I/O error, a newer peer's) has the code `other`; show a general message for it. The full list is in [`src/reason.rs`](../src/reason.rs).

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
| `input_latency` | `peer_id`, `ms`, `worst_ms`, `samples`. About once a second while this device drives a peer that echoes input (`InputEcho`): how long input took from `lc_core_send_input` to the peer's core and the echo back, mean and worst of `samples` echoes. |
| `link_quality` | `peer_id`, `rtt_ms`, `sent_packets`, `lost_packets`, `input_batches`, `input_late`, `input_worst_ms`, `stale_repeats`, `round_trip_worst_ms`, `paced`. Sent only for a second in which the link got in the way of input: `lost_packets` of the `sent_packets` this device sent were lost, `input_late` of the `input_batches` the peer sent were held up on the way for at least 20 ms (the worst by `input_worst_ms`; `stale_repeats` key repeats held up for 100 ms or more were not played), or input took 50 ms or more there and back (`round_trip_worst_ms`). `paced`: motion to the peer is paced from now. A log of these is the list of moments the pointer may have stuttered. |
| `manual_addresses` | `addresses`: one entry per address given, `{"address","status"}` with `status` `searching`, `found` (plus `peer_id`, `name`), `no_response` or `unresolved` (plus `error`). Sent when any of it changes. |
| `peer_active` | `peer_id`. Someone uses that device (see "Presence"). |
| `lock_requested` | `peer_id`. That device's user locked it. |
| `peer_pointer_lock` | `peer_id`, `locked`. A game on that peer holds its pointer (or no longer does): while driving it, send all motion and do not cross at its edges. |
| `files_receiving` / `files_sending` / `files_send_ended` | Progress of file transfers on both sides; see [clipboard-and-files.md](clipboard-and-files.md) and the header. |
| `error` | `message` |
| `stopped` | n/a. Also emitted on startup if a socket can't be bound (preceded by an `error`). |

While remote control is not allowed (`lc_core_set_remote_control_allowed(core, false)`), a peer that drives this device is told to give control back (it sees `control_revoked` with `"revoked"`), and so is a peer that starts to; none of their input is played. This device can still drive peers, and sessions and the clipboard stay.

## `LcInputEvent` conventions

- Keyboard events: `code` is the **USB HID usage** (see [HIDKeyMap.swift](https://github.com/rinflow-dev/rinflow-macos/blob/main/Rinflow/Input/HIDKeyMap.swift) for the macOS mapping and [keymap.rs](https://github.com/rinflow-dev/rinflow-windows/blob/main/agent/src/keymap.rs) for Windows). `key_symbol` is an optional Unicode scalar. Keys are physical: receivers play them by key position, so the receiver's own layout and input method decide the character. Repeats are sent by the controlling side as further key-downs with `LC_INPUT_FLAG_KEY_REPEAT`.
- Mouse button events: `code` is a **1-based button number** (1 left, 2 right, 3 middle, 4 back, 5 forward; up to 32), and `buttons` has bit `n − 1` set while button `n` is held.
- Motion: `delta_x` / `delta_y` are **logical points** (pixels at 100 % scale). The receiver multiplies them by its own display scale.
- Arrival: a move with `LC_INPUT_FLAG_ABSOLUTE_POSITION` puts the pointer at `x` / `y` in the **receiver's** logical space, ignoring the deltas. The controlling side sends one when the pointer crosses a portal, so it appears at the matching point of the peer's edge. The core never merges it with other motion.
- Scroll: `scroll_x` / `scroll_y` are logical points. Positive `scroll_y` scrolls toward the top of the content, and positive `scroll_x` toward the left (the macOS signs). `LC_INPUT_FLAG_PRECISE_SCROLL` marks trackpad or smooth-wheel input; without it the values come from a notched wheel at 10 points per line.
- Leave `sequence` at zero. The core assigns it.
- Events with unknown kinds, flags or modifier bits, or with non-finite coordinates, are rejected.
- The struct is exactly 96 bytes, and both the C header and the Rust tests check this.

## Android

When built for Android, the crate also exports JNI functions for `com.rinflow.android.core.runtime.RustRinflowCore`: `nativeCreate`, `nativeDestroy`, `nativeConnect`, `nativeDisconnect`, `nativeSendInput` and `nativeResetInput`. That class has to provide two callbacks: `onNativeEvent(String)` and `onNativeInput(int kind, int flags, int code, int keySymbol, int repeatCount, int modifiers, int buttons, long timestampMicros, double x, double y, double dx, double dy, double scrollX, double scrollY)`. The Android app is not part of this repository.
