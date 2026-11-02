// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#ifndef RINFLOW_CORE_H
#define RINFLOW_CORE_H

#include <stdbool.h>
#include <stdint.h>

/*
 * 3: several sessions at once (one per peer), lc_core_disconnect_peer and
 *    lc_core_set_input_target. Session events carry "peer_id".
 * 4: the arrangement: lc_core_set_local_displays, lc_core_set_placement,
 *    lc_core_place_beside, config keys "displays" and "placements", events
 *    "peer_displays", "placement_changed" and "portals_changed". Wire
 *    protocol version 3.
 * 5: the clipboard: lc_core_set_clipboard, config key "clipboard", events
 *    "clipboard" and "clipboard_sent".
 * 6: clipboards and files travel on a separate data link; files:
 *    lc_core_accept_files, lc_core_cancel_files, config keys "files",
 *    "download_directory", "max_auto_receive_bytes", events "files_offered",
 *    "files_progress", "files_received", "files_failed".
 * 7: only QUIC with TLS 1.3 (no TCP), and pairing: config keys
 *    "identity_path", "trusted_peers", "require_pairing";
 *    lc_core_accept_pairing, lc_core_reject_pairing, lc_core_unpair,
 *    lc_core_set_require_pairing; events "pairing_requested", "paired",
 *    "pairing_ended", "unpaired". Wire protocol version 4, which does not
 *    talk to version 3: every device needs this core.
 * 8: the core keeps the user's devices connected by itself: config key
 *    "auto_connect" (default true); "disconnected" reasons
 *    "disconnected on the other device" and "refused by the other device";
 *    lc_core_system_will_sleep and lc_core_network_changed. See "Staying
 *    connected" below.
 * 9: devices by address (config key "manual_addresses",
 *    lc_core_set_manual_addresses, event "manual_addresses");
 *    shortcut translation at run time; presence (lc_core_report_user_activity,
 *    lc_core_lock_peers, events "peer_active", "lock_requested"); pointer
 *    lock (lc_core_set_pointer_locked, event "peer_pointer_lock"); file
 *    events "files_receiving", "files_sending", "files_send_ended", and
 *    "peer_id" in "files_progress"; lc_core_cancel_files also stops this
 *    device's own files being sent; placements may chain (a peer against
 *    another placed peer). The new messages go only to peers that announce
 *    them, so version 8 cores still connect.
 * 10: the modifier keys of a peer of the other platform convention are
 *    configurable key by key: config key "modifier_map" and
 *    lc_core_set_modifier_map replace "shortcut_remap" and
 *    lc_core_set_shortcut_remap. Nothing changes on the wire. Event
 *    "files_not_shared" when copied files pass the limits (see Files).
 */
#define LC_CORE_ABI_VERSION 10

