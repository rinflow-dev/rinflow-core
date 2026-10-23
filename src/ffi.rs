use crate::clipboard::Clipboard;
use crate::layout::{Display, Offset, Side};
use crate::model::{CoreConfig, LcInputEvent};
use crate::runtime::{Command, CoreThread, EventSink};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub type LcEventCallback = Option<extern "C" fn(*const c_char, *mut c_void)>;
pub type LcInputCallback = Option<extern "C" fn(*const LcInputEvent, *mut c_void)>;

pub struct LcCore {
    thread: CoreThread,
}

/// Version 6 moves clipboards to a data link and adds files:
/// `lc_core_accept_files`, `lc_core_cancel_files`, config keys `files`,
/// `download_directory`, `max_auto_receive_bytes` and the `files_*` events.
/// Version 7 adds QUIC with TLS and pairing: config keys `identity_path`,
/// `trusted_peers`, `require_pairing`; `lc_core_accept_pairing`,
/// `lc_core_reject_pairing`, `lc_core_unpair`, `lc_core_set_require_pairing`;
/// events `pairing_requested`, `paired`, `pairing_ended`, `unpaired`.
/// Version 8 keeps the user's devices connected (`auto_connect`) and adds
/// `lc_core_system_will_sleep` and `lc_core_network_changed`.
/// Version 9 finds devices by address (config key `manual_addresses`,
/// `lc_core_set_manual_addresses`, event `manual_addresses`), switches
/// shortcut translation at run time (`lc_core_set_shortcut_remap`), adds
/// presence (`lc_core_report_user_activity`, `lc_core_lock_peers`, events
/// `peer_active`, `lock_requested`) and pointer lock
/// (`lc_core_set_pointer_locked`, event `peer_pointer_lock`), events for
/// file transfers on both sides, and chained placements.
#[unsafe(no_mangle)]
pub extern "C" fn lc_core_abi_version() -> u32 {
    9
}

/// Reads a C string argument; `None` for null or invalid UTF-8.
unsafe fn string_argument(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: the caller passes a valid null-terminated string.
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_create(
    config_json: *const c_char,
    event_callback: LcEventCallback,
    input_callback: LcInputCallback,
    context: *mut c_void,
) -> *mut LcCore {
    catch_unwind(AssertUnwindSafe(|| {
        if config_json.is_null() {
            return std::ptr::null_mut();
        }
        let json = unsafe { CStr::from_ptr(config_json) };
        let config = match serde_json::from_slice::<CoreConfig>(json.to_bytes()) {
            Ok(config) => config,
            Err(_) => return std::ptr::null_mut(),
        };

        let context_address = context as usize;
        let event_sink = move |message: String| {
            if let (Some(callback), Ok(message)) = (event_callback, CString::new(message)) {
                callback(message.as_ptr(), context_address as *mut c_void);
            }
        };
        let input_sink = move |event: LcInputEvent| {
            if let Some(callback) = input_callback {
                callback(&event, context_address as *mut c_void);
            }
        };
        let thread = match CoreThread::spawn(config, EventSink::new(event_sink, input_sink)) {
            Ok(thread) => thread,
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(LcCore { thread }))
    }))
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_destroy(core: *mut LcCore) {
    if core.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut core = unsafe { Box::from_raw(core) };
        core.thread.stop_and_join();
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_connect(core: *mut LcCore, peer_id: *const c_char) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() || peer_id.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let peer_id = match unsafe { CStr::from_ptr(peer_id) }.to_str() {
            Ok(peer_id) => peer_id.to_owned(),
            Err(_) => return false,
        };
        core.thread.send(Command::Connect(peer_id)).is_ok()
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_disconnect(core: *mut LcCore) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        core.thread.send(Command::Disconnect).is_ok()
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_disconnect_peer(
    core: *mut LcCore,
    peer_id: *const c_char,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let Some(peer_id) = (unsafe { string_argument(peer_id) }) else {
            return false;
        };
        core.thread.send(Command::DisconnectPeer(peer_id)).is_ok()
    }))
    .unwrap_or(false)
}

/// `peer_id` may be null to stop sending input to anyone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_input_target(
    core: *mut LcCore,
    peer_id: *const c_char,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let target = if peer_id.is_null() {
            None
        } else {
            match unsafe { string_argument(peer_id) } {
                Some(peer_id) => Some(peer_id),
                None => return false,
            }
        };
        core.thread.send(Command::SetInputTarget(target)).is_ok()
    }))
    .unwrap_or(false)
}

/// `displays_json`: a JSON array of `{"x","y","width","height","scale"}` in
/// logical points.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_local_displays(
    core: *mut LcCore,
    displays_json: *const c_char,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let Some(json) = (unsafe { string_argument(displays_json) }) else {
            return false;
        };
        let Ok(displays) = serde_json::from_str::<Vec<Display>>(&json) else {
            return false;
        };
        core.thread
            .send(Command::SetLocalDisplays(displays))
            .is_ok()
    }))
    .unwrap_or(false)
}

/// `offset_json`: `{"x":…,"y":…}` to place the peer near that offset, or
/// null to remove its placement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_placement(
    core: *mut LcCore,
    peer_id: *const c_char,
    offset_json: *const c_char,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let Some(peer_id) = (unsafe { string_argument(peer_id) }) else {
            return false;
        };
        let offset = if offset_json.is_null() {
            None
        } else {
            let Some(json) = (unsafe { string_argument(offset_json) }) else {
                return false;
            };
            match serde_json::from_str::<Offset>(&json) {
                Ok(offset) => Some(offset),
                Err(_) => return false,
            }
        };
        core.thread
            .send(Command::SetPlacement { peer_id, offset })
            .is_ok()
    }))
    .unwrap_or(false)
}

