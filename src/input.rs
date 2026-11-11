// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::model::{
    LC_INPUT_FLAG_ABSOLUTE_POSITION, LC_INPUT_FLAG_KEY_REPEAT, LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP,
    LC_INPUT_MOUSE_BUTTON_DOWN, LC_INPUT_MOUSE_BUTTON_UP, LcInputEvent, MAX_INPUT_BATCH_SIZE,
    WireMessage,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const SOFT_QUEUE_LIMIT: usize = 1024;
const HARD_QUEUE_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Queued,
    Coalesced,
    DroppedCoalescible,
}

/// What the platform shell queued, in the order it called the core. Target
/// changes and resets travel in the same queue as input, so input sent right
/// after a target change goes to the new target and nothing sent after a
/// reset is discarded by it.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformEntry {
    Event(LcInputEvent),
    SetTarget(Option<String>),
    Reset,
}

#[derive(Clone)]
pub struct PlatformInputQueue {
    shared: Arc<Mutex<PlatformInputState>>,
}

struct PlatformInputState {
    entries: VecDeque<PlatformEntry>,
    notification_pending: bool,
}

impl PlatformInputQueue {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(PlatformInputState {
                entries: VecDeque::new(),
                notification_pending: false,
            })),
        }
    }

    /// Returns whether the runtime must be notified that input is available.
    pub fn push(&self, mut event: LcInputEvent) -> Result<(PushOutcome, bool), &'static str> {
        event.validate()?;
        event.sequence = 0;
        event.reserved = 0;
        // When the shell handed it over, on one clock for every shell: the
        // receiving core tells from it how late input arrives (see
        // `InputTiming`).
        event.timestamp_micros = monotonic_micros();
        let mut state = self.shared.lock().expect("platform input queue poisoned");

        let outcome = if let Some(PlatformEntry::Event(previous)) = state.entries.back_mut()
            && coalesce(previous, &event)
        {
            PushOutcome::Coalesced
        } else {
            if state.entries.len() >= SOFT_QUEUE_LIMIT {
                if let Some(index) = state.entries.iter().position(
                    |entry| matches!(entry, PlatformEntry::Event(event) if event.is_coalescible()),
                ) {
                    state.entries.remove(index);
                } else if event.is_coalescible() {
                    return Ok((PushOutcome::DroppedCoalescible, false));
                }
            }
            if state.entries.len() >= HARD_QUEUE_LIMIT {
                return Err("platform critical input queue is full");
            }
            state.entries.push_back(PlatformEntry::Event(event));
            PushOutcome::Queued
        };

        Ok((outcome, Self::notify(&mut state)))
    }

    /// Queues a target change or reset behind the input already queued.
    /// Returns whether the runtime must be notified.
    pub fn push_control(&self, entry: PlatformEntry) -> bool {
        let mut state = self.shared.lock().expect("platform input queue poisoned");
        state.entries.push_back(entry);
        Self::notify(&mut state)
    }

    fn notify(state: &mut PlatformInputState) -> bool {
        let should_notify = !state.notification_pending;
        state.notification_pending = true;
        should_notify
    }

    pub fn drain(&self) -> Vec<PlatformEntry> {
        let mut state = self.shared.lock().expect("platform input queue poisoned");
        state.notification_pending = false;
        state.entries.drain(..).collect()
    }
}

#[derive(Clone)]
pub struct OutboundInputQueue {
    shared: Arc<OutboundShared>,
}

struct OutboundShared {
    state: Mutex<OutboundState>,
    notify: Notify,
}

struct OutboundState {
    entries: VecDeque<ScheduledInput>,
    next_sequence: u64,
}

enum ScheduledInput {
    Event(LcInputEvent),
    Reset,
}

