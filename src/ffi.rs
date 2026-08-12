use crate::model::{CoreConfig, LcInputEvent};
use crate::runtime::{Command, CoreThread, EventSink};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub type LcEventCallback = Option<extern "C" fn(*const c_char, *mut c_void)>;
pub type LcInputCallback = Option<extern "C" fn(*const LcInputEvent, *mut c_void)>;

pub struct LcCore {
    thread: CoreThread,
}

#[unsafe(no_mangle)]
pub extern "C" fn lc_core_abi_version() -> u32 {
    2
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
