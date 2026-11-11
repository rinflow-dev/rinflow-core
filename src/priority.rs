// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scheduling priority of the core's network threads.
//!
//! Input passes through them on its way out and in. At the default priority
//! they wait their turn behind whatever else the device is busy with (a
//! build, a game, a browser), a scheduling slice at a time, which is 10 to
//! 15 ms on Windows: the pointer on the other device stutters while this one
//! works hard. They do little work themselves, so they may go first.
//!
//! - Windows: one step above normal.
//! - macOS and iOS: the user-interactive class, which also keeps them on
//!   the performance cores.
//! - Elsewhere nothing: raising a thread's priority needs privileges there.

/// Raises the calling thread. Best effort: a thread that cannot be raised
/// runs as before.
pub fn raise_current_thread() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        // SAFETY: changes the priority of the calling thread only.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: changes the class of the calling thread only.
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
}

/// While one lives, this process's timers fire to the millisecond.
///
/// Windows otherwise rounds every timed wait up to its clock tick, 15.6 ms
/// unless some program asked for less: motion paced 4 ms apart (see
/// `MOTION_INTERVAL` in `transport.rs`) would go out 15 ms apart, which
/// shows. Asking costs a little power, so it is held only while input is
/// being sent. Other systems time waits finely enough as they are.
pub struct FineTimers(());

impl FineTimers {
    pub fn new() -> Self {
        #[cfg(windows)]
        // SAFETY: a request counted per process, undone when this drops.
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        Self(())
    }
}

impl Drop for FineTimers {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: ends the request made in `new`.
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}