impl OutboundInputQueue {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(OutboundShared {
                state: Mutex::new(OutboundState {
                    entries: VecDeque::new(),
                    next_sequence: 1,
                }),
                notify: Notify::new(),
            }),
        }
    }

    pub fn push(&self, mut event: LcInputEvent) -> Result<PushOutcome, &'static str> {
        event.validate()?;
        // Sequence numbers describe what was actually written, not every raw
        // platform event observed before pointer coalescing.
        event.sequence = 0;
        event.reserved = 0;

        let mut state = self.shared.state.lock().expect("input queue poisoned");
        if let Some(ScheduledInput::Event(previous)) = state.entries.back_mut()
            && coalesce(previous, &event)
        {
            drop(state);
            self.shared.notify.notify_one();
            return Ok(PushOutcome::Coalesced);
        }

        if state.entries.len() >= SOFT_QUEUE_LIMIT {
            if let Some(index) = state.entries.iter().position(
                |entry| matches!(entry, ScheduledInput::Event(event) if event.is_coalescible()),
            ) {
                state.entries.remove(index);
            } else if event.is_coalescible() {
                return Ok(PushOutcome::DroppedCoalescible);
            }
        }
        if state.entries.len() >= HARD_QUEUE_LIMIT {
            return Err("critical input queue is full");
        }

        state.entries.push_back(ScheduledInput::Event(event));
        drop(state);
        self.shared.notify.notify_one();
        Ok(PushOutcome::Queued)
    }

    /// Discards unsent input and appends a reset marker. A reset is stronger
    /// than the stale input it replaces and guarantees that the receiver can
    /// release every pressed key and pointer button.
    pub fn reset(&self) {
        let mut state = self.shared.state.lock().expect("input queue poisoned");
        state.entries.clear();
        state.entries.push_back(ScheduledInput::Reset);
        drop(state);
        self.shared.notify.notify_one();
    }

    /// Only relative motion and scrolling wait, which merge with what comes
    /// next (see `coalesce`): they may be held back for a moment, unlike
    /// keys, buttons, resets and the absolute move that places the pointer.
    pub fn only_motion(&self) -> bool {
        let state = self.shared.state.lock().expect("input queue poisoned");
        !state.entries.is_empty()
            && state.entries.iter().all(|entry| {
                matches!(entry, ScheduledInput::Event(event)
                    if event.is_coalescible()
                        && event.flags & LC_INPUT_FLAG_ABSOLUTE_POSITION == 0)
            })
    }

    pub fn pop_message(&self) -> Option<WireMessage> {
        let mut state = self.shared.state.lock().expect("input queue poisoned");
        let first = state.entries.pop_front()?;
        match first {
            ScheduledInput::Reset => {
                let sequence = take_sequence(&mut state);
                Some(WireMessage::InputReset { sequence })
            }
            ScheduledInput::Event(mut event) => {
                let first_sequence = take_sequence(&mut state);
                event.sequence = first_sequence;
                let mut events = vec![event];
                while events.len() < MAX_INPUT_BATCH_SIZE {
                    let Some(ScheduledInput::Event(_)) = state.entries.front() else {
                        break;
                    };
                    let Some(ScheduledInput::Event(mut event)) = state.entries.pop_front() else {
                        unreachable!("front entry checked")
                    };
                    event.sequence = take_sequence(&mut state);
                    events.push(event);
                }
                Some(WireMessage::InputBatch {
                    first_sequence,
                    events,
                })
            }
        }
    }

    pub async fn notified(&self) {
        self.shared.notify.notified().await;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("input queue poisoned")
            .entries
            .len()
    }
}

/// Microseconds on a clock that only moves forward, from an arbitrary start.
pub(crate) fn monotonic_micros() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    // Never 0, which a shell leaves in the field.
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
        + 1
}

fn take_sequence(state: &mut OutboundState) -> u64 {
    let sequence = state.next_sequence;
    state.next_sequence = state.next_sequence.checked_add(1).unwrap_or(1);
    sequence
}