#ifdef __cplusplus
extern "C" {
#endif

typedef struct LcCore LcCore;

/*
 * Platform-neutral input event.
 *
 * - code: USB HID usage (keyboard page) for key events; one-based button
 *   number for button events (1 left, 2 right, 3 middle, 4 back, 5 forward).
 *   buttons has bit n-1 set while button n is held.
 * - delta_x / delta_y: logical points (pixels at 100 % scale). The receiver
 *   multiplies them by its own display scale.
 * - scroll_x / scroll_y: logical points. Positive scroll_y scrolls toward the
 *   top of the content, positive scroll_x toward the left (macOS signs).
 *   LC_INPUT_FLAG_PRECISE_SCROLL marks trackpad or smooth-wheel input;
 *   without it the values come from a notched wheel at 10 points per line.
 * - A move with LC_INPUT_FLAG_ABSOLUTE_POSITION places the pointer at x / y,
 *   in the receiver's logical space (where it enters the peer); its deltas
 *   are ignored and it is never merged with other motion.
 * - Leave sequence at zero; the core assigns it.
 *
 * Keys keep their position by default: a Mac's Command arrives as the
 * Windows/Super key, Control as Control, Option as Alt, as with Deskflow.
 * The config key "modifier_map" says instead what each modifier key of a
 * controlling peer of the other platform convention plays as here (see
 * remap.rs and lc_core_set_modifier_map).
 */
typedef struct LcInputEvent {
    uint32_t kind;
    uint32_t flags;
    uint32_t code;
    uint32_t key_symbol;
    uint32_t repeat_count;
    uint32_t modifiers;
    uint32_t buttons;
    uint32_t reserved;
    uint64_t sequence;
    uint64_t timestamp_micros;
    double x;
    double y;
    double delta_x;
    double delta_y;
    double scroll_x;
    double scroll_y;
} LcInputEvent;

#if defined(__cplusplus)
static_assert(sizeof(LcInputEvent) == 96, "LcInputEvent ABI layout changed");
#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(LcInputEvent) == 96, "LcInputEvent ABI layout changed");
#endif

typedef void (*LcEventCallback)(const char *json, void *context);
typedef void (*LcInputCallback)(const LcInputEvent *event, void *context);

uint32_t lc_core_abi_version(void);
LcCore *lc_core_create(
    const char *config_json,
    LcEventCallback event_callback,
    LcInputCallback input_callback,
    void *context
);

void lc_core_destroy(LcCore *core);

/* Opens a session with the peer (unless one exists) and makes it the input
 * target. Sessions with other peers stay open. */
bool lc_core_connect(LcCore *core, const char *peer_id);
/* Closes the session with the input target, or every session if none is set. */
bool lc_core_disconnect(LcCore *core);
bool lc_core_disconnect_peer(LcCore *core, const char *peer_id);

/*
 * Staying connected (version 8, config "auto_connect", default true). The
 * core connects by itself to the user's devices: paired peers, and placed
 * peers unless "require_pairing" is on. It connects when such a peer is
 * discovered, and again after its session drops while the peer is still
 * around (1 s, 2 s, 4 s … up to 30 s between failed attempts). The shell
 * needs no timer of its own.
 *   - Automatic attempts are quiet: no "connecting", and no "disconnected"
 *     unless they reached "connected" (or the peer's certificate changed).
 *     They do not change the input target.
 *   - A peer disconnected with lc_core_disconnect_peer / lc_core_disconnect
 *     is left alone until lc_core_connect, or until it connects to this
 *     device. The peer reports "disconnected on the other device" and does
 *     not reconnect by itself either.
 *   - So is a peer after declined or timed-out pairing, a changed identity,
 *     or "refused by the other device" (it would not let this device in).
 *
 * Sleep and network changes (version 8). Report them so sessions end in
 * order and come back at once, instead of after heartbeat timeouts:
 *   lc_core_system_will_sleep   when the system is about to sleep (macOS:
 *       NSWorkspace.willSleepNotification). Every session ends now, so
 *       peers get control back and release held keys; this device's own
 *       "disconnected" events say "going to sleep". Until the system is
 *       asleep (at most 15 s), no connections are made or taken.
 *   lc_core_network_changed     after waking (didWakeNotification) and
 *       whenever the network changes (an address came or went: Wi-Fi
 *       joined, DHCP, VPN; NWPathMonitor). Announces this device, rejoins
 *       the discovery group, dials the user's devices now, and ends within
 *       3 s the sessions that no longer answer. Harmless to call too often.
 */
bool lc_core_system_will_sleep(LcCore *core);
bool lc_core_network_changed(LcCore *core);

/*
 * Devices by address (version 9). Discovery rides on multicast, which guest
 * and company networks, access points that keep clients apart, other
 * subnets and VPNs often do not carry. The user can then add the other
 * device's address: host name or IPv4 address, optionally with the
 * discovery port ("192.168.1.20", "mac-mini.local:42420"). This device
 * sends its announcement straight there and the other device answers, so
 * from then on it is found as if multicast worked (only one of the two
 * needs the other's address). Config key "manual_addresses": [...], or at
 * run time lc_core_set_manual_addresses(core, "[\"192.168.1.20\"]"), which
 * replaces the list. Their state:
 *   {"event":"manual_addresses","addresses":[{"address":"192.168.1.20",
 *    "status":"found","peer_id":…,"name":…}, …]}
 * status: "searching", "found", "no_response", or "unresolved" (with
 * "error").
 */
bool lc_core_set_manual_addresses(LcCore *core, const char *addresses_json);

/*
 * Modifier keys of peers of the other platform convention (Mac <-> PC),
 * also config key "modifier_map": a JSON object naming, for each of the
 * peer's keys by position ("super" is Command on a Mac and the Windows key
 * on a PC, "alt" is Option), what it plays as here: "control", "super",
 * "alt", "shift" or "none" (not played). A missing key keeps its position.
 * Swapping Command and Control, so that a Mac's Command copies on a PC and
 * its Control is the Windows key:
 *   {"super":"control","control":"super","alt":"alt"}
 * Between two PCs or two Macs keys always keep their position. Keys held
 * while it changes are released. False for invalid JSON.
 */
bool lc_core_set_modifier_map(LcCore *core, const char *map_json);

/*
 * Presence (version 9). While someone uses this device's own keyboard or
 * mouse, call lc_core_report_user_activity every few seconds; connected
 * peers then report {"event":"peer_active","peer_id":…} (at most every 20 s
 * per peer) and can keep their screens awake meanwhile, if their users want
 * that. When the user locks this device, lc_core_lock_peers makes peers
 * report {"event":"lock_requested","peer_id":…}; they lock if their users
 * want that. Both decisions belong to the receiving shell's settings.
 */
bool lc_core_report_user_activity(LcCore *core);
bool lc_core_lock_peers(LcCore *core);

/*
 * Pointer lock (version 9). A game or a pointer-lock web page that hides and
 * holds the pointer needs relative motion and no edges. The driven device's
 * shell detects that and calls lc_core_set_pointer_locked(core, true) (and
 * false when it ends); the device driving it then reports
 *   {"event":"peer_pointer_lock","peer_id":…,"locked":true}
 * and keeps the pointer on that peer instead of crossing at its edges until
 * "locked":false (or until it stops driving it). The release shortcut still
 * brings the pointer back.
 */
bool lc_core_set_pointer_locked(LcCore *core, bool locked);
/* Chooses where lc_core_send_input / lc_core_reset_input go. The previous
 * target gets a reset first. NULL drops input until a target is set. When no
 * target was ever set, the first peer to connect becomes the target. */
bool lc_core_set_input_target(LcCore *core, const char *peer_id);
bool lc_core_send_input(LcCore *core, const LcInputEvent *event);
bool lc_core_reset_input(LcCore *core);
/* Lets peers drive this device (the default), or stops them: a peer that
 * drives it, or starts to, is told to give control back (control_revoked on
 * its side) and none of its input is played. This device can still drive
 * peers. Added within version 6.
 *
 * Every connected peer's round trip is reported about once a second:
 *   {"event":"latency","peer_id":…,"ms":…}
 */
bool lc_core_set_remote_control_allowed(LcCore *core, bool allowed);

/*
 * Arrangement (all geometry in logical points: macOS points, or pixels at
 * 100 % scale). A placement is the offset of the peer's desktop in this
 * device's desktop space; the core snaps it so the two desktops touch,
 * reports it as "placement_changed" (save it and pass it back in the
 * "placements" config key next time) and syncs the mirrored placement to the
 * peer. The most recent edit on either side wins.
 *
 * Since version 9 a peer may also sit against another placed peer (three
 * devices in a row, whichever has the keyboard), as long as every placed
 * peer connects to this device through others. Such a chained placement is
 * not synced: the peer it does not touch keeps its own arrangement.
 * "portals_changed" still lists only the portals of this device's own
 * edges; the pointer crosses between placed peers where they touch.
 */
/* displays_json: [{"x":0,"y":0,"width":1512,"height":982,"scale":2}, …] */
bool lc_core_set_local_displays(LcCore *core, const char *displays_json);
/* offset_json: {"x":1512,"y":40}, or NULL to remove the placement. */
bool lc_core_set_placement(LcCore *core, const char *peer_id, const char *offset_json);
/* side: "left", "right", "top" or "bottom". */
bool lc_core_place_beside(LcCore *core, const char *peer_id, const char *side);

/*
 * Clipboard (config key "clipboard": true when the shell implements it).
 * The clipboard follows the pointer: it goes to a peer when this device
 * starts driving it, and a peer's comes back when it gives control back,
 * each only if it changed since the last exchange with that peer.
 *
 * Report the system clipboard after every change:
 *   {"formats":[{"mime":"text/plain;charset=utf-8","data":"<base64>"}, …]}
 * most specific first; "formats":[] when it holds nothing shared (for
 * example contents marked concealed by a password manager). Types:
 * "text/plain;charset=utf-8", "text/html;charset=utf-8", "image/png".
 * At most 8 MiB in all. A peer's clipboard arrives as the event
 *   {"event":"clipboard","peer_id":…,"formats":[…same shape…]}
 * to be written to the system clipboard; that write must not be reported
 * back as a change.
 */
bool lc_core_set_clipboard(LcCore *core, const char *clipboard_json);

/*
 * Files (config: "files": true, which needs "clipboard": true and
 * "download_directory"; optional "max_auto_receive_bytes", default 512 MiB).
 * A zero limit requires explicit acceptance for every offer, including
 * empty files and directories.
 * Desktop: files copied to the system clipboard travel with it.
 *
 * Sending: when the system clipboard holds files, report them in
 * lc_core_set_clipboard as "text/uri-list" (file:// URLs, one per line)
 * next to the other formats (their names as text, for example). The core
 * replaces the local paths with a manifest; a peer that receives it pulls
 * the files from this device while it runs. At most 10 000 files and
 * folders, 32 folders deep; past that the files are not shared (the rest of
 * the clipboard is) and the copying side hears (version 10):
 *   {"event":"files_not_shared","reason":"too_many_files","limit":10000}
 *   {"event":"files_not_shared","reason":"too_deep","limit":32}
 *       tell the user, for example to zip them first
 *
 * Receiving: within the limit, files are fetched at once into the download
 * directory (folders kept, names cleaned, never overwriting). Events:
 *   {"event":"files_offered","peer_id":…,"transfer":N,"count":…,"total_size":…}
 *       over the limit: ask the user, then lc_core_accept_files(core, N)
 *       (or lc_core_cancel_files)
 *   {"event":"files_receiving","peer_id":…,"transfer":N,"name":…,
 *    "items":…,"count":…,"total_size":…}   accepted, under way (version 9):
 *       "name" is the first top-level item, "items" how many there are,
 *       "count" how many files in all
 *   {"event":"files_progress","peer_id":…,"transfer":N,"received":…,"total":…}
 *   {"event":"files_received","peer_id":…,"transfer":N,"paths":[…]}
 *       followed by a "clipboard" event whose formats start with
 *       "text/uri-list" of those paths: write it so pasting gives the files
 *   {"event":"files_failed","peer_id":…,"transfer":N,"reason":…}
 *
 * Sending (version 9), while a peer pulls this device's files:
 *   {"event":"files_sending","peer_id":…,"transfer":N,"name":…,"items":…,
 *    "count":…,"sent":…,"total":…}
 *   {"event":"files_send_ended","peer_id":…,"transfer":N,"reason":"done"}
 *       or another reason when it stopped (cancelled, connection lost)
 * lc_core_cancel_files with that transfer stops it, and the peer.
 */
bool lc_core_accept_files(LcCore *core, uint32_t transfer);
bool lc_core_cancel_files(LcCore *core, uint32_t transfer);

/*
 * Security (version 7). Every connection is QUIC over UDP with TLS 1.3: the
 * control port for sessions, and a second UDP port the system picks for
 * clipboards and files. Both sides show a self-signed certificate, known by
 * its SHA-256 fingerprint. If the identity or the control port cannot be
 * set up, the core reports "error" and "stopped".
 *
 * Config:
 *   "identity_path": file for this device's key and certificate (PEM),
 *       created on first start; keep it, or paired peers no longer know
 *       this device. The fingerprint is in "started": {"fingerprint":…}.
 *   "trusted_peers": [{"peer_id":…,"fingerprint":…}], saved from "paired".
 *   "require_pairing": only paired peers may connect; unknown ones pair
 *       first.
 *
 * Pairing: both devices show the same six digits; each user compares them.
 * The code arrives once the two cores have exchanged their random parts of
 * it, a moment after the connection, and also on a device that already
 * trusts the other one: that device's user confirms too. A peer with an
 * older core cannot pair ("disconnected" with the reason "the other device
 * needs a newer Rinflow to pair"); peers paired before still connect.
 *   {"event":"pairing_requested","peer_id":…,"name":…,"platform":…,
 *    "code":"482917","fingerprint":…}   show the code; then
 *   lc_core_accept_pairing(core, peer_id) or lc_core_reject_pairing.
 *   {"event":"paired","peer_id":…,"fingerprint":…}   save it
 *   {"event":"pairing_ended","peer_id":…,"reason":…}  declined, timed out
 *       or the connection ended: close the dialog
 * The side that was declined reports "pairing declined on the other device".
 * A paired peer whose certificate changed is refused ("disconnected" with
 * the reason "the device's identity changed").
 * "connected" carries "paired" and the peer's "fingerprint".
 */
bool lc_core_accept_pairing(LcCore *core, const char *peer_id);
bool lc_core_reject_pairing(LcCore *core, const char *peer_id);
/* Forgets a paired peer: {"event":"unpaired","peer_id":…}. */
bool lc_core_unpair(LcCore *core, const char *peer_id);
bool lc_core_set_require_pairing(LcCore *core, bool required);

enum {
    LC_INPUT_MOUSE_MOVE = 1,
    LC_INPUT_MOUSE_BUTTON_DOWN = 2,
    LC_INPUT_MOUSE_BUTTON_UP = 3,
    LC_INPUT_SCROLL = 4,
    LC_INPUT_KEY_DOWN = 5,
    LC_INPUT_KEY_UP = 6,
};

enum {
    LC_INPUT_FLAG_KEY_REPEAT = 1 << 0,
    LC_INPUT_FLAG_PRECISE_SCROLL = 1 << 1,
    LC_INPUT_FLAG_ABSOLUTE_POSITION = 1 << 2,
};

enum {
    LC_MODIFIER_SHIFT = 1 << 0,
    LC_MODIFIER_CONTROL = 1 << 1,
    LC_MODIFIER_OPTION = 1 << 2,
    LC_MODIFIER_COMMAND = 1 << 3,
    LC_MODIFIER_CAPS_LOCK = 1 << 4,
    LC_MODIFIER_LEFT_SHIFT = 1 << 8,
    LC_MODIFIER_RIGHT_SHIFT = 1 << 9,
    LC_MODIFIER_LEFT_CONTROL = 1 << 10,
    LC_MODIFIER_RIGHT_CONTROL = 1 << 11,
    LC_MODIFIER_LEFT_OPTION = 1 << 12,
    LC_MODIFIER_RIGHT_OPTION = 1 << 13,
    LC_MODIFIER_LEFT_COMMAND = 1 << 14,
    LC_MODIFIER_RIGHT_COMMAND = 1 << 15,
};

#ifdef __cplusplus
}
#endif

#endif
