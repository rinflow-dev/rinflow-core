// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! JNI bindings for the Android shell (`RustRinflowCore.kt`).
//!
//! The same commands and JSON events as the C ABI in `ffi.rs`, limited to
//! what a device that is driven (never drives) needs. Events reach Kotlin as
//! `onNativeEvent(json)` and a controlling peer's input as
//! `onNativeInput(…)`, both on the core's thread, in the order the core
//! produced them.
//!
//! Kotlin owns the handle: `nativeCreate` hands it out, `nativeDestroy` takes
//! it back exactly once, and no other call runs with that handle after it.

use crate::layout::Display;
use crate::model::{CoreConfig, LcInputEvent};
use crate::runtime::{Command, CoreThread, EventSink};
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{Global, JObject, JString};
use jni::strings::JNIString;
use jni::sys::{jboolean, jdouble, jint, jlong};
use jni::{Env, EnvUnowned, JValue, JavaVM, jni_mangle, jni_sig, jni_str};
use std::sync::Arc;

struct AndroidCore {
    thread: CoreThread,
}

/// The Kotlin object that receives events and input.
struct Callbacks {
    vm: JavaVM,
    owner: Global<JObject<'static>>,
}

impl Callbacks {
    fn event(&self, message: String) {
        // Attaches the core's thread once; later calls are cheap.
        let _ = self.vm.attach_current_thread(|env| {
            let message = env.new_string(message)?;
            env.call_method(
                self.owner.as_obj(),
                jni_str!("onNativeEvent"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(message.as_ref())],
            )?;
            Ok::<(), jni::errors::Error>(())
        });
    }

    fn input(&self, event: LcInputEvent) {
        let _ = self.vm.attach_current_thread(|env| {
            env.call_method(
                self.owner.as_obj(),
                jni_str!("onNativeInput"),
                jni_sig!("(IIIIIIIJDDDDDD)V"),
                &[
                    JValue::Int(event.kind as jint),
                    JValue::Int(event.flags as jint),
                    JValue::Int(event.code as jint),
                    JValue::Int(event.key_symbol as jint),
                    JValue::Int(event.repeat_count as jint),
                    JValue::Int(event.modifiers as jint),
                    JValue::Int(event.buttons as jint),
                    JValue::Long(event.timestamp_micros as jlong),
                    JValue::Double(event.x as jdouble),
                    JValue::Double(event.y as jdouble),
                    JValue::Double(event.delta_x as jdouble),
                    JValue::Double(event.delta_y as jdouble),
                    JValue::Double(event.scroll_x as jdouble),
                    JValue::Double(event.scroll_y as jdouble),
                ],
            )?;
            Ok::<(), jni::errors::Error>(())
        });
    }
}