fn coalesce(previous: &mut LcInputEvent, next: &LcInputEvent) -> bool {
    // An absolute move places the pointer (for example where it enters the
    // peer); merged with relative motion its position would be lost.
    let absolute = |event: &LcInputEvent| event.flags & LC_INPUT_FLAG_ABSOLUTE_POSITION != 0;
    if previous.is_motion() && next.is_motion() && !absolute(previous) && !absolute(next) {
        previous.flags = next.flags;
        previous.modifiers = next.modifiers;
        previous.buttons = next.buttons;
        previous.timestamp_micros = next.timestamp_micros;
        previous.x = next.x;
        previous.y = next.y;
        previous.delta_x += next.delta_x;
        previous.delta_y += next.delta_y;
        return true;
    }
    if previous.is_scroll() && next.is_scroll() {
        previous.flags |= next.flags;
        previous.modifiers = next.modifiers;
        previous.buttons = next.buttons;
        previous.timestamp_micros = next.timestamp_micros;
        previous.x = next.x;
        previous.y = next.y;
        previous.scroll_x += next.scroll_x;
        previous.scroll_y += next.scroll_y;
        return true;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceStatus {
    InOrder,
    Duplicate,
    Gap { expected: u64, received: u64 },
}

/// How late input from a peer arrives: for the `link_quality` event, and to
/// drop key repeats that come too late to mean anything.
///
/// The sending core stamps every event when its shell hands it over. Two
/// devices' clocks cannot be compared, but the difference between this
/// device's clock at arrival and the peer's stamp can be compared with
/// itself: it is smallest for input that came straight through, and input
/// whose difference is 60 ms above that was held up for 60 ms on the way (a
/// lost packet sent again, a busy Wi-Fi channel, a busy thread on either
/// device). That is the stall a user sees as a pointer that stops and then
/// jumps, and it holds for every event of the burst that follows a stall,
/// not only the first.
///
/// The smallest difference is that of the last minute or so, because the
/// two clocks drift apart by a few milliseconds a minute.
#[derive(Default)]
pub struct InputTiming {
    /// What this device's clock counts from.
    origin: Option<std::time::Instant>,
    /// The smallest difference (arrival minus stamp, in microseconds) in the
    /// current stretch of [`Self::STRETCH`] and in the one before.
    smallest: [Option<i64>; 2],
    /// When the current stretch began.
    stretch_began: Option<std::time::Instant>,
    batches: u32,
    late: u32,
    worst_micros: u64,
    stale_repeats: u32,
}

/// What [`InputTiming`] saw since it was last asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputTimingReport {
    /// Batches of input that arrived.
    pub batches: u32,
    /// Those held up for at least [`InputTiming::LATE_MICROS`].
    pub late: u32,
    /// The longest any was held up, in microseconds.
    pub worst_micros: u64,
    /// Key repeats dropped because they came too late.
    pub stale_repeats: u32,
}

impl InputTiming {
    /// A batch held up this long counts as late: more than a frame at 60 Hz.
    pub const LATE_MICROS: u64 = 20_000;
    /// A key repeat held up this long is not played. A held key repeats
    /// every 30 to 80 ms; one this late has later ones right behind it, and
    /// playing them all at once deletes or moves further than the user held
    /// the key for. The key going down, and up, is always played.
    pub const STALE_REPEAT_MICROS: u64 = 100_000;
    const STRETCH: std::time::Duration = std::time::Duration::from_secs(30);

    /// How long the event stamped `stamp` by the peer, arriving at `now`,
    /// was held up on the way, in microseconds. `None` for an event without
    /// a stamp.
    pub fn held_up(&mut self, stamp: u64, now: std::time::Instant) -> Option<u64> {
        if stamp == 0 {
            return None;
        }
        let origin = *self.origin.get_or_insert(now);
        if self
            .stretch_began
            .is_none_or(|began| now.saturating_duration_since(began) >= Self::STRETCH)
        {
            self.stretch_began = Some(now);
            self.smallest = [None, self.smallest[0]];
        }
        let arrival = now.saturating_duration_since(origin).as_micros() as i64;
        let difference = arrival.saturating_sub(stamp.min(i64::MAX as u64) as i64);
        let current = self.smallest[0].map_or(difference, |known| known.min(difference));
        self.smallest[0] = Some(current);
        let smallest = self.smallest[1].map_or(current, |before| before.min(current));
        Some(difference.saturating_sub(smallest).max(0) as u64)
    }

    /// A batch arrived whose newest event was held up for `held_up`
    /// microseconds (`None`: not known).
    pub fn batch(&mut self, held_up: Option<u64>) {
        self.batches = self.batches.saturating_add(1);
        if let Some(held_up) = held_up {
            self.worst_micros = self.worst_micros.max(held_up);
            if held_up >= Self::LATE_MICROS {
                self.late = self.late.saturating_add(1);
            }
        }
    }

    /// Whether a key repeat held up for `held_up` is dropped, and counts it.
    pub fn stale_repeat(&mut self, held_up: Option<u64>) -> bool {
        let stale = held_up.is_some_and(|held_up| held_up >= Self::STALE_REPEAT_MICROS);
        if stale {
            self.stale_repeats = self.stale_repeats.saturating_add(1);
        }
        stale
    }

    /// What arrived since the last call; `None` if nothing did.
    pub fn take(&mut self) -> Option<InputTimingReport> {
        if self.batches == 0 {
            return None;
        }
        let report = InputTimingReport {
            batches: self.batches,
            late: self.late,
            worst_micros: self.worst_micros,
            stale_repeats: self.stale_repeats,
        };
        self.batches = 0;
        self.late = 0;
        self.worst_micros = 0;
        self.stale_repeats = 0;
        Some(report)
    }
}

#[derive(Default)]
pub struct RemoteInputState {
    last_sequence: u64,
    pressed_keys: HashMap<u32, LcInputEvent>,
    pressed_buttons: u32,
    /// The peer is driving this device: input arrived since the session
    /// started or since the last reset.
    controlled: bool,
}

impl RemoteInputState {
    /// Marks that remote input is arriving. True when this starts a new
    /// period of remote control.
    pub fn begin_control(&mut self) -> bool {
        !std::mem::replace(&mut self.controlled, true)
    }

    /// Marks that the peer stopped driving this device. True if it was.
    pub fn end_control(&mut self) -> bool {
        std::mem::replace(&mut self.controlled, false)
    }

    /// The peer is driving this device right now.
    pub fn is_controlling(&self) -> bool {
        self.controlled
    }

    pub fn accept_range(&mut self, first_sequence: u64, count: usize) -> SequenceStatus {
        let Some(last_sequence) = first_sequence.checked_add(count.saturating_sub(1) as u64) else {
            return SequenceStatus::Gap {
                expected: self.last_sequence.saturating_add(1),
                received: first_sequence,
            };
        };
        let expected = self.last_sequence.saturating_add(1);
        if last_sequence <= self.last_sequence {
            return SequenceStatus::Duplicate;
        }
        let status = if first_sequence == expected {
            SequenceStatus::InOrder
        } else {
            SequenceStatus::Gap {
                expected,
                received: first_sequence,
            }
        };
        self.last_sequence = last_sequence;
        status
    }

    pub fn accept_sequence(&mut self, sequence: u64) -> SequenceStatus {
        self.accept_range(sequence, 1)
    }

    pub fn observe(&mut self, event: LcInputEvent) {
        match event.kind {
            LC_INPUT_KEY_DOWN => {
                self.pressed_keys.insert(event.code, event);
            }
            LC_INPUT_KEY_UP => {
                self.pressed_keys.remove(&event.code);
            }
            LC_INPUT_MOUSE_BUTTON_DOWN | LC_INPUT_MOUSE_BUTTON_UP => {
                // The sender includes the entire post-event button state. This
                // remains correct for chords and non-primary buttons.
                self.pressed_buttons = event.buttons;
            }
            _ => {}
        }
    }

    pub fn release_events(&mut self) -> Vec<LcInputEvent> {
        let mut events = Vec::with_capacity(
            self.pressed_keys.len() + self.pressed_buttons.count_ones() as usize,
        );
        let mut keys: Vec<_> = self.pressed_keys.drain().map(|(_, event)| event).collect();
        keys.sort_unstable_by_key(|event| event.code);
        for mut event in keys {
            event.kind = LC_INPUT_KEY_UP;
            event.flags &= !LC_INPUT_FLAG_KEY_REPEAT;
            event.repeat_count = 0;
            event.key_symbol = 0;
            event.modifiers = 0;
            events.push(event);
        }

        let buttons = self.pressed_buttons;
        self.pressed_buttons = 0;
        let mut remaining = buttons;
        for index in 0..32 {
            let mask = 1_u32 << index;
            if buttons & mask == 0 {
                continue;
            }
            remaining &= !mask;
            events.push(LcInputEvent {
                kind: LC_INPUT_MOUSE_BUTTON_UP,
                code: index + 1,
                buttons: remaining,
                ..LcInputEvent::default()
            });
        }
        events
    }
}

/// How long input takes to a driven peer and back, from its `InputEcho`
/// messages, for the `input_latency` event.
#[derive(Default)]
pub struct RoundTrips {
    samples: u32,
    total_micros: u64,
    worst_micros: u64,
}

impl RoundTrips {
    /// An echo of the input stamped `stamp` (this device's clock) came back
    /// at `now` (the same clock).
    pub fn echoed(&mut self, stamp: u64, now: u64) {
        // A stamp from the future or from long ago is not this core's.
        let Some(micros) = now.checked_sub(stamp).filter(|micros| *micros < 10_000_000) else {
            return;
        };
        self.samples += 1;
        self.total_micros += micros;
        self.worst_micros = self.worst_micros.max(micros);
    }

    /// Since the last call: how many echoes, their mean and their worst, in
    /// microseconds. `None` if none came.
    pub fn take(&mut self) -> Option<(u32, u64, u64)> {
        let taken = std::mem::take(self);
        (taken.samples > 0).then(|| {
            (
                taken.samples,
                taken.total_micros / u64::from(taken.samples),
                taken.worst_micros,
            )
        })
    }
}

/// Whether motion to a peer is paced (sent at most every `MOTION_INTERVAL`,
/// merged in between; see `transport.rs`) or sent as it comes.
///
/// On a link that keeps up, every report of the mouse goes out at once:
/// nothing is gained by holding it. Pacing is for a link in trouble, where
/// a thousand small packets a second make the trouble worse. So it starts
/// when the link shows trouble and lasts a while after the last sign of it.
/// If trouble comes back soon after pacing stopped, the unpaced motion was
/// probably its cause, and pacing then lasts twice as long, up to
/// [`Self::LONGEST`]: a link that only works paced ends up paced.
pub struct MotionPacing {
    /// Paced until then.
    until: Option<std::time::Instant>,
    /// How long pacing lasts after the last trouble.
    hold: std::time::Duration,
    /// When pacing last stopped.
    stopped: Option<std::time::Instant>,
}

impl Default for MotionPacing {
    fn default() -> Self {
        Self {
            until: None,
            hold: Self::SHORTEST,
            stopped: None,
        }
    }
}

impl MotionPacing {
    pub const SHORTEST: std::time::Duration = std::time::Duration::from_secs(30);
    pub const LONGEST: std::time::Duration = std::time::Duration::from_secs(600);

    /// Looks at the link once (every second): `trouble` if packets were
    /// lost, input arrived late or the round trip grew. Returns whether
    /// motion is paced from now.
    pub fn observe(&mut self, trouble: bool, now: std::time::Instant) -> bool {
        let paced = self.until.is_some_and(|until| now < until);
        if !paced && self.until.take().is_some() {
            self.stopped = Some(now);
        }
        if trouble {
            if !paced {
                let soon = self
                    .stopped
                    .is_some_and(|stopped| now.saturating_duration_since(stopped) < self.hold * 2);
                self.hold = if soon {
                    (self.hold * 2).min(Self::LONGEST)
                } else {
                    Self::SHORTEST
                };
            }
            self.until = Some(now + self.hold);
        }
        self.until.is_some()
    }
}
