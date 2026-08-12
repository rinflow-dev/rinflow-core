use crate::model::{CoreConfig, LcInputEvent};
use crate::runtime::{Command, CoreThread, EventSink};
use jni::objects::{Global, JObject, JString};
use jni::strings::JNIString;
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jdouble, jint, jlong};
use jni::{EnvUnowned, JValue, JavaVM, jni_sig, jni_str};
use std::sync::Arc;

struct AndroidCore {
    thread: CoreThread,
}

struct AndroidCallbacks {
    vm: JavaVM,
    owner: Global<JObject<'static>>,
}

impl AndroidCallbacks {
    fn event(&self, message: String) {
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

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_linkcursor_android_core_runtime_RustLinkCursorCore_nativeCreate<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    owner: JObject<'caller>,
    config_json: JString<'caller>,
) -> jlong {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jlong> {
            let config_json = config_json.to_string();
            let config = match serde_json::from_str::<CoreConfig>(&config_json) {
                Ok(config) => config,
                Err(error) => {
                    env.throw_new(
                        jni_str!("java/lang/IllegalArgumentException"),
                        JNIString::from(format!("invalid LinkCursor configuration: {error}")),
                    )?;
                    return Ok(0);
                }
            };

            let callbacks = Arc::new(AndroidCallbacks {
                vm: env.get_java_vm()?,
                owner: env.new_global_ref(owner)?,
            });
            let event_callbacks = Arc::clone(&callbacks);
            let input_callbacks = Arc::clone(&callbacks);
            let sink = EventSink::new(
                move |message| event_callbacks.event(message),
                move |event| input_callbacks.input(event),
            );
            let thread = match CoreThread::spawn(config, sink) {
                Ok(thread) => thread,
                Err(error) => {
                    env.throw_new(
                        jni_str!("java/lang/IllegalStateException"),
                        JNIString::from(format!("could not start LinkCursor Core: {error}")),
                    )?;
                    return Ok(0);
                }
            };
            Ok(Box::into_raw(Box::new(AndroidCore { thread })) as jlong)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_linkcursor_android_core_runtime_RustLinkCursorCore_nativeDestroy<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _owner: JObject<'caller>,
    handle: jlong,
) {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<()> {
            if handle != 0 {
                // SAFETY: Kotlin transfers each non-zero handle exactly once and clears it
                // before invoking this method, so this reconstructs the originating Box.
                let mut core = unsafe { Box::from_raw(handle as *mut AndroidCore) };
                core.thread.stop_and_join();
            }
            Ok(())
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_linkcursor_android_core_runtime_RustLinkCursorCore_nativeConnect<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _owner: JObject<'caller>,
    handle: jlong,
    peer_id: JString<'caller>,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let result = with_core(handle, |core| {
                core.thread.send(Command::Connect(peer_id.to_string()))
            });
            Ok(boolean(result.is_ok()))
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_linkcursor_android_core_runtime_RustLinkCursorCore_nativeDisconnect<
    'caller,
>(
    unowned_env: EnvUnowned<'caller>,
    _owner: JObject<'caller>,
    handle: jlong,
) -> jboolean {
    command(unowned_env, handle, Command::Disconnect)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_linkcursor_android_core_runtime_RustLinkCursorCore_nativeResetInput<
    'caller,
>(
    unowned_env: EnvUnowned<'caller>,
    _owner: JObject<'caller>,
    handle: jlong,
) -> jboolean {
    command(unowned_env, handle, Command::ResetInput)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_linkcursor_android_core_runtime_RustLinkCursorCore_nativeSendInput<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _owner: JObject<'caller>,
    handle: jlong,
    kind: jint,
    flags: jint,
    code: jint,
    key_symbol: jint,
    repeat_count: jint,
    modifiers: jint,
    buttons: jint,
    timestamp_micros: jlong,
    x: jdouble,
    y: jdouble,
    delta_x: jdouble,
    delta_y: jdouble,
    scroll_x: jdouble,
    scroll_y: jdouble,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let event = LcInputEvent {
                kind: kind as u32,
                flags: flags as u32,
                code: code as u32,
                key_symbol: key_symbol as u32,
                repeat_count: repeat_count as u32,
                modifiers: modifiers as u32,
                buttons: buttons as u32,
                reserved: 0,
                sequence: 0,
                timestamp_micros: timestamp_micros as u64,
                x,
                y,
                delta_x,
                delta_y,
                scroll_x,
                scroll_y,
            };
            Ok(boolean(
                with_core(handle, |core| core.thread.send(Command::SendInput(event))).is_ok(),
            ))
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

fn command<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    handle: jlong,
    command: Command,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(boolean(
                with_core(handle, |core| core.thread.send(command)).is_ok(),
            ))
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

fn with_core<T>(
    handle: jlong,
    action: impl FnOnce(&AndroidCore) -> Result<T, String>,
) -> Result<T, String> {
    if handle == 0 {
        return Err("native core is not running".to_owned());
    }
    // SAFETY: handles are created by nativeCreate and remain owned by Kotlin until
    // nativeDestroy. Public Kotlin operations serialize access with a Mutex.
    let core = unsafe { &*(handle as *const AndroidCore) };
    action(core)
}

fn boolean(value: bool) -> jboolean {
    if value { JNI_TRUE } else { JNI_FALSE }
}
