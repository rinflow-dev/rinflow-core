mod api;
mod ffi;
mod input;
mod model;
mod protocol;
mod runtime;
mod transport;

#[cfg(target_os = "android")]
mod android_jni;

pub use api::Core;
pub use ffi::{LcCore, LcEventCallback, LcInputCallback};
pub use model::{CoreConfig, LcInputEvent};

#[cfg(test)]
mod tests {
    use super::input::{OutboundInputQueue, PushOutcome, RemoteInputState};
    use super::model::{
        Announcement, CoreConfig, LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_INPUT_MOUSE_BUTTON_DOWN,
        LC_INPUT_MOUSE_BUTTON_UP, LC_INPUT_MOUSE_MOVE, LcInputEvent, PROTOCOL_MAX_VERSION,
        WireMessage,
    };
    use super::protocol::{decode_message, encode_message};
    use super::runtime::{Command, CoreThread, EventSink};
    use serde_json::Value;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    #[test]
    fn announcement_negotiates_only_overlapping_protocol_versions() {
        let mut announcement = Announcement::from_config(&CoreConfig {
            device_id: "one".to_owned(),
            ..CoreConfig::default()
        });
        assert!(announcement.is_compatible());
        assert_eq!(
            announcement.negotiated_version(),
            Some(PROTOCOL_MAX_VERSION)
        );
        assert_eq!(announcement.platform, "unknown");
        announcement.min_protocol_version = PROTOCOL_MAX_VERSION + 1;
        announcement.max_protocol_version = PROTOCOL_MAX_VERSION + 1;
        assert!(!announcement.is_compatible());
    }

    #[test]
    fn binary_input_batch_round_trips_and_assigns_sequences() {
        let message = WireMessage::InputBatch {
            first_sequence: 41,
            events: vec![
                LcInputEvent {
                    kind: LC_INPUT_KEY_DOWN,
                    code: 0x04,
                    key_symbol: u32::from('a'),
                    modifiers: 1,
                    ..LcInputEvent::default()
                },
                LcInputEvent {
                    kind: LC_INPUT_KEY_UP,
                    code: 0x04,
                    ..LcInputEvent::default()
                },
            ],
        };
        let bytes = encode_message(&message).expect("encode message");
        assert_ne!(bytes.first(), Some(&b'{'));
        let actual = decode_message(&bytes).expect("decode message");
        let WireMessage::InputBatch {
            first_sequence,
            events,
        } = actual
        else {
            panic!("expected input batch")
        };
        assert_eq!(first_sequence, 41);
        assert_eq!(events[0].sequence, 41);
        assert_eq!(events[1].sequence, 42);
        assert_eq!(events[0].key_symbol, u32::from('a'));
    }

    #[test]
    fn binary_protocol_rejects_trailing_data() {
        let mut bytes = encode_message(&WireMessage::Ping { sequence: 7 }).expect("encode ping");
        bytes.push(0);
        assert!(decode_message(&bytes).is_err());
    }

    #[test]
    fn binary_protocol_rejects_every_truncated_prefix() {
        let bytes = encode_message(&WireMessage::InputBatch {
            first_sequence: 1,
            events: vec![LcInputEvent {
                kind: LC_INPUT_MOUSE_MOVE,
                delta_x: 1.0,
                ..LcInputEvent::default()
            }],
        })
        .expect("encode input batch");
        for length in 0..bytes.len() {
            assert!(
                decode_message(&bytes[..length]).is_err(),
                "prefix of length {length} unexpectedly decoded"
            );
        }
    }

    #[test]
    fn input_ffi_layout_has_expected_size() {
        assert_eq!(std::mem::size_of::<LcInputEvent>(), 96);
        assert_eq!(std::mem::align_of::<LcInputEvent>(), 8);
    }

