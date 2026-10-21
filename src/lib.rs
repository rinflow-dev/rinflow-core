mod api;
mod clipboard;
mod ffi;
mod files;
mod input;
pub mod layout;
mod model;
mod protocol;
mod qos;
mod quic;
mod remap;
mod runtime;
mod secure;
mod transport;

#[cfg(target_os = "android")]
mod android_jni;

pub use api::Core;
pub use clipboard::{
    CLIPBOARD_CHUNK_SIZE, Clipboard, ClipboardFormat, MAX_CLIPBOARD_SIZE, MIME_HTML, MIME_PNG,
    MIME_TEXT,
};
pub use ffi::{LcCore, LcEventCallback, LcInputCallback};
pub use files::MIME_URI_LIST;
pub use model::{
    CoreConfig, LC_INPUT_FLAG_ABSOLUTE_POSITION, LC_INPUT_FLAG_KEY_REPEAT,
    LC_INPUT_FLAG_PRECISE_SCROLL, LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_INPUT_MOUSE_BUTTON_DOWN,
    LC_INPUT_MOUSE_BUTTON_UP, LC_INPUT_MOUSE_MOVE, LC_INPUT_SCROLL, LC_MODIFIER_CAPS_LOCK,
    LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL, LC_MODIFIER_LEFT_COMMAND, LC_MODIFIER_LEFT_CONTROL,
    LC_MODIFIER_LEFT_OPTION, LC_MODIFIER_LEFT_SHIFT, LC_MODIFIER_OPTION, LC_MODIFIER_RIGHT_COMMAND,
    LC_MODIFIER_RIGHT_CONTROL, LC_MODIFIER_RIGHT_OPTION, LC_MODIFIER_RIGHT_SHIFT,
    LC_MODIFIER_SHIFT, LcInputEvent, PlacementRecord, TrustedPeer,
};

#[cfg(test)]
mod tests {
    use super::clipboard::{
        self, CLIPBOARD_CHUNK_SIZE, Clipboard, ClipboardAssembler, ClipboardFormat,
        MAX_CLIPBOARD_SIZE, MIME_HTML, MIME_PNG, MIME_TEXT,
    };
    use super::files::{self as transfer_files, FileWriter, MIME_URI_LIST, Manifest};
    use super::input::{
        OutboundInputQueue, PlatformEntry, PlatformInputQueue, PushOutcome, RemoteInputState,
    };
    use super::layout::{Display, Offset, Side};
    use super::model::{
        Announcement, CAPABILITY_CONTROL_REVOKE, CoreConfig, LC_INPUT_FLAG_ABSOLUTE_POSITION,
        LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_INPUT_MOUSE_BUTTON_DOWN, LC_INPUT_MOUSE_BUTTON_UP,
        LC_INPUT_MOUSE_MOVE, LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL, LC_MODIFIER_LEFT_COMMAND,
        LC_MODIFIER_LEFT_CONTROL, LC_MODIFIER_SHIFT, LOCAL_CAPABILITIES, LcInputEvent,
        PROTOCOL_MAX_VERSION, WireMessage,
    };
    use super::model::{PlacementRecord, TrustedPeer};
    use super::protocol::{decode_message, encode_message};
    use super::remap::ShortcutRemap;
    use super::runtime::trust as runtime_reason;
    use super::runtime::{Command, CoreThread, EventSink};
    use serde_json::Value;
    use std::net::UdpSocket;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    fn text_clipboard(text: &str) -> Clipboard {
        Clipboard {
            formats: vec![ClipboardFormat {
                mime: MIME_TEXT.to_owned(),
                data: text.as_bytes().to_vec(),
            }],
        }
    }

    #[test]
    fn clipboard_round_trips_through_bytes_json_and_chunks() {
        let clipboard = Clipboard {
            formats: vec![
                ClipboardFormat {
                    mime: MIME_HTML.to_owned(),
                    data: b"<b>hi</b>".to_vec(),
                },
                ClipboardFormat {
                    mime: MIME_TEXT.to_owned(),
                    data: "hi 你好".as_bytes().to_vec(),
                },
                ClipboardFormat {
                    mime: MIME_PNG.to_owned(),
                    // Several chunks' worth, to cross chunk boundaries.
                    data: (0..CLIPBOARD_CHUNK_SIZE * 2 + 17)
                        .map(|i| i as u8)
                        .collect(),
                },
            ],
        };
        clipboard.validate().expect("valid clipboard");
        let encoded = clipboard.encode();
        assert_eq!(Clipboard::decode(&encoded).unwrap(), clipboard);

        let json = serde_json::json!({ "formats": clipboard.to_json() });
        assert_eq!(Clipboard::from_json(&json).unwrap(), clipboard);

        let frames = clipboard::chunks(9, &encoded);
        assert_eq!(frames.len(), 3);
        let mut assembler = ClipboardAssembler::default();
        let mut complete = None;
        for frame in frames {
            // Every frame survives the wire format.
            let frame = decode_message(&encode_message(&frame).unwrap()).unwrap();
            let WireMessage::ClipboardChunk {
                id,
                total,
                offset,
                data,
            } = frame
            else {
                panic!("not a clipboard chunk: {frame:?}");
            };
            assert!(complete.is_none());
            complete = assembler.push(id, total, offset, &data).unwrap();
        }
        assert_eq!(complete.unwrap(), encoded);
    }

    #[test]
    fn clipboard_limits_and_broken_transfers_are_refused() {
        let huge = Clipboard {
            formats: vec![ClipboardFormat {
                mime: MIME_PNG.to_owned(),
                data: vec![0; MAX_CLIPBOARD_SIZE],
            }],
        };
        assert!(huge.validate().is_err());
        assert!(
            encode_message(&WireMessage::ClipboardChunk {
                id: 1,
                total: 10,
                offset: 8,
                data: vec![0; 4],
            })
            .is_err()
        );

        let encoded = text_clipboard("abc").encode();
        let mut assembler = ClipboardAssembler::default();
        // A transfer must start at its beginning.
        assert!(
            assembler
                .push(1, encoded.len() as u32, 2, &encoded[2..])
                .is_err()
        );
        // A newer transfer replaces one in progress.
        assert_eq!(assembler.push(2, 100, 0, &[0; 10]).unwrap(), None);
        assert_eq!(
            assembler
                .push(3, encoded.len() as u32, 0, &encoded)
                .unwrap()
                .unwrap(),
            encoded
        );
    }

