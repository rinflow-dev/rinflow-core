use crate::model::{
    LC_INPUT_FLAG_KEY_REPEAT, LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_INPUT_MOUSE_BUTTON_DOWN,
    LC_INPUT_MOUSE_BUTTON_UP, LcInputEvent, MAX_INPUT_BATCH_SIZE, WireMessage,
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

fn take_sequence(state: &mut OutboundState) -> u64 {
    let sequence = state.next_sequence;
    state.next_sequence = state.next_sequence.checked_add(1).unwrap_or(1);
    sequence
}

fn coalesce(previous: &mut LcInputEvent, next: &LcInputEvent) -> bool {
    if previous.is_motion() && next.is_motion() {
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
