#ifndef LINKCURSOR_CORE_H
#define LINKCURSOR_CORE_H

#include <stdbool.h>
#include <stdint.h>

/*
 * 3: several sessions at once (one per peer), lc_core_disconnect_peer and
 *    lc_core_set_input_target. Session events carry "peer_id".
 * 4: the arrangement: lc_core_set_local_displays, lc_core_set_placement,
 *    lc_core_place_beside, config keys "displays" and "placements", events
 *    "peer_displays" and "placement_changed". Wire protocol version 3.
 */
#define LC_CORE_ABI_VERSION 4

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
 * - Leave sequence at zero; the core assigns it.
 *
 * Keys keep their position by default: a Mac's Command arrives as the
 * Windows/Super key, Control as Control, Option as Alt, as with Deskflow.
 * With the config key "shortcut_remap" on, Command and Control are instead
 * translated when the controlling peer uses the other platform's shortcut
 * convention (see remap.rs).
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
/* Chooses where lc_core_send_input / lc_core_reset_input go. The previous
 * target gets a reset first. NULL drops input until a target is set. When no
 * target was ever set, the first peer to connect becomes the target. */
bool lc_core_set_input_target(LcCore *core, const char *peer_id);
bool lc_core_send_input(LcCore *core, const LcInputEvent *event);
bool lc_core_reset_input(LcCore *core);

/*
 * Arrangement (all geometry in logical points: macOS points, or pixels at
 * 100 % scale). A placement is the offset of the peer's desktop in this
 * device's desktop space; the core snaps it so the two desktops touch,
 * reports it as "placement_changed" (save it and pass it back in the
 * "placements" config key next time) and syncs the mirrored placement to the
 * peer. The most recent edit on either side wins.
 */
/* displays_json: [{"x":0,"y":0,"width":1512,"height":982,"scale":2}, …] */
bool lc_core_set_local_displays(LcCore *core, const char *displays_json);
/* offset_json: {"x":1512,"y":40}, or NULL to remove the placement. */
bool lc_core_set_placement(LcCore *core, const char *peer_id, const char *offset_json);
/* side: "left", "right", "top" or "bottom". */
bool lc_core_place_beside(LcCore *core, const char *peer_id, const char *side);

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