    #[test]
    fn manifest_paths_are_cleaned_or_refused() {
        let clean = |path: &str| {
            transfer_files::sanitize(path).map(|p| p.to_string_lossy().replace('\\', "/"))
        };
        assert_eq!(clean("报告/第一章.pdf").as_deref(), Some("报告/第一章.pdf"));
        assert_eq!(clean("a<b>:c").as_deref(), None); // ':' never: drive letters, streams
        assert_eq!(clean("a<b>|c?.txt").as_deref(), Some("a_b__c_.txt"));
        assert_eq!(clean("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(clean("com1").as_deref(), Some("_com1"));
        assert_eq!(clean("trailing. . ").as_deref(), Some("trailing"));
        assert_eq!(clean("back\\slash").as_deref(), None); // backslash refused
        assert_eq!(clean("tab\there").as_deref(), Some("tabhere")); // control characters go
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "C:/x",
            "...",
            "a/ /b",
        ] {
            assert_eq!(clean(bad), None, "{bad:?} must be refused");
        }
    }

    #[test]
    fn uri_lists_round_trip_and_ignore_foreign_urls() {
        let paths = vec![
            PathBuf::from("/tmp/a b/报告#1.pdf"),
            PathBuf::from("/tmp/c%d"),
        ];
        let list = transfer_files::uri_list(&paths);
        if cfg!(unix) {
            assert_eq!(transfer_files::parse_uri_list(&list), paths);
        }
        if cfg!(windows) {
            let windows = vec![
                PathBuf::from(r"C:\Users\a b\报告#1.pdf"),
                PathBuf::from(r"\\server\share\x"),
            ];
            let list = transfer_files::uri_list(&windows);
            assert_eq!(transfer_files::parse_uri_list(&list), windows);
        }
        let mixed =
            b"# comment\r\nhttps://example.com/x\r\nfile://server/share/x\r\nfile:///tmp/ok\r\n";
        let parsed = transfer_files::parse_uri_list(mixed);
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn manifests_must_add_up() {
        let manifest = |total| Manifest {
            transfer: 1,
            total_size: total,
            entries: vec![
                transfer_files::Entry {
                    path: "d".into(),
                    size: 0,
                    dir: true,
                },
                transfer_files::Entry {
                    path: "d/f".into(),
                    size: 5,
                    dir: false,
                },
            ],
        };
        assert!(manifest(5).validate().is_ok());
        assert!(manifest(6).validate().is_err());
    }

    #[test]
    fn unfinished_files_leave_nothing_behind() {
        let dir = temp_dir("partial");
        let target = dir.join("x.bin");
        {
            let mut writer = FileWriter::create(target.clone(), 10, 1).unwrap();
            assert!(!writer.write(0, b"12345").unwrap());
            assert!(writer.write(3, b"zz").is_err(), "out of order");
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let mut writer = FileWriter::create(target.clone(), 3, 2).unwrap();
        assert!(writer.write(0, b"abc").unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        // The same name again gets a number.
        assert_eq!(
            transfer_files::unique_child(&dir, "x.bin"),
            dir.join("x (1).bin")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn data_link_and_file_messages_round_trip() {
        for message in [
            WireMessage::DataInvite {
                token: 42,
                port: 50_123,
            },
            WireMessage::PairRequest,
            WireMessage::PairAccept,
            WireMessage::DataHello {
                device_id: "dev".into(),
                token: 7,
            },
            WireMessage::FileRequest {
                transfer: 3,
                entry: 9,
                offset: 1 << 40,
            },
            WireMessage::FileChunk {
                transfer: 3,
                entry: 9,
                offset: 5,
                data: vec![1, 2, 3],
            },
            WireMessage::FileCancel {
                transfer: 3,
                reason: "文件已变化".into(),
            },
        ] {
            let bytes = encode_message(&message).unwrap();
            assert_eq!(decode_message(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn control_revoke_round_trips_and_rejects_other_flags() {
        for handoff in [false, true] {
            let message = WireMessage::ControlRevoke { handoff };
            let bytes = encode_message(&message).unwrap();
            assert_eq!(decode_message(&bytes).unwrap(), message);
        }
        let mut bytes = encode_message(&WireMessage::ControlRevoke { handoff: true }).unwrap();
        *bytes.last_mut().unwrap() = 2;
        assert!(decode_message(&bytes).is_err());
    }

    #[test]
    fn announcement_negotiates_only_overlapping_protocol_versions() {
        let config = CoreConfig {
            device_id: "one".to_owned(),
            ..CoreConfig::default()
        };
        let fingerprint = "ab".repeat(32);
        let mut announcement =
            Announcement::from_config(&config, &[], config.advertised_capabilities(), &fingerprint);
        assert!(announcement.is_compatible());
        assert_eq!(
            announcement.negotiated_version(),
            Some(PROTOCOL_MAX_VERSION)
        );
        assert_eq!(announcement.platform, "unknown");
        assert_eq!(
            announcement.fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );
        announcement.min_protocol_version = PROTOCOL_MAX_VERSION + 1;
        announcement.max_protocol_version = PROTOCOL_MAX_VERSION + 1;
        assert!(!announcement.is_compatible());
        // Version 3 ran over plain TCP; this core speaks only QUIC.
        announcement.min_protocol_version = 3;
        announcement.max_protocol_version = 3;
        assert!(!announcement.is_compatible());
    }

    #[test]
    fn data_invites_need_a_port() {
        let bytes = encode_message(&WireMessage::DataInvite {
            token: 1,
            port: 42_500,
        })
        .unwrap();
        // Without the port (as protocol 3 sent it), or with port 0.
        assert!(decode_message(&bytes[..bytes.len() - 2]).is_err());
        let mut zero = bytes.clone();
        let length = zero.len();
        zero[length - 2..].fill(0);
        assert!(decode_message(&zero).is_err());
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
    fn absolute_moves_are_not_merged_with_relative_motion() {
        let queue = OutboundInputQueue::new();
        let relative = LcInputEvent {
            kind: LC_INPUT_MOUSE_MOVE,
            delta_x: 3.0,
            ..LcInputEvent::default()
        };
        queue.push(relative).expect("motion before");
        let arrival = LcInputEvent {
            kind: LC_INPUT_MOUSE_MOVE,
            flags: LC_INPUT_FLAG_ABSOLUTE_POSITION,
            x: 1440.0,
            y: 300.0,
            ..LcInputEvent::default()
        };
        assert_eq!(queue.push(arrival).expect("arrival"), PushOutcome::Queued);
        assert_eq!(
            queue.push(relative).expect("motion after"),
            PushOutcome::Queued
        );

        let Some(WireMessage::InputBatch { events, .. }) = queue.pop_message() else {
            panic!("expected input batch")
        };
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].flags, LC_INPUT_FLAG_ABSOLUTE_POSITION);
        assert_eq!((events[1].x, events[1].y), (1440.0, 300.0));
        assert_eq!(events[2].delta_x, 3.0);
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
    fn keys_keep_their_position_by_default() {
        // Command stays the GUI key (the Windows key on a PC), as in Deskflow.
        assert!(!CoreConfig::default().shortcut_remap);
        let mut command = LcInputEvent {
            kind: LC_INPUT_KEY_DOWN,
            code: 0xE3,
            modifiers: LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND,
            ..LcInputEvent::default()
        };
        let sent = command;
        ShortcutRemap::None.apply(&mut command);
        assert_eq!(command, sent);
    }

    #[test]
    fn shortcut_remap_depends_on_both_platforms() {
        assert_eq!(
            ShortcutRemap::between("macos", "windows"),
            ShortcutRemap::AppleToPc
        );
        assert_eq!(
            ShortcutRemap::between("linux", "macos"),
            ShortcutRemap::PcToApple
        );
        assert_eq!(
            ShortcutRemap::between("macos", "macos"),
            ShortcutRemap::None
        );
        assert_eq!(
            ShortcutRemap::between("windows", "linux"),
            ShortcutRemap::None
        );
        assert_eq!(
            ShortcutRemap::between("unknown", "windows"),
            ShortcutRemap::None
        );
    }

    #[test]
    fn apple_to_pc_turns_command_into_control_and_keeps_control() {
        let key = |code, modifiers| LcInputEvent {
            kind: LC_INPUT_KEY_DOWN,
            code,
            modifiers,
            ..LcInputEvent::default()
        };
        let mut command_c = key(0x06, LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND);
        ShortcutRemap::AppleToPc.apply(&mut command_c);
        assert_eq!(command_c.code, 0x06);
        assert_eq!(
            command_c.modifiers,
            LC_MODIFIER_CONTROL | LC_MODIFIER_LEFT_CONTROL
        );

        let mut right_command = key(0xE7, 0);
        ShortcutRemap::AppleToPc.apply(&mut right_command);
        assert_eq!(right_command.code, 0xE4);

        let mut control = key(0xE0, LC_MODIFIER_CONTROL | LC_MODIFIER_SHIFT);
        ShortcutRemap::AppleToPc.apply(&mut control);
        assert_eq!(control.code, 0xE0);
        assert_eq!(control.modifiers, LC_MODIFIER_CONTROL | LC_MODIFIER_SHIFT);
    }

    #[test]
    fn pc_to_apple_swaps_control_and_super() {
        let mut control = LcInputEvent {
            kind: LC_INPUT_KEY_UP,
            code: 0xE0,
            modifiers: LC_MODIFIER_CONTROL | LC_MODIFIER_LEFT_CONTROL,
            ..LcInputEvent::default()
        };
        ShortcutRemap::PcToApple.apply(&mut control);
        assert_eq!(control.code, 0xE3);
        assert_eq!(
            control.modifiers,
            LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND
        );

        let mut super_key = LcInputEvent {
            kind: LC_INPUT_KEY_DOWN,
            code: 0xE3,
            modifiers: LC_MODIFIER_COMMAND,
            ..LcInputEvent::default()
        };
        ShortcutRemap::PcToApple.apply(&mut super_key);
        assert_eq!(super_key.code, 0xE0);
        assert_eq!(super_key.modifiers, LC_MODIFIER_CONTROL);
    }

    #[test]
    fn target_changes_stay_ordered_with_input() {
        let queue = PlatformInputQueue::new();
        let first = LcInputEvent {
            kind: LC_INPUT_MOUSE_MOVE,
            delta_x: 1.0,
            ..LcInputEvent::default()
        };
        let second = LcInputEvent {
            delta_x: 2.0,
            ..first
        };
        queue.push(first).expect("first motion");
        queue.push_control(PlatformEntry::SetTarget(Some("other".to_owned())));
        // Motion after the switch must not merge into motion for the old target.
        queue.push(second).expect("second motion");
        assert_eq!(
            queue.drain(),
            vec![
                PlatformEntry::Event(first),
                PlatformEntry::SetTarget(Some("other".to_owned())),
                PlatformEntry::Event(second),
            ]
        );
    }

    #[test]
    fn remote_control_starts_once_and_ends_once() {
        let mut state = RemoteInputState::default();
        assert!(state.begin_control());
        assert!(!state.begin_control());
        assert!(state.end_control());
        assert!(!state.end_control());
    }

    #[test]
    fn config_requires_an_identity() {
        assert!(CoreConfig::default().validate().is_err());
    }

    #[test]
    fn two_cores_discover_connect_and_exchange_input() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let first_port = free_udp_port();
        let second_port = free_udp_port();

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
                platform: "macos".to_owned(),
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
                platform: "windows".to_owned(),
                control_port: second_port,
                discovery_port,
                // Opt in, to cover the translation end to end; by default
                // keys keep their position.
                shortcut_remap: true,
                ..CoreConfig::default()
            },
            second_sink,
        )
        .expect("start second core");

        wait_for_event(&first_events_rx, Duration::from_secs(8), |event| {
            event["event"] == "peer_up" && event["peer"]["id"] == "second-test-device"
        });
        // The receiver picks the shortcut translation from the platform it
        // discovered, so it has to know the sender before the session starts.
        wait_for_event(&second_events_rx, Duration::from_secs(8), |event| {
            event["event"] == "peer_up" && event["peer"]["id"] == "first-test-device"
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
        wait_for_event(&second_events_rx, Duration::from_secs(3), |event| {
            event["event"] == "remote_control_started"
        });

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

        // With shortcut_remap on, Mac Command arrives on Windows as Control.
        first
            .send(Command::SendInput(LcInputEvent {
                kind: LC_INPUT_KEY_DOWN,
                code: 0xE3,
                modifiers: LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND,
                ..LcInputEvent::default()
            }))
            .expect("send command down");
        let command_down = second_input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive command down");
        assert_eq!(command_down.code, 0xE0);
        assert_eq!(
            command_down.modifiers,
            LC_MODIFIER_CONTROL | LC_MODIFIER_LEFT_CONTROL
        );

        first.send(Command::ResetInput).expect("reset remote input");
        let key_up = second_input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive synthetic key up");
        assert_eq!(key_up.kind, LC_INPUT_KEY_UP);
        assert_eq!(key_up.code, 0x04);
        let control_up = second_input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive synthetic control up");
        assert_eq!((control_up.kind, control_up.code), (LC_INPUT_KEY_UP, 0xE0));
        wait_for_event(&second_events_rx, Duration::from_secs(3), |event| {
            event["event"] == "remote_control_ended" && event["reason"] == "remote reset"
        });

        first.stop_and_join();
        second.stop_and_join();
    }

    /// A lone event must go out at once, not wait for more input or the
    /// one-second keepalive. Without this a key release could lag behind.
    #[test]
    fn single_input_event_is_delivered_promptly() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let (first_events_tx, first_events_rx) = mpsc::channel();
        let (second_events_tx, second_events_rx) = mpsc::channel();
        let (second_input_tx, second_input_rx) = mpsc::channel();
        let mut first = CoreThread::spawn(
            CoreConfig {
                device_id: "latency-first".to_owned(),
                control_port: free_udp_port(),
                discovery_port,
                ..CoreConfig::default()
            },
            EventSink::new(
                move |event| {
                    let _ = first_events_tx.send(event);
                },
                |_| {},
            ),
        )
        .expect("start first core");
        let mut second = CoreThread::spawn(
            CoreConfig {
                device_id: "latency-second".to_owned(),
                control_port: free_udp_port(),
                discovery_port,
                ..CoreConfig::default()
            },
            EventSink::new(
                move |event| {
                    let _ = second_events_tx.send(event);
                },
                move |event| {
                    let _ = second_input_tx.send(event);
                },
            ),
        )
        .expect("start second core");
        wait_for_event(&first_events_rx, Duration::from_secs(8), |event| {
            event["event"] == "peer_up" && event["peer"]["id"] == "latency-second"
        });
        first
            .send(Command::Connect("latency-second".to_owned()))
            .expect("request connection");
        wait_for_event(&first_events_rx, Duration::from_secs(6), |event| {
            event["event"] == "connected"
        });
        wait_for_event(&second_events_rx, Duration::from_secs(6), |event| {
            event["event"] == "connected"
        });

        for round in 0..3 {
            let sent = Instant::now();
            first
                .send(Command::SendInput(LcInputEvent {
                    kind: LC_INPUT_MOUSE_MOVE,
                    delta_x: 1.0,
                    ..LcInputEvent::default()
                }))
                .expect("send input");
            second_input_rx
                .recv_timeout(Duration::from_millis(150))
                .unwrap_or_else(|_| panic!("round {round}: input not delivered within 150 ms"));
            assert!(sent.elapsed() < Duration::from_millis(150));
            std::thread::sleep(Duration::from_millis(300));
        }

        first.stop_and_join();
        second.stop_and_join();
    }

    /// Tests that start real cores run one at a time. Two cores of one test
    /// share a discovery port on the same machine; with several tests doing
    /// that at once, loopback multicast occasionally drops announcements,
    /// which a real network with one core per machine does not see.
    fn network_test() -> std::sync::MutexGuard<'static, ()> {
        static NETWORK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        NETWORK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct TestCore {
        thread: CoreThread,
        events: Receiver<String>,
        input: Receiver<LcInputEvent>,
        port: u16,
    }

    fn test_core(device_id: &str, discovery_port: u16) -> TestCore {
        test_core_with(device_id, discovery_port, |_| {})
    }

    fn test_core_with(
        device_id: &str,
        discovery_port: u16,
        customize: impl FnOnce(&mut CoreConfig),
    ) -> TestCore {
        let (events_tx, events) = mpsc::channel();
        let (input_tx, input) = mpsc::channel();
        let port = free_udp_port();
        let mut config = CoreConfig {
            device_id: device_id.to_owned(),
            control_port: port,
            discovery_port,
            ..CoreConfig::default()
        };
        customize(&mut config);
        let thread = CoreThread::spawn(
            config,
            EventSink::new(
                move |event| {
                    let _ = events_tx.send(event);
                },
                move |event| {
                    let _ = input_tx.send(event);
                },
            ),
        )
        .expect("start core");
        TestCore {
            thread,
            events,
            input,
            port,
        }
    }

    /// Waits until every listed peer was discovered, in whatever order.
    fn wait_for_peers(core: &TestCore, peer_ids: &[&str]) {
        let mut missing: Vec<&str> = peer_ids.to_vec();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !missing.is_empty() {
            let message = core
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("peers not discovered: {missing:?}"));
            let event: Value = serde_json::from_str(&message).expect("valid event JSON");
            if event["event"] == "peer_up" {
                missing.retain(|id| event["peer"]["id"] != *id);
            }
        }
    }

    fn wait_connected(core: &TestCore, peer_id: &str) {
        wait_for_event(&core.events, Duration::from_secs(6), |event| {
            event["event"] == "connected" && event["peer"]["id"] == peer_id
        });
    }

    fn motion(delta_x: f64) -> LcInputEvent {
        LcInputEvent {
            kind: LC_INPUT_MOUSE_MOVE,
            delta_x,
            ..LcInputEvent::default()
        }
    }

    /// The motion events received within `wait`, as their `delta_x`.
    fn received(core: &TestCore, wait: Duration) -> Vec<f64> {
        let deadline = Instant::now() + wait;
        let mut deltas = Vec::new();
        while let Ok(event) = core
            .input
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if event.kind == LC_INPUT_MOUSE_MOVE {
                deltas.push(event.delta_x);
            }
        }
        deltas
    }

    fn events_named(core: &TestCore, name: &str) -> Vec<Value> {
        core.events
            .try_iter()
            .filter_map(|event| serde_json::from_str::<Value>(&event).ok())
            .filter(|event| event["event"] == name)
            .collect()
    }

    #[test]
    fn sessions_with_several_peers_stay_open_and_input_follows_the_target() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("multi-a", discovery_port);
        let mut b = test_core("multi-b", discovery_port);
        let mut c = test_core("multi-c", discovery_port);
        wait_for_peers(&a, &["multi-b", "multi-c"]);

        a.thread.send(Command::Connect("multi-b".into())).unwrap();
        wait_connected(&a, "multi-b");
        a.thread.send(Command::Connect("multi-c".into())).unwrap();
        wait_connected(&a, "multi-c");
        wait_connected(&b, "multi-a");
        wait_connected(&c, "multi-a");

        // The last connect made C the target; B's session stays open.
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&c, Duration::from_millis(400)), vec![1.0]);
        assert!(received(&b, Duration::from_millis(100)).is_empty());

        a.thread
            .send(Command::SetInputTarget(Some("multi-b".into())))
            .unwrap();
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![2.0]);
        assert!(received(&c, Duration::from_millis(100)).is_empty());
        // Switching away gave control back to C.
        wait_for_event(&c.events, Duration::from_secs(2), |event| {
            event["event"] == "remote_control_ended" && event["peer_id"] == "multi-a"
        });

        // Closing C leaves B connected and still receiving input.
        a.thread
            .send(Command::DisconnectPeer("multi-c".into()))
            .unwrap();
        // C sees an orderly close, not a failure.
        wait_for_event(&c.events, Duration::from_secs(3), |event| {
            event["event"] == "disconnected"
                && event["peer_id"] == "multi-a"
                && event["reason"] == "peer closed the connection"
        });
        a.thread.send(Command::SendInput(motion(3.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![3.0]);
        assert!(events_named(&b, "disconnected").is_empty());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        c.thread.stop_and_join();
    }

    #[test]
    fn an_unidentified_connection_does_not_replace_a_session() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("stranger-a", discovery_port);
        let mut b = test_core("stranger-b", discovery_port);
        wait_for_peers(&a, &["stranger-b"]);
        a.thread
            .send(Command::Connect("stranger-b".into()))
            .unwrap();
        wait_connected(&a, "stranger-b");
        wait_connected(&b, "stranger-a");

        // Something on the network connects to B and never says hello.
        let _stranger = QuicStranger::connect(b.port);
        std::thread::sleep(Duration::from_millis(200));

        a.thread.send(Command::SendInput(motion(5.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![5.0]);
        assert!(events_named(&b, "disconnected").is_empty());
        assert!(events_named(&a, "disconnected").is_empty());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    /// A device that completes the QUIC handshake with a certificate of its
    /// own and opens a stream, then sends half a frame and nothing more.
    struct QuicStranger {
        _endpoints: crate::quic::QuicEndpoints,
        _connection: quinn::Connection,
        _send: quinn::SendStream,
        // Last: the endpoints need it while they are dropped.
        _runtime: tokio::runtime::Runtime,
    }

    impl QuicStranger {
        fn connect(port: u16) -> Self {
            let runtime = tokio::runtime::Runtime::new().expect("stranger runtime");
            let (endpoints, connection, send) = runtime.block_on(async {
                let identity = crate::secure::Identity::load_or_create(None, "stranger").unwrap();
                let endpoints = crate::quic::QuicEndpoints::bind(0, &identity).unwrap();
                let connection = endpoints
                    .control
                    .connect(([127, 0, 0, 1], port).into(), crate::secure::SERVER_NAME)
                    .unwrap()
                    .await
                    .expect("connect to the core");
                let (mut send, _recv) = connection.open_bi().await.unwrap();
                send.write_all(&[0, 0]).await.unwrap();
                (endpoints, connection, send)
            });
            Self {
                _endpoints: endpoints,
                _connection: connection,
                _send: send,
                _runtime: runtime,
            }
        }
    }

    #[test]
    fn peers_dialing_each_other_at_once_keep_exactly_one_session() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("dial-a", discovery_port);
        let mut b = test_core("dial-b", discovery_port);
        wait_for_peers(&a, &["dial-b"]);
        wait_for_peers(&b, &["dial-a"]);

        a.thread.send(Command::Connect("dial-b".into())).unwrap();
        b.thread.send(Command::Connect("dial-a".into())).unwrap();
        wait_connected(&a, "dial-b");
        wait_connected(&b, "dial-a");
        // Let both duplicates arrive and be resolved.
        std::thread::sleep(Duration::from_millis(500));

        a.thread.send(Command::SendInput(motion(7.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(500)), vec![7.0]);
        b.thread.send(Command::SendInput(motion(8.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(500)), vec![8.0]);
        // Duplicates are closed quietly: neither side saw a disconnect.
        assert!(events_named(&a, "disconnected").is_empty());
        assert!(events_named(&b, "disconnected").is_empty());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    fn wait_revoked(core: &TestCore, peer_id: &str, reason: &str) -> Value {
        wait_for_event(&core.events, Duration::from_secs(3), |event| {
            event["event"] == "control_revoked"
                && event["peer_id"] == peer_id
                && event["reason"] == reason
        })
    }

    fn wait_control_ended(core: &TestCore, peer_id: &str) {
        wait_for_event(&core.events, Duration::from_secs(3), |event| {
            event["event"] == "remote_control_ended" && event["peer_id"] == peer_id
        });
    }

    #[test]
    fn a_device_driving_a_peer_gives_way_when_another_takes_it_over() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("drive-a", discovery_port);
        let mut b = test_core("drive-b", discovery_port);
        let mut c = test_core("drive-c", discovery_port);
        wait_for_peers(&a, &["drive-b", "drive-c"]);
        wait_for_peers(&c, &["drive-a"]);
        a.thread.send(Command::Connect("drive-b".into())).unwrap();
        wait_connected(&a, "drive-b");
        wait_connected(&b, "drive-a");
        c.thread.send(Command::Connect("drive-a".into())).unwrap();
        wait_connected(&c, "drive-a");

        // A drives B; then C starts driving A.
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![1.0]);
        c.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![2.0]);

        // A's control of B ends: its shell is told, and B is released.
        let revoked = wait_revoked(&a, "drive-b", "taken_over");
        assert_eq!(revoked["by"], "drive-c");
        wait_control_ended(&b, "drive-a");
        // Until A's shell acknowledges, its leftover input goes nowhere.
        a.thread.send(Command::SendInput(motion(3.0))).unwrap();
        assert!(received(&b, Duration::from_millis(300)).is_empty());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        c.thread.stop_and_join();
    }

    #[test]
    fn a_new_controller_takes_a_device_from_the_previous_one() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("taken-a", discovery_port);
        let mut b = test_core("taken-b", discovery_port);
        let mut c = test_core("taken-c", discovery_port);
        wait_for_peers(&b, &["taken-a"]);
        wait_for_peers(&c, &["taken-a"]);
        b.thread.send(Command::Connect("taken-a".into())).unwrap();
        wait_connected(&b, "taken-a");
        c.thread.send(Command::Connect("taken-a".into())).unwrap();
        wait_connected(&c, "taken-a");

        // B drives A; then C starts driving A too.
        b.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![1.0]);
        c.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![2.0]);

        // B is told to stop and its pointer comes home; A only plays C.
        wait_revoked(&b, "taken-a", "revoked");
        wait_control_ended(&a, "taken-b");
        b.thread.send(Command::SendInput(motion(3.0))).unwrap();
        c.thread.send(Command::SendInput(motion(4.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![4.0]);

        // Once B's shell has given control back, B can take A again.
        b.thread.send(Command::ResetInput).unwrap();
        b.thread
            .send(Command::SetInputTarget(Some("taken-a".into())))
            .unwrap();
        b.thread.send(Command::SendInput(motion(5.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![5.0]);
        wait_revoked(&c, "taken-a", "revoked");

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        c.thread.stop_and_join();
    }

    #[test]
    fn pushing_into_the_controlling_peer_hands_control_over() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("handoff-a", discovery_port);
        let mut b = test_core("handoff-b", discovery_port);
        wait_for_peers(&b, &["handoff-a"]);
        b.thread.send(Command::Connect("handoff-a".into())).unwrap();
        wait_connected(&b, "handoff-a");
        wait_connected(&a, "handoff-b");

        // B drives A; then someone at A pushes the pointer over to B.
        b.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![1.0]);
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![2.0]);

        // B gives way and leaves its pointer to A; B's stale input is dropped.
        let revoked = wait_revoked(&b, "handoff-a", "taken_over");
        assert_eq!(revoked["by"], "handoff-a");
        wait_control_ended(&a, "handoff-b");
        b.thread.send(Command::SendInput(motion(3.0))).unwrap();
        assert!(received(&a, Duration::from_millis(300)).is_empty());
        a.thread.send(Command::SendInput(motion(4.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![4.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    /// This core's certificate fingerprint, from its `started` event (call
    /// before anything else reads its events).
    fn started_fingerprint(core: &TestCore) -> String {
        let started = wait_for_event(&core.events, Duration::from_secs(3), |event| {
            event["event"] == "started"
        });
        started["fingerprint"]
            .as_str()
            .expect("a QUIC identity")
            .to_owned()
    }

    fn wait_connected_event(core: &TestCore, peer_id: &str) -> Value {
        wait_for_event(&core.events, Duration::from_secs(6), |event| {
            event["event"] == "connected" && event["peer"]["id"] == peer_id
        })
    }

    #[test]
    fn sessions_run_encrypted_over_quic_and_know_the_peer_certificate() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("quic-a", discovery_port);
        let mut b = test_core("quic-b", discovery_port);
        let a_fingerprint = started_fingerprint(&a);
        let b_fingerprint = started_fingerprint(&b);
        wait_for_peers(&a, &["quic-b"]);
        a.thread.send(Command::Connect("quic-b".into())).unwrap();

        let on_a = wait_connected_event(&a, "quic-b");
        assert_eq!(on_a["paired"], false);
        assert_eq!(on_a["fingerprint"], b_fingerprint.as_str());
        let on_b = wait_connected_event(&b, "quic-a");
        assert_eq!(on_b["fingerprint"], a_fingerprint.as_str());

        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![1.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn devices_that_require_pairing_pair_by_comparing_codes() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let require = |config: &mut CoreConfig| config.require_pairing = true;
        let mut a = test_core_with("pair-a", discovery_port, require);
        let mut b = test_core_with("pair-b", discovery_port, require);
        let a_fingerprint = started_fingerprint(&a);
        let b_fingerprint = started_fingerprint(&b);
        wait_for_peers(&a, &["pair-b"]);
        a.thread.send(Command::Connect("pair-b".into())).unwrap();

        // Both devices show the same code, each about the other.
        let request = |event: &Value| event["event"] == "pairing_requested";
        let on_a = wait_for_event(&a.events, Duration::from_secs(6), request);
        let on_b = wait_for_event(&b.events, Duration::from_secs(6), request);
        assert_eq!(on_a["peer_id"], "pair-b");
        assert_eq!(on_b["peer_id"], "pair-a");
        assert_eq!(on_a["code"], on_b["code"]);
        assert_eq!(on_a["code"].as_str().unwrap().len(), 6);
        assert_eq!(on_a["fingerprint"], b_fingerprint.as_str());

        // Nothing flows before both users accepted.
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        b.thread
            .send(Command::AcceptPairing("pair-a".into()))
            .unwrap();
        assert!(received(&b, Duration::from_millis(300)).is_empty());
        assert!(events_named(&a, "connected").is_empty());

        a.thread
            .send(Command::AcceptPairing("pair-b".into()))
            .unwrap();
        let paired = wait_for_event(&a.events, Duration::from_secs(3), |event| {
            event["event"] == "paired"
        });
        assert_eq!(paired["fingerprint"], b_fingerprint.as_str());
        let connected = wait_connected_event(&a, "pair-b");
        assert_eq!(connected["paired"], true);
        let paired = wait_for_event(&b.events, Duration::from_secs(3), |event| {
            event["event"] == "paired"
        });
        assert_eq!(paired["fingerprint"], a_fingerprint.as_str());
        wait_connected_event(&b, "pair-a");

        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![2.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn a_device_that_does_not_require_pairing_still_pairs_when_asked() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let require = |config: &mut CoreConfig| config.require_pairing = true;
        let mut a = test_core_with("ask-a", discovery_port, require);
        let mut b = test_core("ask-b", discovery_port);
        wait_for_peers(&a, &["ask-b"]);
        a.thread.send(Command::Connect("ask-b".into())).unwrap();

        // B lets A in at once, then shows the code A asked it to check.
        let request = |event: &Value| event["event"] == "pairing_requested";
        let on_a = wait_for_event(&a.events, Duration::from_secs(6), request);
        let on_b = wait_for_event(&b.events, Duration::from_secs(6), request);
        assert_eq!(on_a["code"], on_b["code"]);
        b.thread
            .send(Command::AcceptPairing("ask-a".into()))
            .unwrap();
        a.thread
            .send(Command::AcceptPairing("ask-b".into()))
            .unwrap();
        for core in [&a, &b] {
            wait_for_event(&core.events, Duration::from_secs(3), |event| {
                event["event"] == "paired"
            });
        }
        wait_connected_event(&a, "ask-b");
        a.thread.send(Command::SendInput(motion(4.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![4.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn declining_pairing_closes_the_connection_on_both_sides() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let require = |config: &mut CoreConfig| config.require_pairing = true;
        let mut a = test_core_with("decline-a", discovery_port, require);
        let mut b = test_core_with("decline-b", discovery_port, require);
        wait_for_peers(&a, &["decline-b"]);
        a.thread.send(Command::Connect("decline-b".into())).unwrap();
        wait_for_event(&b.events, Duration::from_secs(6), |event| {
            event["event"] == "pairing_requested"
        });
        b.thread
            .send(Command::RejectPairing("decline-a".into()))
            .unwrap();
        let ended = wait_for_event(&a.events, Duration::from_secs(4), |event| {
            event["event"] == "pairing_ended"
        });
        assert_eq!(ended["peer_id"], "decline-b");
        // The side that was declined is told so, not just that the
        // connection closed.
        assert_eq!(ended["reason"], crate::transport::PAIRING_DECLINED_BY_PEER);
        let disconnected = wait_for_event(&a.events, Duration::from_secs(4), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "decline-b"
        });
        assert_eq!(
            disconnected["reason"],
            crate::transport::PAIRING_DECLINED_BY_PEER
        );
        assert!(events_named(&a, "paired").is_empty());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn paired_devices_reconnect_without_asking_and_a_changed_identity_is_refused() {
        let _network = network_test();
        let directory =
            std::env::temp_dir().join(format!("linkcursor-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = |name: &str| directory.join(name).to_string_lossy().into_owned();
        let identity = |name: &str| {
            crate::secure::Identity::load_or_create(Some(std::path::Path::new(&path(name))), name)
                .unwrap()
                .fingerprint
        };
        let (a_fingerprint, b_fingerprint) = (identity("trust-a"), identity("trust-b"));
        let trusting = |own: &'static str, other: &'static str, fingerprint: String| {
            let identity_path = path(own);
            move |config: &mut CoreConfig| {
                config.require_pairing = true;
                config.identity_path = Some(identity_path);
                config.trusted_peers = vec![TrustedPeer {
                    peer_id: other.to_owned(),
                    fingerprint,
                }];
            }
        };

        // Paired before (the shell passes what `paired` reported): straight in.
        let discovery_port = free_udp_port();
        let mut a = test_core_with(
            "trust-a",
            discovery_port,
            trusting("trust-a", "trust-b", b_fingerprint.clone()),
        );
        let mut b = test_core_with(
            "trust-b",
            discovery_port,
            trusting("trust-b", "trust-a", a_fingerprint.clone()),
        );
        wait_for_peers(&a, &["trust-b"]);
        a.thread.send(Command::Connect("trust-b".into())).unwrap();
        let connected = wait_connected_event(&a, "trust-b");
        assert_eq!(connected["paired"], true);
        assert!(events_named(&a, "pairing_requested").is_empty());
        a.thread.stop_and_join();
        b.thread.stop_and_join();

        // A device claiming to be a paired one with another certificate.
        let discovery_port = free_udp_port();
        let mut a = test_core_with(
            "trust-a",
            discovery_port,
            trusting("trust-a", "trust-b", "00".repeat(32)),
        );
        let mut b = test_core_with(
            "trust-b",
            discovery_port,
            trusting("trust-b", "trust-a", a_fingerprint),
        );
        wait_for_peers(&a, &["trust-b"]);
        a.thread.send(Command::Connect("trust-b".into())).unwrap();
        let refused = wait_for_event(&a.events, Duration::from_secs(6), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "trust-b"
        });
        assert_eq!(refused["reason"], runtime_reason::IDENTITY_CHANGED);
        assert!(events_named(&a, "connected").is_empty());
        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn requiring_pairing_ends_sessions_with_unpaired_peers() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("strict-a", discovery_port);
        let mut b = test_core("strict-b", discovery_port);
        wait_for_peers(&a, &["strict-b"]);
        a.thread.send(Command::Connect("strict-b".into())).unwrap();
        wait_connected(&a, "strict-b");

        a.thread.send(Command::SetRequirePairing(true)).unwrap();
        let ended = wait_for_event(&a.events, Duration::from_secs(3), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "strict-b"
        });
        assert_eq!(ended["reason"], runtime_reason::PAIRING_REQUIRED);
        // Connecting again asks to pair.
        a.thread.send(Command::Connect("strict-b".into())).unwrap();
        wait_for_event(&a.events, Duration::from_secs(6), |event| {
            event["event"] == "pairing_requested" && event["peer_id"] == "strict-b"
        });

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn a_device_that_refuses_remote_control_sends_its_controller_back() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("refuse-a", discovery_port);
        let mut b = test_core("refuse-b", discovery_port);
        wait_for_peers(&b, &["refuse-a"]);
        b.thread.send(Command::Connect("refuse-a".into())).unwrap();
        wait_connected(&b, "refuse-a");
        wait_connected(&a, "refuse-b");

        // B drives A; then A stops allowing it.
        b.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![1.0]);
        a.thread
            .send(Command::SetRemoteControlAllowed(false))
            .unwrap();
        wait_revoked(&b, "refuse-a", "revoked");
        wait_control_ended(&a, "refuse-b");

        // B's shell gives control back and pushes in again: sent back again,
        // and nothing is played.
        b.thread.send(Command::ResetInput).unwrap();
        b.thread.send(Command::SendInput(motion(2.0))).unwrap();
        wait_revoked(&b, "refuse-a", "revoked");
        assert!(received(&a, Duration::from_millis(300)).is_empty());
        assert!(events_named(&a, "remote_control_started").is_empty());

        // Allowed again: B drives A.
        a.thread
            .send(Command::SetRemoteControlAllowed(true))
            .unwrap();
        b.thread.send(Command::ResetInput).unwrap();
        b.thread.send(Command::SendInput(motion(3.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![3.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn connected_peers_report_their_round_trip() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("rtt-a", discovery_port);
        let mut b = test_core("rtt-b", discovery_port);
        wait_for_peers(&a, &["rtt-b"]);
        a.thread.send(Command::Connect("rtt-b".into())).unwrap();
        wait_connected(&a, "rtt-b");

        // Pings go out every second; both sides time their own.
        for (core, peer) in [(&a, "rtt-b"), (&b, "rtt-a")] {
            let event = wait_for_event(&core.events, Duration::from_secs(4), |event| {
                event["event"] == "latency" && event["peer_id"] == peer
            });
            let ms = event["ms"].as_f64().expect("latency in ms");
            assert!((0.0..1000.0).contains(&ms), "round trip {ms} ms");
        }

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn driving_another_peer_stops_whoever_drives_this_device() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("local-a", discovery_port);
        let mut b = test_core("local-b", discovery_port);
        let mut c = test_core("local-c", discovery_port);
        wait_for_peers(&a, &["local-c"]);
        wait_for_peers(&b, &["local-a"]);
        b.thread.send(Command::Connect("local-a".into())).unwrap();
        wait_connected(&b, "local-a");
        wait_connected(&a, "local-b");
        a.thread.send(Command::Connect("local-c".into())).unwrap();
        wait_connected(&a, "local-c");

        // B drives A; then someone at A pushes the pointer over to C.
        b.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![1.0]);
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&c, Duration::from_millis(400)), vec![2.0]);

        // B is told to stop, and A no longer plays what B sends.
        wait_revoked(&b, "local-a", "revoked");
        wait_control_ended(&a, "local-b");
        b.thread.send(Command::SendInput(motion(3.0))).unwrap();
        assert!(received(&a, Duration::from_millis(300)).is_empty());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        c.thread.stop_and_join();
    }

    #[test]
    fn a_controller_that_cannot_be_told_is_ignored_until_it_resets() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("older-a", discovery_port);
        // An older build: it does not know `ControlRevoke`.
        let mut b = test_core_with("older-b", discovery_port, |config| {
            config.capabilities = LOCAL_CAPABILITIES & !CAPABILITY_CONTROL_REVOKE;
        });
        let mut c = test_core("older-c", discovery_port);
        wait_for_peers(&b, &["older-a"]);
        wait_for_peers(&c, &["older-a"]);
        b.thread.send(Command::Connect("older-a".into())).unwrap();
        wait_connected(&b, "older-a");
        c.thread.send(Command::Connect("older-a".into())).unwrap();
        wait_connected(&c, "older-a");

        b.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![1.0]);
        c.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![2.0]);
        wait_control_ended(&a, "older-b");

        // B was sent nothing it cannot read, so it stays connected, but A
        // plays only C until B gives control back by itself.
        b.thread.send(Command::SendInput(motion(3.0))).unwrap();
        c.thread.send(Command::SendInput(motion(4.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![4.0]);
        assert!(events_named(&b, "control_revoked").is_empty());
        assert!(events_named(&b, "disconnected").is_empty());

        b.thread.send(Command::ResetInput).unwrap();
        b.thread.send(Command::SendInput(motion(5.0))).unwrap();
        assert_eq!(received(&a, Duration::from_millis(400)), vec![5.0]);
        wait_revoked(&c, "older-a", "revoked");

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        c.thread.stop_and_join();
    }

    #[track_caller]
    /// The text of the next `clipboard` event, if one arrives within `wait`.
    fn clipboard_text(core: &TestCore, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        while let Ok(message) = core
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            let event: Value = serde_json::from_str(&message).expect("valid event JSON");
            if event["event"] == "clipboard" {
                let clipboard =
                    Clipboard::from_json(&serde_json::json!({ "formats": event["formats"] }))
                        .expect("clipboard event formats");
                let text = &clipboard.formats[0];
                assert_eq!(text.mime, MIME_TEXT);
                return Some(String::from_utf8(text.data.clone()).unwrap());
            }
        }
        None
    }

    #[test]
    fn the_clipboard_follows_control_both_ways_without_echo() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let with_clipboard = |config: &mut CoreConfig| config.clipboard = true;
        let mut a = test_core_with("clip-a", discovery_port, with_clipboard);
        let mut b = test_core_with("clip-b", discovery_port, with_clipboard);
        wait_for_peers(&a, &["clip-b"]);
        a.thread.send(Command::Connect("clip-b".into())).unwrap();
        wait_connected(&a, "clip-b");
        wait_connected(&b, "clip-a");

        // A copies, then drives B: B gets A's clipboard.
        a.thread
            .send(Command::SetClipboard(text_clipboard("from a")))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(
            clipboard_text(&b, Duration::from_secs(2)).as_deref(),
            Some("from a")
        );

        // Copying on B while A drives it, then giving control back, brings
        // B's clipboard to A.
        b.thread
            .send(Command::SetClipboard(text_clipboard("from b")))
            .unwrap();
        a.thread.send(Command::ResetInput).unwrap();
        assert_eq!(
            clipboard_text(&a, Duration::from_secs(2)).as_deref(),
            Some("from b")
        );

        // A's shell writes it and reports the change; driving B again sends
        // nothing, B has those contents already.
        a.thread
            .send(Command::SetClipboard(text_clipboard("from b")))
            .unwrap();
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(clipboard_text(&b, Duration::from_millis(500)), None);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn a_peer_without_clipboard_support_gets_none_and_stays_connected() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core_with("plain-a", discovery_port, |config| config.clipboard = true);
        let mut b = test_core("plain-b", discovery_port);
        wait_for_peers(&a, &["plain-b"]);
        a.thread.send(Command::Connect("plain-b".into())).unwrap();
        wait_connected(&a, "plain-b");
        wait_connected(&b, "plain-a");

        a.thread
            .send(Command::SetClipboard(text_clipboard("secret")))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(500)), vec![1.0]);
        assert!(events_named(&a, "clipboard_sent").is_empty());
        assert!(events_named(&b, "disconnected").is_empty());
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(500)), vec![2.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "linkcursor-test-{name}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos() ^ (name.len() as u128 * 7919)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uri_clipboard(paths: &[PathBuf]) -> Clipboard {
        Clipboard {
            formats: vec![
                ClipboardFormat {
                    mime: MIME_URI_LIST.to_owned(),
                    data: transfer_files::uri_list(paths),
                },
                ClipboardFormat {
                    mime: MIME_TEXT.to_owned(),
                    data: b"names".to_vec(),
                },
            ],
        }
    }

    /// Two cores with clipboard and files, B receiving into its own folder.
    fn file_cores(name: &str, limit: u64) -> (TestCore, TestCore, PathBuf, PathBuf) {
        let discovery_port = free_udp_port();
        let source = temp_dir(&format!("{name}-src"));
        let downloads = temp_dir(&format!("{name}-dst"));
        let a = test_core_with(&format!("{name}-a"), discovery_port, |config| {
            config.clipboard = true;
            config.files = true;
            config.download_directory =
                Some(temp_dir(&format!("{name}-a-dst")).to_string_lossy().into());
        });
        let dst = downloads.to_string_lossy().into_owned();
        let b = test_core_with(&format!("{name}-b"), discovery_port, move |config| {
            config.clipboard = true;
            config.files = true;
            config.download_directory = Some(dst);
            config.max_auto_receive_bytes = limit;
        });
        wait_for_peers(&a, &[&format!("{name}-b")]);
        a.thread
            .send(Command::Connect(format!("{name}-b")))
            .unwrap();
        wait_connected(&a, &format!("{name}-b"));
        wait_connected(&b, &format!("{name}-a"));
        (a, b, source, downloads)
    }

    fn pattern(length: usize, seed: u8) -> Vec<u8> {
        (0..length)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn copied_files_and_folders_arrive_complete_without_overwriting() {
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores("files", u64::MAX);
        let report = source.join("报告");
        std::fs::create_dir_all(report.join("img")).unwrap();
        std::fs::write(report.join("a.txt"), pattern(100_000, 1)).unwrap();
        std::fs::write(report.join("empty.txt"), b"").unwrap();
        std::fs::write(report.join("img").join("b.bin"), pattern(300_001, 2)).unwrap();
        std::fs::write(source.join("note.txt"), b"hello").unwrap();
        // Something called note.txt is already there: it must survive.
        std::fs::write(downloads.join("note.txt"), b"keep me").unwrap();

        a.thread
            .send(Command::SetClipboard(uri_clipboard(&[
                report.clone(),
                source.join("note.txt"),
            ])))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();

        let done = wait_for_event(&b.events, Duration::from_secs(10), |event| {
            event["event"] == "files_received" || event["event"] == "files_failed"
        });
        assert_eq!(done["event"], "files_received", "{done}");
        let got = |path: &Path| std::fs::read(path).unwrap();
        assert_eq!(got(&downloads.join("报告/a.txt")), pattern(100_000, 1));
        assert_eq!(got(&downloads.join("报告/empty.txt")), b"");
        assert_eq!(got(&downloads.join("报告/img/b.bin")), pattern(300_001, 2));
        assert_eq!(got(&downloads.join("note (1).txt")), b"hello");
        assert_eq!(got(&downloads.join("note.txt")), b"keep me");
        // No partial file is left anywhere.
        let leftovers: Vec<_> = walk(&downloads)
            .into_iter()
            .filter(|path| path.to_string_lossy().ends_with(".partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // The clipboard that follows lists the received top-level paths.
        let clipboard = wait_for_event(&b.events, Duration::from_secs(2), |event| {
            event["event"] == "clipboard"
        });
        let clipboard =
            Clipboard::from_json(&serde_json::json!({ "formats": clipboard["formats"] })).unwrap();
        assert_eq!(clipboard.formats[0].mime, MIME_URI_LIST);
        let pasted = transfer_files::parse_uri_list(&clipboard.formats[0].data);
        assert_eq!(
            pasted,
            vec![downloads.join("报告"), downloads.join("note (1).txt")]
        );
        assert_eq!(clipboard.formats[1].data, b"names");

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(downloads);
    }

    #[test]
    fn large_transfers_wait_for_acceptance_and_changed_files_are_refused() {
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores("offer", 10);
        let file = source.join("big.bin");
        std::fs::write(&file, pattern(50_000, 3)).unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(std::slice::from_ref(
                &file,
            ))))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        let offered = wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_offered"
        });
        assert_eq!(offered["total_size"], 50_000);
        assert_eq!(offered["count"], 1);
        let transfer = offered["transfer"].as_u64().unwrap() as u32;
        b.thread.send(Command::AcceptFiles(transfer)).unwrap();
        wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_received"
        });
        assert_eq!(
            std::fs::read(downloads.join("big.bin")).unwrap(),
            pattern(50_000, 3)
        );

        // A new copy whose file then changes before it is accepted.
        let other = source.join("other.bin");
        std::fs::write(&other, pattern(40_000, 4)).unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(std::slice::from_ref(
                &other,
            ))))
            .unwrap();
        a.thread.send(Command::ResetInput).unwrap();
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        let offered = wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_offered"
        });
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&other, pattern(40_001, 5)).unwrap();
        b.thread
            .send(Command::AcceptFiles(
                offered["transfer"].as_u64().unwrap() as u32
            ))
            .unwrap();
        let failed = wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_failed" || event["event"] == "files_received"
        });
        assert_eq!(failed["event"], "files_failed", "{failed}");
        assert!(!downloads.join("other.bin").exists());
        assert!(
            walk(&downloads)
                .iter()
                .all(|p| !p.to_string_lossy().ends_with(".partial"))
        );

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(downloads);
    }

    #[test]
    fn input_keeps_flowing_during_a_large_transfer() {
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores("flow", u64::MAX);
        let file = source.join("large.bin");
        std::fs::write(&file, pattern(8 * 1024 * 1024, 6)).unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(&[file])))
            .unwrap();
        a.thread.send(Command::SendInput(motion(0.5))).unwrap();
        wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_progress"
        });
        // While the file streams, every motion still arrives promptly.
        for delta in 1..=5 {
            let sent = Instant::now();
            a.thread
                .send(Command::SendInput(motion(delta as f64)))
                .unwrap();
            let deltas = received(&b, Duration::from_millis(300));
            assert!(
                deltas.contains(&(delta as f64)),
                "motion {delta} late: {deltas:?}"
            );
            assert!(sent.elapsed() < Duration::from_millis(400));
        }
        wait_for_event(&b.events, Duration::from_secs(20), |event| {
            event["event"] == "files_received"
        });
        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(downloads);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk(&path));
            }
            found.push(path);
        }
        found
    }

    fn wait_for_event(
        receiver: &Receiver<String>,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        let mut observed = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = match receiver.recv_timeout(remaining) {
                Ok(message) => message,
                Err(_) => panic!("expected core event before timeout; observed: {observed:?}"),
            };
            let event: Value = serde_json::from_str(&message).expect("valid event JSON");
            if predicate(&event) {
                return event;
            }
            observed.push(event);
        }
    }

    fn display(width: f64, height: f64) -> Display {
        Display {
            x: 0.0,
            y: 0.0,
            width,
            height,
            scale: 2.0,
        }
    }

    #[track_caller]
    fn wait_for_placement(core: &TestCore, peer_id: &str, source: &str) -> Value {
        wait_for_event(&core.events, Duration::from_secs(4), |event| {
            event["event"] == "placement_changed"
                && event["peer_id"] == peer_id
                && event["source"] == source
        })
    }

    #[test]
    fn screen_info_and_placement_round_trip() {
        for message in [
            WireMessage::ScreenInfo {
                displays: vec![display(1512.0, 982.0), display(1920.0, 1080.0)],
            },
            WireMessage::ScreenInfo { displays: vec![] },
            WireMessage::Placement {
                offset: Some(Offset {
                    x: 1440.0,
                    y: -40.5,
                }),
                updated_ms: 1_792_000_000_000,
            },
            WireMessage::Placement {
                offset: None,
                updated_ms: 7,
            },
        ] {
            let bytes = encode_message(&message).expect("encode");
            assert_eq!(decode_message(&bytes).expect("decode"), message);
        }
        assert!(
            encode_message(&WireMessage::ScreenInfo {
                displays: vec![display(0.0, 10.0)],
            })
            .is_err()
        );
    }

    #[test]
    fn placements_sync_mirrored_and_the_newer_edit_wins() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let pc = [display(1440.0, 900.0)];
        let mac = [display(1512.0, 982.0)];
        // The Mac remembers an old placement of the PC from an earlier session.
        let mut a = test_core_with("place-pc", discovery_port, |config| {
            config.displays = pc.to_vec();
        });
        let mut b = test_core_with("place-mac", discovery_port, |config| {
            config.displays = mac.to_vec();
            config.placements = vec![PlacementRecord {
                peer_id: "place-pc".to_owned(),
                offset: Some(Offset { x: 0.0, y: 982.0 }),
                updated_ms: 1,
            }];
        });
        wait_for_peers(&a, &["place-mac"]);
        // Placing before connecting: stored here, synced once connected.
        a.thread
            .send(Command::PlaceBeside {
                peer_id: "place-mac".to_owned(),
                side: Side::Right,
            })
            .unwrap();
        let local = wait_for_placement(&a, "place-mac", "local");
        assert_eq!(local["offset"]["x"], 1440.0);

        a.thread.send(Command::Connect("place-mac".into())).unwrap();
        wait_connected(&a, "place-mac");
        // The PC's newer placement replaces the Mac's old one, mirrored.
        let synced = wait_for_placement(&b, "place-pc", "peer");
        assert_eq!(synced["offset"]["x"], -1440.0);
        assert_eq!(
            synced["offset"]["y"],
            -local["offset"]["y"].as_f64().unwrap()
        );
        // ...and the Mac's stale placement did not overwrite the PC's.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            events_named(&a, "placement_changed")
                .iter()
                .all(|event| event["source"] != "peer")
        );

        // Moving it from the Mac side: the PC follows.
        b.thread
            .send(Command::PlaceBeside {
                peer_id: "place-pc".to_owned(),
                side: Side::Bottom,
            })
            .unwrap();
        let moved = wait_for_placement(&b, "place-pc", "local");
        assert_eq!(moved["offset"]["y"], 982.0);
        let followed = wait_for_placement(&a, "place-mac", "peer");
        assert_eq!(followed["offset"]["y"], -982.0);

        // Removing is synced too.
        a.thread
            .send(Command::SetPlacement {
                peer_id: "place-mac".to_owned(),
                offset: None,
            })
            .unwrap();
        let removed = wait_for_placement(&b, "place-pc", "peer");
        assert!(removed["offset"].is_null());

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn displays_reach_peers_and_placements_follow_display_changes() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core_with("screens-a", discovery_port, |config| {
            config.displays = vec![
                display(1440.0, 900.0),
                Display {
                    x: 1440.0,
                    ..display(1920.0, 1080.0)
                },
            ];
        });
        let mut b = test_core_with("screens-b", discovery_port, |config| {
            config.displays = vec![display(1512.0, 982.0)];
        });
        // Discovery already carries the displays.
        let described = wait_for_event(&a.events, Duration::from_secs(8), |event| {
            event["event"] == "peer_displays" && event["peer_id"] == "screens-b"
        });
        assert_eq!(described["displays"][0]["width"], 1512.0);

        a.thread
            .send(Command::PlaceBeside {
                peer_id: "screens-b".to_owned(),
                side: Side::Right,
            })
            .unwrap();
        let placed = wait_for_placement(&a, "screens-b", "local");
        assert_eq!(placed["offset"]["x"], 3360.0);
        // Shells get the crossing segment without computing it.
        let portals = wait_for_event(&a.events, Duration::from_secs(2), |event| {
            event["event"] == "portals_changed"
        });
        assert_eq!(portals["portals"][0]["peer_id"], "screens-b");
        assert_eq!(portals["portals"][0]["side"], "right");
        assert_eq!(portals["portals"][0]["local_display"], 1);
        assert_eq!(portals["portals"][0]["line"], 3360.0);

        // The second monitor is unplugged: the placement moves to the
        // remaining display's edge.
        a.thread
            .send(Command::SetLocalDisplays(vec![display(1440.0, 900.0)]))
            .unwrap();
        let moved = wait_for_placement(&a, "screens-b", "local");
        assert_eq!(moved["offset"]["x"], 1440.0);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn a_saved_placement_is_refitted_once_the_peer_is_seen() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        // Saved next to a second monitor that was unplugged while this
        // device was off.
        let mut a = test_core_with("saved-a", discovery_port, |config| {
            config.displays = vec![display(1440.0, 900.0)];
            config.placements = vec![PlacementRecord {
                peer_id: "saved-b".to_owned(),
                offset: Some(Offset { x: 3360.0, y: 0.0 }),
                updated_ms: 1,
            }];
        });
        let mut b = test_core_with("saved-b", discovery_port, |config| {
            config.displays = vec![display(1512.0, 982.0)];
        });
        let refitted = wait_for_placement(&a, "saved-b", "local");
        assert_eq!(refitted["offset"]["x"], 1440.0);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    fn free_udp_port() -> u16 {
        UdpSocket::bind(("127.0.0.1", 0))
            .expect("bind temporary UDP port")
            .local_addr()
            .expect("temporary UDP address")
            .port()
    }
}
