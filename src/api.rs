use crate::model::{CoreConfig, LcInputEvent};
use crate::runtime::{Command, CoreThread, EventSink};

/// Safe Rust entry point for platform shells that link the crate directly
/// instead of going through the C ABI in `ffi.rs`.
///
/// Events are delivered as the same JSON strings the C ABI emits. Both
/// callbacks run on the core's internal threads. Dropping the value stops
/// the core and waits for its threads to exit.
pub struct Core {
    thread: CoreThread,
}

impl Core {
    pub fn start(
        config: CoreConfig,
        on_event: impl Fn(String) + Send + Sync + 'static,
        on_input: impl Fn(LcInputEvent) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let thread = CoreThread::spawn(config, EventSink::new(on_event, on_input))?;
        Ok(Self { thread })
    }

    pub fn connect(&self, peer_id: &str) -> Result<(), String> {
        self.thread.send(Command::Connect(peer_id.to_owned()))
    }

    pub fn disconnect(&self) -> Result<(), String> {
        self.thread.send(Command::Disconnect)
    }

    pub fn send_input(&self, event: LcInputEvent) -> Result<(), String> {
        self.thread.send(Command::SendInput(event))
    }

    pub fn reset_input(&self) -> Result<(), String> {
        self.thread.send(Command::ResetInput)
    }
}