    #[test]
    fn pointer_motion_is_coalesced_without_reordering_critical_input() {
        let queue = OutboundInputQueue::new();
        assert_eq!(
            queue
                .push(LcInputEvent {
                    kind: LC_INPUT_MOUSE_MOVE,
                    x: 10.0,
                    delta_x: 2.0,
                    ..LcInputEvent::default()
                })
                .expect("first motion"),
            PushOutcome::Queued
        );
        assert_eq!(
            queue
                .push(LcInputEvent {
                    kind: LC_INPUT_MOUSE_MOVE,
                    x: 14.0,
                    delta_x: 4.0,
                    ..LcInputEvent::default()
                })
                .expect("second motion"),
            PushOutcome::Coalesced
        );
        queue
            .push(LcInputEvent {
                kind: LC_INPUT_KEY_DOWN,
                code: 0x04,
                ..LcInputEvent::default()
            })
            .expect("key down");
        assert_eq!(queue.len(), 2);

        let Some(WireMessage::InputBatch { events, .. }) = queue.pop_message() else {
            panic!("expected input batch")
        };
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, LC_INPUT_MOUSE_MOVE);
        assert_eq!(events[0].x, 14.0);
        assert_eq!(events[0].delta_x, 6.0);
        assert_eq!(events[1].kind, LC_INPUT_KEY_DOWN);
        assert_eq!(events[0].sequence + 1, events[1].sequence);
    }

    #[test]
    fn reset_discards_stale_queued_input() {
        let queue = OutboundInputQueue::new();
        queue
            .push(LcInputEvent {
                kind: LC_INPUT_KEY_DOWN,
                code: 0x04,
                ..LcInputEvent::default()
            })
            .expect("key down");
        queue.reset();
        assert!(matches!(
            queue.pop_message(),
            Some(WireMessage::InputReset { sequence: 1 })
        ));
        assert!(queue.pop_message().is_none());
    }

    #[test]
    fn remote_state_releases_pressed_keys_and_buttons() {
        let mut state = RemoteInputState::default();
        state.observe(LcInputEvent {
            kind: LC_INPUT_KEY_DOWN,
            code: 0x04,
            ..LcInputEvent::default()
        });
        state.observe(LcInputEvent {
            kind: LC_INPUT_MOUSE_BUTTON_DOWN,
            code: 1,
            buttons: 1,
            ..LcInputEvent::default()
        });
        let releases = state.release_events();
        assert_eq!(releases.len(), 2);
        assert!(
            releases
                .iter()
                .any(|event| { event.kind == LC_INPUT_KEY_UP && event.code == 0x04 })
        );
        assert!(releases.iter().any(|event| {
            event.kind == LC_INPUT_MOUSE_BUTTON_UP && event.code == 1 && event.buttons == 0
        }));
    }

    #[test]
    fn config_requires_an_identity() {
        assert!(CoreConfig::default().validate().is_err());
    }

    #[test]
    fn two_cores_discover_connect_and_exchange_input() {
        let discovery_port = free_udp_port();
        let first_port = free_tcp_port();
        let second_port = free_tcp_port();

        let (first_events_tx, first_events_rx) = mpsc::channel();
        let (second_events_tx, second_events_rx) = mpsc::channel();
        let (second_input_tx, second_input_rx) = mpsc::channel();

        let first_sink = EventSink::new(
            move |event| {
                let _ = first_events_tx.send(event);
            },
            |_| {},
        );
        let second_sink = EventSink::new(
            move |event| {
                let _ = second_events_tx.send(event);
            },
            move |event| {
                let _ = second_input_tx.send(event);
            },
        );

        let mut first = CoreThread::spawn(
            CoreConfig {
                device_id: "first-test-device".to_owned(),
                device_name: "First".to_owned(),
                control_port: first_port,
                discovery_port,
                ..CoreConfig::default()
            },
            first_sink,
        )
        .expect("start first core");
        let mut second = CoreThread::spawn(
            CoreConfig {
                device_id: "second-test-device".to_owned(),
                device_name: "Second".to_owned(),
                control_port: second_port,
                discovery_port,
                ..CoreConfig::default()
            },
            second_sink,
        )
        .expect("start second core");

        wait_for_event(&first_events_rx, Duration::from_secs(8), |event| {
            event["event"] == "peer_up" && event["peer"]["id"] == "second-test-device"
        });
        first
            .send(Command::Connect("second-test-device".to_owned()))
            .expect("request connection");

        wait_for_event(&first_events_rx, Duration::from_secs(6), |event| {
            event["event"] == "connected"
        });
        wait_for_event(&second_events_rx, Duration::from_secs(6), |event| {
            event["event"] == "connected"
        });

        let expected = LcInputEvent {
            kind: LC_INPUT_MOUSE_MOVE,
            delta_x: 12.5,
            delta_y: -3.0,
            ..LcInputEvent::default()
        };
        first
            .send(Command::SendInput(expected))
            .expect("send input event");
        let received = second_input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive input event");
        assert_eq!(received.kind, expected.kind);
        assert_eq!(received.delta_x, expected.delta_x);
        assert_eq!(received.delta_y, expected.delta_y);
        assert_eq!(received.sequence, 1);

        first
            .send(Command::SendInput(LcInputEvent {
                kind: LC_INPUT_KEY_DOWN,
                code: 0x04,
                ..LcInputEvent::default()
            }))
            .expect("send key down");
        let key_down = second_input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive key down");
        assert_eq!(key_down.kind, LC_INPUT_KEY_DOWN);
        first.send(Command::ResetInput).expect("reset remote input");
        let key_up = second_input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive synthetic key up");
        assert_eq!(key_up.kind, LC_INPUT_KEY_UP);
        assert_eq!(key_up.code, 0x04);

        first.stop_and_join();
        second.stop_and_join();
    }

    fn wait_for_event(
        receiver: &Receiver<String>,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        let mut observed = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = receiver.recv_timeout(remaining).unwrap_or_else(|_| {
                panic!("expected core event before timeout; observed: {observed:?}")
            });
            let event: Value = serde_json::from_str(&message).expect("valid event JSON");
            if predicate(&event) {
                return;
            }
            observed.push(event);
        }
    }

    fn free_tcp_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .expect("bind temporary TCP port")
            .local_addr()
            .expect("temporary TCP address")
            .port()
    }

    fn free_udp_port() -> u16 {
        UdpSocket::bind(("127.0.0.1", 0))
            .expect("bind temporary UDP port")
            .local_addr()
            .expect("temporary UDP address")
            .port()
    }
}