/// `side`: "left", "right", "top" or "bottom".
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_place_beside(
    core: *mut LcCore,
    peer_id: *const c_char,
    side: *const c_char,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let (Some(peer_id), Some(side)) = (unsafe { string_argument(peer_id) }, unsafe {
            string_argument(side)
        }) else {
            return false;
        };
        let Ok(side) = serde_json::from_value::<Side>(serde_json::Value::String(side)) else {
            return false;
        };
        core.thread
            .send(Command::PlaceBeside { peer_id, side })
            .is_ok()
    }))
    .unwrap_or(false)
}

/// `clipboard_json`: `{"formats":[{"mime":…,"data":base64},…]}`, the system
/// clipboard after a change; no formats when it holds nothing shared.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_clipboard(
    core: *mut LcCore,
    clipboard_json: *const c_char,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let Some(json) = (unsafe { string_argument(clipboard_json) }) else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
            return false;
        };
        let Ok(clipboard) = Clipboard::from_json(&value) else {
            return false;
        };
        core.thread.send(Command::SetClipboard(clipboard)).is_ok()
    }))
    .unwrap_or(false)
}

/// Receives the transfer announced by `files_offered`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_accept_files(core: *mut LcCore, transfer: u32) -> bool {
    send_command(core, Command::AcceptFiles(transfer))
}

/// Stops receiving a transfer (offered or under way).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_cancel_files(core: *mut LcCore, transfer: u32) -> bool {
    send_command(core, Command::CancelFiles(transfer))
}

/// Pairing: the user checked the code of `pairing_requested` and accepts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_accept_pairing(core: *mut LcCore, peer_id: *const c_char) -> bool {
    match unsafe { string_argument(peer_id) } {
        Some(peer_id) => send_command(core, Command::AcceptPairing(peer_id)),
        None => false,
    }
}

/// Pairing: the user declines; the connection closes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_reject_pairing(core: *mut LcCore, peer_id: *const c_char) -> bool {
    match unsafe { string_argument(peer_id) } {
        Some(peer_id) => send_command(core, Command::RejectPairing(peer_id)),
        None => false,
    }
}

/// Forgets a paired peer's certificate (`unpaired`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_unpair(core: *mut LcCore, peer_id: *const c_char) -> bool {
    match unsafe { string_argument(peer_id) } {
        Some(peer_id) => send_command(core, Command::Unpair(peer_id)),
        None => false,
    }
}

/// Only paired peers may connect (config key `require_pairing`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_require_pairing(core: *mut LcCore, required: bool) -> bool {
    send_command(core, Command::SetRequirePairing(required))
}

/// The system is about to sleep: sessions end in order now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_system_will_sleep(core: *mut LcCore) -> bool {
    send_command(core, Command::WillSleep)
}

/// The system woke up, or the network changed: find and reconnect peers now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_network_changed(core: *mut LcCore) -> bool {
    send_command(core, Command::NetworkChanged)
}

/// `addresses_json`: a JSON array of strings, host names or IPv4 addresses
/// with an optional discovery port, replacing the earlier list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_manual_addresses(
    core: *mut LcCore,
    addresses_json: *const c_char,
) -> bool {
    let Some(json) = (unsafe { string_argument(addresses_json) }) else {
        return false;
    };
    match serde_json::from_str::<Vec<String>>(&json) {
        Ok(addresses) => send_command(core, Command::SetManualAddresses(addresses)),
        Err(_) => false,
    }
}

/// Translates Command and Control shortcuts from peers of the other
/// platform convention (config key `shortcut_remap`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_shortcut_remap(core: *mut LcCore, enabled: bool) -> bool {
    send_command(core, Command::SetShortcutRemap(enabled))
}

/// Someone used this device's own keyboard or mouse (call every few seconds
/// while that goes on): peers report `peer_active`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_report_user_activity(core: *mut LcCore) -> bool {
    send_command(core, Command::ReportUserActivity)
}

/// The user locked this device: peers report `lock_requested`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_lock_peers(core: *mut LcCore) -> bool {
    send_command(core, Command::LockPeers)
}

/// A game or a pointer-lock page holds this device's pointer, or no longer
/// does: peers driving it report `peer_pointer_lock`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_pointer_locked(core: *mut LcCore, locked: bool) -> bool {
    send_command(core, Command::SetPointerLocked(locked))
}

/// Lets peers drive this device (the default), or stops them. Added within
/// version 6 (nothing else changed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_set_remote_control_allowed(
    core: *mut LcCore,
    allowed: bool,
) -> bool {
    send_command(core, Command::SetRemoteControlAllowed(allowed))
}

fn send_command(core: *mut LcCore, command: Command) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        // SAFETY: a non-null pointer came from lc_core_create.
        let core = unsafe { &*core };
        core.thread.send(command).is_ok()
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_send_input(core: *mut LcCore, event: *const LcInputEvent) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() || event.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        let event = unsafe { *event };
        core.thread.send(Command::SendInput(event)).is_ok()
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lc_core_reset_input(core: *mut LcCore) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return false;
        }
        let core = unsafe { &*core };
        core.thread.send(Command::ResetInput).is_ok()
    }))
    .unwrap_or(false)
}
