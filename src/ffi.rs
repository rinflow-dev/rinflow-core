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

/// Version 4 adds the arrangement: display descriptions and placements.
#[unsafe(no_mangle)]
pub extern "C" fn lc_core_abi_version() -> u32 {
    4
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
