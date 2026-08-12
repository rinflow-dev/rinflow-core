#ifndef LINKCURSOR_CORE_H
#define LINKCURSOR_CORE_H

#include <stdbool.h>
#include <stdint.h>

#define LC_CORE_ABI_VERSION 2

#ifdef __cplusplus
extern "C" {
#endif

typedef struct LcCore LcCore;

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
bool lc_core_connect(LcCore *core, const char *peer_id);
bool lc_core_disconnect(LcCore *core);
bool lc_core_send_input(LcCore *core, const LcInputEvent *event);
bool lc_core_reset_input(LcCore *core);

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