/// Starts the core with a `CoreConfig` as JSON. Throws
/// `IllegalArgumentException` for a configuration it cannot read and
/// `IllegalStateException` when the core cannot start.
#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_create<'local>(
    mut env: EnvUnowned<'local>,
    owner: JObject<'local>,
    config_json: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let config_json = config_json.try_to_string(env)?;
        let config = match serde_json::from_str::<CoreConfig>(&config_json) {
            Ok(config) => config,
            Err(error) => {
                env.throw_new(
                    jni_str!("java/lang/IllegalArgumentException"),
                    JNIString::from(format!("invalid Rinflow configuration: {error}")),
                )?;
                return Ok(0);
            }
        };
        let callbacks = Arc::new(Callbacks {
            vm: env.get_java_vm()?,
            owner: env.new_global_ref(owner)?,
        });
        let event_callbacks = Arc::clone(&callbacks);
        let sink = EventSink::new(
            move |message| event_callbacks.event(message),
            move |event| callbacks.input(event),
        );
        match CoreThread::spawn(config, sink) {
            Ok(thread) => Ok(Box::into_raw(Box::new(AndroidCore { thread })) as jlong),
            Err(error) => {
                env.throw_new(
                    jni_str!("java/lang/IllegalStateException"),
                    JNIString::from(format!("could not start Rinflow Core: {error}")),
                )?;
                Ok(0)
            }
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Stops the core and waits for it: peers see a clean disconnect, and the
/// releases of whatever a peer still held arrive as input before this
/// returns.
#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_destroy<'local>(mut env: EnvUnowned<'local>, _owner: JObject<'local>, handle: jlong) {
    env.with_env(|_| -> jni::errors::Result<()> {
        if handle != 0 {
            // SAFETY: Kotlin hands each handle from nativeCreate back exactly
            // once and makes no other call with it afterwards.
            let mut core = unsafe { Box::from_raw(handle as *mut AndroidCore) };
            core.thread.stop_and_join();
        }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_connect<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    peer_id: JString<'local>,
) -> jboolean {
    string_command(env, handle, &peer_id, |peer_id| {
        Some(Command::Connect(peer_id))
    })
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_disconnect_peer<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    peer_id: JString<'local>,
) -> jboolean {
    string_command(env, handle, &peer_id, |peer_id| {
        Some(Command::DisconnectPeer(peer_id))
    })
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_accept_pairing<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    peer_id: JString<'local>,
) -> jboolean {
    string_command(env, handle, &peer_id, |peer_id| {
        Some(Command::AcceptPairing(peer_id))
    })
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_reject_pairing<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    peer_id: JString<'local>,
) -> jboolean {
    string_command(env, handle, &peer_id, |peer_id| {
        Some(Command::RejectPairing(peer_id))
    })
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_unpair<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    peer_id: JString<'local>,
) -> jboolean {
    string_command(env, handle, &peer_id, |peer_id| {
        Some(Command::Unpair(peer_id))
    })
}

/// `displays_json`: `[{"x","y","width","height","scale"}, …]` in logical
/// points (dp on Android).
#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_set_local_displays<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    displays_json: JString<'local>,
) -> jboolean {
    string_command(env, handle, &displays_json, |json| {
        serde_json::from_str::<Vec<Display>>(&json)
            .ok()
            .map(Command::SetLocalDisplays)
    })
}

/// Removes the peer's placement here and on the peer (the peers place this
/// device; it never places them).
#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_remove_placement<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    peer_id: JString<'local>,
) -> jboolean {
    string_command(env, handle, &peer_id, |peer_id| {
        Some(Command::SetPlacement {
            peer_id,
            offset: None,
        })
    })
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_set_require_pairing<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    required: jboolean,
) -> jboolean {
    command(env, handle, Command::SetRequirePairing(required))
}

#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_set_remote_control_allowed<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
    allowed: jboolean,
) -> jboolean {
    command(env, handle, Command::SetRemoteControlAllowed(allowed))
}

/// The network changed (or the device woke): announce, rejoin discovery and
/// reconnect now.
#[jni_mangle("com.rinflow.android.core.runtime.RustRinflowCore")]
pub fn native_network_changed<'local>(
    env: EnvUnowned<'local>,
    _owner: JObject<'local>,
    handle: jlong,
) -> jboolean {
    command(env, handle, Command::NetworkChanged)
}

/// Sends the command `build` makes of a string argument. False when the
/// core is not running or `build` refuses the argument (invalid JSON).
fn string_command(
    mut env: EnvUnowned<'_>,
    handle: jlong,
    argument: &JString<'_>,
    build: impl FnOnce(String) -> Option<Command>,
) -> jboolean {
    env.with_env(|env: &mut Env<'_>| -> jni::errors::Result<jboolean> {
        Ok(build(argument.try_to_string(env)?).is_some_and(|command| send(handle, command)))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

fn command(mut env: EnvUnowned<'_>, handle: jlong, command: Command) -> jboolean {
    env.with_env(|_| -> jni::errors::Result<jboolean> { Ok(send(handle, command)) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

fn send(handle: jlong, command: Command) -> jboolean {
    if handle == 0 {
        return false;
    }
    // SAFETY: a non-zero handle came from nativeCreate and Kotlin has not
    // passed it to nativeDestroy yet.
    let core = unsafe { &*(handle as *const AndroidCore) };
    core.thread.send(command).is_ok()
}
