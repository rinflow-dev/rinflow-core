// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;
mod clipboard;
mod ffi;
mod file_stream;
mod files;
mod input;
pub mod layout;
mod model;
mod priority;
mod protocol;
mod qos;
mod quic;
pub mod reason;
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
pub use file_stream::{MAX_READ_SIZE, RemoteFileLease};
pub use files::MIME_URI_LIST;
pub use files::{
    Entry as FileEntry, Manifest as FileManifest, sanitize_name as sanitize_file_name,
};
pub use model::{
    CoreConfig, LC_INPUT_FLAG_ABSOLUTE_POSITION, LC_INPUT_FLAG_KEY_REPEAT,
    LC_INPUT_FLAG_PRECISE_SCROLL, LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_INPUT_MOUSE_BUTTON_DOWN,
    LC_INPUT_MOUSE_BUTTON_UP, LC_INPUT_MOUSE_MOVE, LC_INPUT_SCROLL, LC_MODIFIER_CAPS_LOCK,
    LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL, LC_MODIFIER_LEFT_COMMAND, LC_MODIFIER_LEFT_CONTROL,
    LC_MODIFIER_LEFT_OPTION, LC_MODIFIER_LEFT_SHIFT, LC_MODIFIER_OPTION, LC_MODIFIER_RIGHT_COMMAND,
    LC_MODIFIER_RIGHT_CONTROL, LC_MODIFIER_RIGHT_OPTION, LC_MODIFIER_RIGHT_SHIFT,
    LC_MODIFIER_SHIFT, LcInputEvent, PlacementRecord, TrustedPeer,
};
pub use remap::{Modifier, ModifierMap};
pub use runtime::manual::parse_address;

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
        LC_INPUT_FLAG_KEY_REPEAT, LC_INPUT_KEY_DOWN, LC_INPUT_KEY_UP, LC_INPUT_MOUSE_BUTTON_DOWN,
        LC_INPUT_MOUSE_BUTTON_UP, LC_INPUT_MOUSE_MOVE, LC_MODIFIER_COMMAND, LC_MODIFIER_CONTROL,
        LC_MODIFIER_LEFT_COMMAND, LC_MODIFIER_LEFT_CONTROL, LC_MODIFIER_SHIFT, LOCAL_CAPABILITIES,
        LcInputEvent, PROTOCOL_MAX_VERSION, WireMessage,
    };
    use super::model::{CAPABILITY_PAIRING_V2, PlacementRecord, TrustedPeer};
    use super::protocol::{decode_message, encode_message};
    use super::remap::{Modifier, ModifierMap};
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
        // Names a Mac allows are translated, never refused: one of them must
        // not stop a whole folder.
        assert_eq!(clean("a<b>|c?.txt").as_deref(), Some("a_b__c_.txt"));
        // ':' (a '/' typed in Finder) and '\' become '_': no drive letter,
        // stream or separator is left.
        assert_eq!(clean("2026:10:30.txt").as_deref(), Some("2026_10_30.txt"));
        assert_eq!(clean("C:/x").as_deref(), Some("C_/x"));
        assert_eq!(clean("a\\..\\..\\x").as_deref(), Some("a_.._.._x"));
        // A folder with a custom icon holds "Icon\r".
        assert_eq!(clean("相册/Icon\r").as_deref(), Some("相册/Icon"));
        assert_eq!(clean("tab\there").as_deref(), Some("tabhere"));
        assert_eq!(clean("trailing. . ").as_deref(), Some("trailing"));
        // Left with no name: '_'.
        assert_eq!(clean("...").as_deref(), Some("_"));
        assert_eq!(clean("a/ /b").as_deref(), Some("a/_/b"));
        // Device names, on any level and with superscript digits.
        assert_eq!(clean("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(clean("com1").as_deref(), Some("_com1"));
        assert_eq!(clean("lpt²/aux").as_deref(), Some("_lpt²/_aux"));
        assert_eq!(clean("conout$").as_deref(), Some("_conout$"));
        // Only what could leave the destination is refused.
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "./a", "a/"] {
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
            WireMessage::PairCommit {
                commitment: [7; 32],
            },
            WireMessage::PairNonce { nonce: [8; 32] },
            WireMessage::PairReveal { nonce: [9; 32] },
            WireMessage::FileStreamOpen {
                transfer: 7,
                lease: 123,
            },
            WireMessage::FileOfferRevoke { transfer: 7 },
            WireMessage::FileStreamOpened {
                lease: 123,
                reason: String::new(),
            },
            WireMessage::FileStreamRead {
                lease: 123,
                request: 456,
                entry: 2,
                offset: 1 << 40,
                length: 262144,
            },
            WireMessage::FileStreamData {
                request: 456,
                data: vec![1, 2, 3],
                reason: String::new(),
            },
            WireMessage::FileStreamClose {
                lease: 123,
                from_source: true,
                reason: "cancelled".into(),
            },
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

    fn key_event(kind: u32, code: u32, modifiers: u32) -> LcInputEvent {
        LcInputEvent {
            kind,
            code,
            modifiers,
            ..LcInputEvent::default()
        }
    }

    #[test]
    fn keys_keep_their_position_by_default() {
        // Command stays the GUI key (the Windows key on a PC), as in Deskflow.
        assert_eq!(CoreConfig::default().modifier_map, ModifierMap::KEEP);
        let mut command = key_event(
            LC_INPUT_KEY_DOWN,
            0xE3,
            LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND,
        );
        let sent = command;
        assert!(ModifierMap::KEEP.apply(&mut command));
        assert_eq!(command, sent);
    }

    #[test]
    fn the_modifier_map_applies_only_between_conventions() {
        let swap = ModifierMap::SWAP;
        assert_eq!(swap.between("macos", "windows"), swap);
        assert_eq!(swap.between("linux", "macos"), swap);
        assert_eq!(swap.between("macos", "macos"), ModifierMap::KEEP);
        assert_eq!(swap.between("windows", "linux"), ModifierMap::KEEP);
        assert_eq!(swap.between("unknown", "windows"), ModifierMap::KEEP);
    }

    #[test]
    fn swapping_makes_command_control_and_control_the_windows_key() {
        let swap = ModifierMap::SWAP;
        // ⌘C copies on a PC.
        let mut command_c = key_event(
            LC_INPUT_KEY_DOWN,
            0x06,
            LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND,
        );
        assert!(swap.apply(&mut command_c));
        assert_eq!(command_c.code, 0x06);
        assert_eq!(
            command_c.modifiers,
            LC_MODIFIER_CONTROL | LC_MODIFIER_LEFT_CONTROL
        );
        // Sides are kept: right Command is right Control.
        let mut right_command = key_event(LC_INPUT_KEY_DOWN, 0xE7, 0);
        assert!(swap.apply(&mut right_command));
        assert_eq!(right_command.code, 0xE4);
        // The Mac's Control is the Windows key, Shift stays with it.
        let mut control = key_event(
            LC_INPUT_KEY_UP,
            0xE0,
            LC_MODIFIER_CONTROL | LC_MODIFIER_LEFT_CONTROL | LC_MODIFIER_SHIFT,
        );
        assert!(swap.apply(&mut control));
        assert_eq!(control.code, 0xE3);
        assert_eq!(
            control.modifiers,
            LC_MODIFIER_COMMAND | LC_MODIFIER_LEFT_COMMAND | LC_MODIFIER_SHIFT
        );
        // Both held: both come out, swapped.
        let mut both = key_event(
            LC_INPUT_KEY_DOWN,
            0x04,
            LC_MODIFIER_COMMAND | LC_MODIFIER_CONTROL,
        );
        assert!(swap.apply(&mut both));
        assert_eq!(both.modifiers, LC_MODIFIER_COMMAND | LC_MODIFIER_CONTROL);
    }

    #[test]
    fn each_modifier_can_play_as_any_or_none() {
        let map = ModifierMap {
            super_key: Modifier::Control,
            control: Modifier::None,
            alt: Modifier::Shift,
        };
        // Mapped to nothing: the key is not played.
        let mut control = key_event(LC_INPUT_KEY_DOWN, 0xE4, LC_MODIFIER_CONTROL);
        assert!(!map.apply(&mut control));
        // Its modifier bits go too; Option plays as Shift, on its side.
        let mut option_a = key_event(
            LC_INPUT_KEY_DOWN,
            0x04,
            LC_MODIFIER_CONTROL
                | crate::model::LC_MODIFIER_OPTION
                | crate::model::LC_MODIFIER_RIGHT_OPTION,
        );
        assert!(map.apply(&mut option_a));
        assert_eq!(
            option_a.modifiers,
            crate::model::LC_MODIFIER_SHIFT | crate::model::LC_MODIFIER_RIGHT_SHIFT
        );
        let mut right_option = key_event(LC_INPUT_KEY_UP, 0xE6, 0);
        assert!(map.apply(&mut right_option));
        assert_eq!(right_option.code, 0xE5);
        // Other keys and mouse input are untouched.
        let mut letter = key_event(LC_INPUT_KEY_DOWN, 0x04, 0);
        assert!(map.apply(&mut letter));
        assert_eq!(letter.code, 0x04);
    }

    #[test]
    fn modifier_maps_read_from_json() {
        let map: ModifierMap =
            serde_json::from_str(r#"{"super":"control","control":"super","alt":"alt"}"#).unwrap();
        assert_eq!(map, ModifierMap::SWAP);
        // Missing keys keep their position.
        let map: ModifierMap = serde_json::from_str(r#"{"control":"none"}"#).unwrap();
        assert_eq!(
            map,
            ModifierMap {
                control: Modifier::None,
                ..ModifierMap::KEEP
            }
        );
        assert!(serde_json::from_str::<ModifierMap>(r#"{"super":"hyper"}"#).is_err());
        let config: CoreConfig =
            serde_json::from_str(r#"{"modifier_map":{"super":"control","control":"super"}}"#)
                .unwrap();
        assert_eq!(config.modifier_map, ModifierMap::SWAP);
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
        // The queue stamps each event with when it came; that aside, they
        // are as they were pushed.
        let mut drained = queue.drain();
        for entry in &mut drained {
            if let PlatformEntry::Event(event) = entry {
                assert_ne!(event.timestamp_micros, 0);
                event.timestamp_micros = 0;
            }
        }
        assert_eq!(
            drained,
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
                modifier_map: ModifierMap::SWAP,
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

        // Swapped, Mac Command arrives on Windows as Control.
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

    /// On a link that keeps up, a mouse that reports 1000 times a second is
    /// sent as it reports: nothing is held back (motion is paced only on a
    /// link in trouble, see `MotionPacing`), and none of it is lost. The
    /// driving device hears how long its input takes there and back.
    #[test]
    fn fast_motion_goes_out_as_it_comes_and_all_of_it_arrives() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("pace-a", discovery_port);
        let mut b = test_core("pace-b", discovery_port);
        wait_for_peers(&a, &["pace-b"]);
        a.thread.send(Command::Connect("pace-b".into())).unwrap();
        wait_connected(&a, "pace-b");
        wait_connected(&b, "pace-a");

        const REPORTS: u32 = 1000;
        let mut arrivals: Vec<(Instant, f64)> = Vec::new();
        let start = Instant::now();
        for report in 0..REPORTS {
            a.thread.send(Command::SendInput(motion(1.0))).unwrap();
            // One report a millisecond, as exactly as a test can.
            while start.elapsed() < Duration::from_millis(u64::from(report) + 1) {
                while let Ok(event) = b.input.try_recv() {
                    arrivals.push((Instant::now(), event.delta_x));
                }
                std::hint::spin_loop();
            }
        }
        while let Ok(event) = b.input.recv_timeout(Duration::from_millis(200)) {
            arrivals.push((Instant::now(), event.delta_x));
        }

        let total: f64 = arrivals.iter().map(|(_, delta)| delta).sum();
        assert_eq!(total, f64::from(REPORTS), "motion was lost or made up");
        let longest_gap = arrivals
            .windows(2)
            .map(|pair| pair[1].0 - pair[0].0)
            .max()
            .unwrap_or_default();
        eprintln!(
            "{REPORTS} reports in 1 s arrived as {} moves, longest gap {longest_gap:?}",
            arrivals.len()
        );
        // Not paced: nearly every report is its own move (a few merge when
        // two reports meet in the queue).
        assert!(
            arrivals.len() > 600,
            "{} moves for {REPORTS} reports: held back on a good link",
            arrivals.len()
        );
        assert!(longest_gap < Duration::from_millis(40), "{longest_gap:?}");

        // B echoed the stamps of what it received: A knows the round trip.
        let latency = wait_for_event(&a.events, Duration::from_secs(3), |event| {
            event["event"] == "input_latency" && event["peer_id"] == "pace-b"
        });
        eprintln!("input round trip: {latency}");
        assert!(latency["samples"].as_u64().unwrap() >= 1);
        assert!(latency["worst_ms"].as_f64().unwrap() < 50.0, "{latency}");
        assert!(events_named(&a, "link_quality").is_empty());

        // A button pressed right after motion does not wait for a slot.
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        let pressed = Instant::now();
        a.thread
            .send(Command::SendInput(LcInputEvent {
                kind: LC_INPUT_MOUSE_BUTTON_DOWN,
                code: 1,
                buttons: 1,
                ..LcInputEvent::default()
            }))
            .unwrap();
        loop {
            let event = b
                .input
                .recv_timeout(Duration::from_millis(200))
                .expect("the button arrives");
            if event.kind == LC_INPUT_MOUSE_BUTTON_DOWN {
                break;
            }
        }
        eprintln!("a button after motion arrived in {:?}", pressed.elapsed());
        assert!(pressed.elapsed() < Duration::from_millis(50));

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    /// Motion is paced while a link is in trouble and for a while after;
    /// trouble that returns soon after pacing stopped makes it last longer.
    #[test]
    fn motion_is_paced_while_the_link_is_in_trouble_and_longer_if_it_returns() {
        use super::input::MotionPacing;
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut pacing = MotionPacing::default();
        assert!(!pacing.observe(false, at(0)));

        // Trouble: paced for 30 s after its last sign.
        assert!(pacing.observe(true, at(1)));
        assert!(pacing.observe(true, at(5)));
        assert!(pacing.observe(false, at(34)));
        assert!(!pacing.observe(false, at(36)));

        // Back within a minute of stopping: unpaced motion was the cause,
        // so twice as long now, and twice again the next time.
        assert!(pacing.observe(true, at(40)));
        assert!(pacing.observe(false, at(99)));
        assert!(!pacing.observe(false, at(101)));
        assert!(pacing.observe(true, at(110)));
        assert!(pacing.observe(false, at(229)));
        assert!(!pacing.observe(false, at(231)));

        // Trouble long after is a new matter: 30 s again.
        assert!(pacing.observe(true, at(2_000)));
        assert!(!pacing.observe(false, at(2_031)));

        // Never longer than ten minutes, however often it returns.
        let mut now = 3_000;
        for _ in 0..10 {
            assert!(pacing.observe(true, at(now)));
            while pacing.observe(false, at(now + 1)) {
                now += 1;
            }
            now += 2;
        }
        assert!(pacing.observe(true, at(now)));
        assert!(pacing.observe(false, at(now + 599)));
        assert!(!pacing.observe(false, at(now + 601)));
    }

    #[test]
    fn echoes_give_the_round_trip_of_input() {
        use super::input::RoundTrips;
        let mut round_trips = RoundTrips::default();
        assert_eq!(round_trips.take(), None);
        round_trips.echoed(1_000, 3_000);
        round_trips.echoed(2_000, 10_000);
        // Stamps of some other clock: from the future, or absurdly old.
        round_trips.echoed(20_000, 10_000);
        round_trips.echoed(1, 60_000_000);
        assert_eq!(round_trips.take(), Some((2, 5_000, 8_000)));
        assert_eq!(round_trips.take(), None);

        let echo = WireMessage::InputEcho { stamp: 123_456 };
        let bytes = encode_message(&echo).expect("encode echo");
        assert_eq!(decode_message(&bytes).expect("decode echo"), echo);
    }

    /// Input whose arrival is further from its stamp than the best seen was
    /// held up on the way by that much: every event of a burst after a
    /// stall, not only the first. Pauses are not delays.
    #[test]
    fn input_timing_tells_how_long_input_was_held_up() {
        use super::input::InputTiming;
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // The peer's clock is somewhere else entirely.
        let stamp = |sent_ms: u64| 7_000_000_000 + sent_ms * 1_000;
        let mut timing = InputTiming::default();
        assert_eq!(timing.take(), None);

        // Straight through, 2 ms on the way.
        assert_eq!(timing.held_up(stamp(0), at(2)), Some(0));
        assert_eq!(timing.held_up(stamp(8), at(10)), Some(0));
        // A stall: sent at 16, 24 and 32, all arriving at 70.
        assert_eq!(timing.held_up(stamp(16), at(70)), Some(52_000));
        assert_eq!(timing.held_up(stamp(24), at(70)), Some(44_000));
        assert_eq!(timing.held_up(stamp(32), at(70)), Some(36_000));
        // A pause, then straight through again; a faster trip than any
        // before is the new best.
        assert_eq!(timing.held_up(stamp(5_000), at(5_002)), Some(0));
        assert_eq!(timing.held_up(stamp(5_010), at(5_011)), Some(0));
        assert_eq!(timing.held_up(stamp(5_020), at(5_022)), Some(1_000));
        // No stamp: not known.
        assert_eq!(timing.held_up(0, at(5_030)), None);

        // The clocks drift apart: what counted as the best trip a minute
        // ago no longer does.
        assert_eq!(timing.held_up(stamp(40_000), at(40_004)), Some(3_000));
        assert_eq!(timing.held_up(stamp(80_000), at(80_004)), Some(0));
        // After an hour of silence the clocks are 90 ms further apart:
        // nothing from before says how long this input took.
        assert_eq!(timing.held_up(stamp(3_680_000), at(3_680_094)), Some(0));
        assert_eq!(timing.held_up(stamp(3_680_033), at(3_680_127)), Some(0));

        // Batches: late from 20 ms, and a key repeat is stale from 100 ms.
        timing.batch(Some(0));
        timing.batch(Some(52_000));
        timing.batch(None);
        assert!(!timing.stale_repeat(Some(99_000)));
        assert!(!timing.stale_repeat(None));
        assert!(timing.stale_repeat(Some(100_000)));
        let report = timing.take().expect("input arrived");
        assert_eq!(
            (
                report.batches,
                report.late,
                report.worst_micros,
                report.stale_repeats
            ),
            (3, 1, 52_000, 1)
        );
        assert_eq!(timing.take(), None);
    }

    /// A held key repeats; repeats that were held up on the way are not
    /// played all at once when they finally arrive, but the key still goes
    /// down and comes up.
    #[test]
    fn key_repeats_that_arrive_too_late_are_not_played() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let a = test_core("repeat-a", discovery_port);
        // B, whose shell blocks the core for a while when told to (a motion
        // of 99): whatever A sends meanwhile reaches B's core late, as
        // after a stall of the network.
        let (events_tx, events) = mpsc::channel();
        let (input_tx, input) = mpsc::channel();
        let mut b = CoreThread::spawn(
            CoreConfig {
                device_id: "repeat-b".to_owned(),
                control_port: free_udp_port(),
                discovery_port,
                ..CoreConfig::default()
            },
            EventSink::new(
                move |event| {
                    let _ = events_tx.send(event);
                },
                move |event: LcInputEvent| {
                    if event.kind == LC_INPUT_MOUSE_MOVE && event.delta_x == 99.0 {
                        std::thread::sleep(Duration::from_millis(450));
                    }
                    let _ = input_tx.send(event);
                },
            ),
        )
        .expect("start core");
        let mut a = a;
        wait_for_peers(&a, &["repeat-b"]);
        a.thread.send(Command::Connect("repeat-b".into())).unwrap();
        wait_connected(&a, "repeat-b");
        wait_for_event(&events, Duration::from_secs(6), |event| {
            event["event"] == "connected"
        });

        const BACKSPACE: u32 = 0x2A;
        let key = |kind: u32, repeat: bool| LcInputEvent {
            kind,
            code: BACKSPACE,
            flags: if repeat { LC_INPUT_FLAG_KEY_REPEAT } else { 0 },
            repeat_count: u32::from(repeat),
            ..LcInputEvent::default()
        };
        let send = |event: LcInputEvent| a.thread.send(Command::SendInput(event)).unwrap();
        let repeats_played = |wait: Duration| {
            let deadline = Instant::now() + wait;
            let (mut repeats, mut released) = (0, false);
            while let Ok(event) =
                input.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                repeats += u32::from(event.flags & LC_INPUT_FLAG_KEY_REPEAT != 0);
                released |= event.kind == LC_INPUT_KEY_UP;
            }
            (repeats, released)
        };

        // The key goes down and repeats, straight through: all played.
        send(key(LC_INPUT_KEY_DOWN, false));
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(33));
            send(key(LC_INPUT_KEY_DOWN, true));
        }
        assert_eq!(repeats_played(Duration::from_millis(200)), (5, false));

        // B stalls; eight more repeats are sent meanwhile, then the key
        // comes up after the stall.
        send(motion(99.0));
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..8 {
            send(key(LC_INPUT_KEY_DOWN, true));
            std::thread::sleep(Duration::from_millis(33));
        }
        std::thread::sleep(Duration::from_millis(300));
        send(key(LC_INPUT_KEY_UP, false));
        let (repeats, released) = repeats_played(Duration::from_millis(600));
        eprintln!("of 8 repeats sent during a stall, {repeats} were played");
        // All of them are at least 100 ms late; one may slip through.
        assert!(repeats <= 1, "{repeats} stale repeats were played");
        assert!(released, "the key did not come up");

        let quality = wait_for_event(&events, Duration::from_secs(3), |event| {
            event["event"] == "link_quality" && event["stale_repeats"].as_u64().unwrap_or(0) > 0
        });
        eprintln!("{quality}");

        a.thread.stop_and_join();
        b.stop_and_join();
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
        // C learns that A disconnected on purpose, not that something failed.
        wait_for_event(&c.events, Duration::from_secs(3), |event| {
            event["event"] == "disconnected"
                && event["peer_id"] == "multi-a"
                && event["reason"] == crate::transport::PEER_DISCONNECTED
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
    fn a_stranger_gets_one_stream_and_nothing_more_to_fill() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut core = test_core("streams", discovery_port);
        let runtime = tokio::runtime::Runtime::new().expect("stranger runtime");
        runtime.block_on(async {
            let identity = crate::secure::Identity::load_or_create(None, "stranger").unwrap();
            let endpoints = crate::quic::QuicEndpoints::bind(0, &identity).unwrap();
            let connection = endpoints
                .control
                .connect(
                    ([127, 0, 0, 1], core.port).into(),
                    crate::secure::SERVER_NAME,
                )
                .unwrap()
                .await
                .expect("connect to the core");
            let _stream = connection.open_bi().await.expect("the one stream");
            // QUIC would buffer whatever is sent on more streams until read,
            // and nothing reads them: the core does not let them open.
            let wait = Duration::from_millis(300);
            assert!(
                tokio::time::timeout(wait, connection.open_bi())
                    .await
                    .is_err(),
                "a second stream opened"
            );
            assert!(
                tokio::time::timeout(wait, connection.open_uni())
                    .await
                    .is_err(),
                "a unidirectional stream opened"
            );
            assert_eq!(connection.max_datagram_size(), None, "datagrams accepted");
            connection.close(0u32.into(), b"");
            drop(endpoints);
        });
        core.thread.stop_and_join();
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
    fn a_device_that_already_trusts_its_peer_still_shows_the_code() {
        let _network = network_test();
        let (directory, a_fingerprint, _) =
            paired_identities("one-sided", "one-sided-a", "one-sided-b");
        let discovery_port = free_udp_port();
        // B remembers A; A forgot B (its user unpaired it there).
        let mut a = test_core_with("one-sided-a", discovery_port, {
            let identity_path = directory.join("one-sided-a").to_string_lossy().into_owned();
            move |config: &mut CoreConfig| {
                config.require_pairing = true;
                config.identity_path = Some(identity_path);
            }
        });
        let mut b = test_core_with("one-sided-b", discovery_port, {
            let trusting = paired_with(&directory, "one-sided-b", "one-sided-a", &a_fingerprint);
            move |config: &mut CoreConfig| {
                trusting(config);
                config.require_pairing = true;
            }
        });
        wait_for_peers(&a, &["one-sided-b"]);
        a.thread
            .send(Command::Connect("one-sided-b".into()))
            .unwrap();

        // A's user can only check its code against B's screen, so B shows
        // it too instead of accepting on its own.
        let request = |event: &Value| event["event"] == "pairing_requested";
        let on_a = wait_for_event(&a.events, Duration::from_secs(6), request);
        let on_b = wait_for_event(&b.events, Duration::from_secs(6), request);
        assert_eq!(on_a["code"], on_b["code"]);
        a.thread
            .send(Command::AcceptPairing("one-sided-b".into()))
            .unwrap();
        assert!(events_named(&a, "paired").is_empty());
        b.thread
            .send(Command::AcceptPairing("one-sided-a".into()))
            .unwrap();
        wait_for_event(&a.events, Duration::from_secs(3), |event| {
            event["event"] == "paired"
        });
        let connected = wait_connected_event(&a, "one-sided-b");
        assert_eq!(connected["paired"], true);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_older_device_cannot_pair_but_nothing_else_breaks() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core_with("outdated-a", discovery_port, |config| {
            config.require_pairing = true;
        });
        // An older build: it pairs with a code a device in the middle can
        // forge.
        let mut b = test_core_with("outdated-b", discovery_port, |config| {
            config.capabilities = LOCAL_CAPABILITIES & !CAPABILITY_PAIRING_V2;
        });
        wait_for_peers(&a, &["outdated-b"]);
        a.thread
            .send(Command::Connect("outdated-b".into()))
            .unwrap();
        let ended = wait_for_event(&a.events, Duration::from_secs(6), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "outdated-b"
        });
        assert_eq!(ended["reason"], runtime_reason::PAIRING_OUTDATED);
        assert!(events_named(&a, "pairing_requested").is_empty());
        assert!(events_named(&a, "connected").is_empty());

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
        // Both show the code before B's user declines.
        for core in [&a, &b] {
            wait_for_event(&core.events, Duration::from_secs(6), |event| {
                event["event"] == "pairing_requested"
            });
        }
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
        let directory = std::env::temp_dir().join(format!("rinflow-trust-{}", std::process::id()));
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
            "rinflow-test-{name}-{}-{}",
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
        file_cores_with(name, limit, false)
    }

    fn file_cores_with(
        name: &str,
        limit: u64,
        on_demand: bool,
    ) -> (TestCore, TestCore, PathBuf, PathBuf) {
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
            config.on_demand_files = on_demand;
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

    fn stream_lease(core: &TestCore, peer: &str, transfer: u32) -> crate::RemoteFileLease {
        let lease = crate::file_stream::next_id();
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        core.thread
            .send(Command::FileStream(
                crate::file_stream::StreamCommand::Open {
                    peer: peer.into(),
                    transfer,
                    lease,
                    reply,
                },
            ))
            .unwrap();
        result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        crate::RemoteFileLease {
            id: lease,
            commands: core.thread.sender(),
        }
    }

    #[test]
    fn on_demand_files_wait_for_reads_and_survive_clipboard_replacement() {
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores_with("stream", u64::MAX, true);
        let content = pattern(700_003, 9);
        let file = source.join("video.bin");
        std::fs::write(&file, &content).unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(&[file])))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        let offered = wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "files_available"
        });
        assert!(events_named(&b, "files_receiving").is_empty());
        assert!(events_named(&a, "files_sending").is_empty());
        assert!(walk(&downloads).is_empty());
        let lease = stream_lease(
            &b,
            "stream-a",
            offered["manifest"]["transfer"].as_u64().unwrap() as u32,
        );
        assert_eq!(
            lease.read(0, 0, usize::MAX).unwrap(),
            content[..crate::MAX_READ_SIZE]
        );
        // Both sides copy something else; the already pinned operation
        // still supplies bytes, and finishing must not write the clipboard.
        a.thread
            .send(Command::SetClipboard(text_clipboard("new source copy")))
            .unwrap();
        b.thread
            .send(Command::SetClipboard(text_clipboard("new local copy")))
            .unwrap();
        assert_eq!(
            lease.read(0, 400_000, 12345).unwrap(),
            content[400_000..412_345]
        );
        assert_eq!(lease.read(0, 700_000, 100).unwrap(), content[700_000..]);
        assert!(lease.read(0, content.len() as u64, 1).unwrap().is_empty());
        assert!(walk(&downloads).is_empty());
        lease.finish("delivered");
        wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "files_stream_ended"
        });
        assert!(lease.read(0, 0, 1).is_err());
        assert!(events_named(&b, "files_received").is_empty());
        assert!(events_named(&b, "clipboard").iter().all(|e| {
            e["formats"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["mime"] != MIME_URI_LIST)
        }));
        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(downloads);
    }

    #[test]
    fn on_demand_reads_support_large_offsets_and_detect_source_changes() {
        use std::io::{Seek, SeekFrom, Write};
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores_with("stream-large", 0, true);
        let path = source.join("large.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        // Sparse allocation: this test never transfers or allocates 4 GiB.
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let mut returned = 0;
            unsafe extern "system" {
                fn DeviceIoControl(
                    h: *mut std::ffi::c_void,
                    code: u32,
                    input: *const u8,
                    input_len: u32,
                    output: *mut u8,
                    output_len: u32,
                    returned: *mut u32,
                    overlapped: *mut std::ffi::c_void,
                ) -> i32;
            }
            assert_ne!(
                unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        0x000900c4,
                        std::ptr::null(),
                        0,
                        std::ptr::null_mut(),
                        0,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
        }
        let offset = (1u64 << 32) + 27;
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"large-offset-data").unwrap();
        drop(file);
        a.thread
            .send(Command::SetClipboard(uri_clipboard(std::slice::from_ref(
                &path,
            ))))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        let offered = wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "files_available"
        });
        let lease = stream_lease(
            &b,
            "stream-large-a",
            offered["manifest"]["transfer"].as_u64().unwrap() as u32,
        );
        assert_eq!(lease.read(0, offset, 17).unwrap(), b"large-offset-data");
        assert!(lease.read(0, offset + 18, 1).is_err());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1)
            .unwrap();
        assert!(lease.read(0, 0, 1).unwrap_err().contains("changed"));
        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(downloads);
    }

    #[test]
    fn file_stream_wire_limits_reject_oversized_reads_and_responses() {
        for length in [0, crate::MAX_READ_SIZE as u32 + 1] {
            assert!(
                encode_message(&WireMessage::FileStreamRead {
                    lease: 1,
                    request: 1,
                    entry: 0,
                    offset: 0,
                    length
                })
                .is_err()
            );
        }
        assert!(
            encode_message(&WireMessage::FileStreamData {
                request: 1,
                data: vec![0; crate::MAX_READ_SIZE + 1],
                reason: String::new()
            })
            .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_commit_does_not_overwrite_a_target_created_during_transfer() {
        let root = temp_dir("commit-race");
        let target = root.join("file.bin");
        let mut writer = transfer_files::FileWriter::create(target.clone(), 3, 1).unwrap();
        std::fs::write(&target, b"keep this").unwrap();
        assert!(writer.write(0, b"new").is_err());
        drop(writer);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep this");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn folders_past_the_limits_ask_to_zip_and_the_rest_still_travels() {
        let _network = network_test();
        let (mut a, mut b, source, _downloads) = file_cores_with("too-deep", 0, true);
        // Folders inside folders, one level deeper than allowed.
        let mut deep = source.join("deep");
        for level in 0..=transfer_files::MAX_DEPTH {
            deep = deep.join(level.to_string());
        }
        std::fs::create_dir_all(&deep).unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(&[source.join("deep")])))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        // The copying side hears why, with the limit, to say it its own way.
        let refused = wait_for_event(&a.events, Duration::from_secs(5), |e| {
            e["event"] == "files_not_shared"
        });
        assert_eq!(refused["reason"], "too_deep");
        assert_eq!(refused["limit"], transfer_files::MAX_DEPTH);
        // The rest of the clipboard (the names as text) still travels.
        let clipboard = wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "clipboard"
        });
        assert_eq!(clipboard["formats"][0]["mime"], MIME_TEXT);
        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
    }

    #[test]
    fn more_files_than_the_limit_are_not_offered() {
        let root = temp_dir("too-many");
        let folder = root.join("many");
        std::fs::create_dir_all(&folder).unwrap();
        for i in 0..=transfer_files::MAX_ENTRIES {
            std::fs::File::create(folder.join(i.to_string())).unwrap();
        }
        let stop = std::sync::atomic::AtomicBool::new(false);
        let offer = transfer_files::Offer::build_cancellable(1, &[folder], &stop);
        assert_eq!(offer.err().as_deref(), Some(transfer_files::TOO_MANY_FILES));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn on_demand_source_cancellation_and_disabled_sharing_revoke_leases() {
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores_with("stream-cancel", 0, true);
        let path = source.join("file.bin");
        std::fs::write(&path, b"content").unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(&[path])))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        let offered = wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "files_available"
        });
        let transfer = offered["manifest"]["transfer"].as_u64().unwrap() as u32;
        let lease = stream_lease(&b, "stream-cancel-a", transfer);
        let sending = wait_for_event(&a.events, Duration::from_secs(5), |e| {
            e["event"] == "files_sending"
        });
        a.thread
            .send(Command::CancelFiles(
                sending["transfer"].as_u64().unwrap() as u32
            ))
            .unwrap();
        let ended = wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "files_stream_ended"
        });
        assert_eq!(ended["reason"], "cancelled");
        assert!(lease.read(0, 0, 7).is_err());
        let lease = stream_lease(&b, "stream-cancel-a", transfer);
        b.thread
            .send(Command::FileStream(
                crate::file_stream::StreamCommand::Revoke,
            ))
            .unwrap();
        wait_for_event(&b.events, Duration::from_secs(5), |e| {
            e["event"] == "files_stream_ended"
        });
        assert!(lease.read(0, 0, 7).is_err());
        assert!(!downloads.join("file.bin").exists());
        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(downloads);
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

        // Both sides can show what goes on.
        let receiving = wait_for_event(&b.events, Duration::from_secs(10), |event| {
            event["event"] == "files_receiving"
        });
        assert_eq!(receiving["peer_id"], "files-a");
        assert_eq!(receiving["name"], "报告");
        assert_eq!(receiving["items"], 2);
        assert_eq!(receiving["count"], 4);
        assert_eq!(receiving["total_size"], 400_006);
        let done = wait_for_event(&b.events, Duration::from_secs(10), |event| {
            event["event"] == "files_received" || event["event"] == "files_failed"
        });
        assert_eq!(done["event"], "files_received", "{done}");
        let sent = wait_for_event(&a.events, Duration::from_secs(2), |event| {
            event["event"] == "files_send_ended"
        });
        assert_eq!(sent["peer_id"], "files-b");
        assert_eq!(sent["reason"], "done");
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
    fn zero_auto_receive_limit_requires_acceptance_even_for_empty_files() {
        let _network = network_test();
        let (mut a, mut b, source, downloads) = file_cores("empty-offer", 0);
        let file = source.join("empty.txt");
        let folder = source.join("empty-folder");
        std::fs::write(&file, []).unwrap();
        std::fs::create_dir(&folder).unwrap();
        a.thread
            .send(Command::SetClipboard(uri_clipboard(&[file, folder])))
            .unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        let offered = wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_offered" || event["event"] == "files_received"
        });
        assert_eq!(offered["event"], "files_offered");
        assert_eq!(offered["total_size"], 0);
        assert!(!downloads.join("empty.txt").exists());
        assert!(!downloads.join("empty-folder").exists());
        b.thread
            .send(Command::AcceptFiles(
                offered["transfer"].as_u64().unwrap() as u32
            ))
            .unwrap();
        wait_for_event(&b.events, Duration::from_secs(5), |event| {
            event["event"] == "files_received"
        });
        assert_eq!(
            std::fs::read(downloads.join("empty.txt")).unwrap(),
            Vec::<u8>::new()
        );
        assert!(downloads.join("empty-folder").is_dir());
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

    /// Identity files for two devices that paired before, in a fresh
    /// directory: (directory, fingerprint of `a`, fingerprint of `b`).
    fn paired_identities(test: &str, a: &str, b: &str) -> (PathBuf, String, String) {
        let directory = std::env::temp_dir().join(format!("rinflow-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let fingerprint = |id: &str| {
            crate::secure::Identity::load_or_create(Some(&directory.join(id)), id)
                .unwrap()
                .fingerprint
        };
        let (a, b) = (fingerprint(a), fingerprint(b));
        (directory, a, b)
    }

    /// The config of `own`, which paired with `other` (as the shell passes
    /// back what `paired` reported).
    fn paired_with(
        directory: &Path,
        own: &str,
        other: &str,
        fingerprint: &str,
    ) -> impl FnOnce(&mut CoreConfig) + use<> {
        let identity_path = directory.join(own).to_string_lossy().into_owned();
        let peer = TrustedPeer {
            peer_id: other.to_owned(),
            fingerprint: fingerprint.to_owned(),
        };
        move |config: &mut CoreConfig| {
            config.identity_path = Some(identity_path);
            config.trusted_peers = vec![peer];
        }
    }

    #[test]
    fn paired_devices_connect_by_themselves_and_come_back_after_a_restart() {
        let _network = network_test();
        let (directory, a_fingerprint, b_fingerprint) =
            paired_identities("auto", "auto-a", "auto-b");
        let discovery_port = free_udp_port();
        let mut a = test_core_with(
            "auto-a",
            discovery_port,
            paired_with(&directory, "auto-a", "auto-b", &b_fingerprint),
        );
        let mut b = test_core_with(
            "auto-b",
            discovery_port,
            paired_with(&directory, "auto-b", "auto-a", &a_fingerprint),
        );

        // Nobody asks: they find each other and connect, quietly.
        let connected = wait_connected_event(&a, "auto-b");
        assert_eq!(connected["paired"], true);
        wait_connected(&b, "auto-a");
        assert!(events_named(&a, "connecting").is_empty());

        // B quits and starts again: they connect again by themselves.
        b.thread.stop_and_join();
        wait_for_event(&a.events, Duration::from_secs(4), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "auto-b"
        });
        let mut b = test_core_with(
            "auto-b",
            discovery_port,
            paired_with(&directory, "auto-b", "auto-a", &a_fingerprint),
        );
        wait_for_event(&a.events, Duration::from_secs(12), |event| {
            event["event"] == "connected" && event["peer"]["id"] == "auto-b"
        });
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![1.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_disconnect_on_either_side_holds_until_someone_connects_again() {
        let _network = network_test();
        let (directory, a_fingerprint, b_fingerprint) =
            paired_identities("hold", "hold-a", "hold-b");
        let discovery_port = free_udp_port();
        let mut a = test_core_with(
            "hold-a",
            discovery_port,
            paired_with(&directory, "hold-a", "hold-b", &b_fingerprint),
        );
        let mut b = test_core_with(
            "hold-b",
            discovery_port,
            paired_with(&directory, "hold-b", "hold-a", &a_fingerprint),
        );
        wait_connected(&a, "hold-b");
        wait_connected(&b, "hold-a");

        // A's user disconnects: B is told why, and neither reconnects.
        a.thread
            .send(Command::DisconnectPeer("hold-b".into()))
            .unwrap();
        let ended = wait_for_event(&b.events, Duration::from_secs(3), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "hold-a"
        });
        assert_eq!(ended["reason"], crate::transport::PEER_DISCONNECTED);
        std::thread::sleep(Duration::from_secs(5));
        assert!(events_named(&a, "connected").is_empty());
        assert!(events_named(&b, "connected").is_empty());

        // B's user connects: that is on purpose.
        b.thread.send(Command::Connect("hold-a".into())).unwrap();
        wait_connected(&a, "hold-b");

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_device_going_to_sleep_ends_its_sessions_in_order_and_is_back_on_waking() {
        let _network = network_test();
        let (directory, a_fingerprint, b_fingerprint) =
            paired_identities("sleep", "sleep-a", "sleep-b");
        let discovery_port = free_udp_port();
        let mut a = test_core_with(
            "sleep-a",
            discovery_port,
            paired_with(&directory, "sleep-a", "sleep-b", &b_fingerprint),
        );
        let mut b = test_core_with(
            "sleep-b",
            discovery_port,
            paired_with(&directory, "sleep-b", "sleep-a", &a_fingerprint),
        );
        wait_connected(&a, "sleep-b");
        wait_connected(&b, "sleep-a");

        // B goes to sleep while driving A: A gets an ordinary close at once
        // (no heartbeat timeout) and control back.
        b.thread.send(Command::SendInput(motion(1.0))).unwrap();
        wait_for_event(&a.events, Duration::from_secs(2), |event| {
            event["event"] == "remote_control_started"
        });
        b.thread.send(Command::WillSleep).unwrap();
        wait_for_event(&a.events, Duration::from_secs(2), |event| {
            event["event"] == "remote_control_ended" && event["peer_id"] == "sleep-b"
        });
        let ended = wait_for_event(&a.events, Duration::from_secs(2), |event| {
            event["event"] == "disconnected" && event["peer_id"] == "sleep-b"
        });
        assert_eq!(ended["reason"], crate::transport::PEER_CLOSED);

        // A keeps trying; B, about to sleep, takes nothing.
        std::thread::sleep(Duration::from_secs(4));
        assert!(events_named(&a, "connected").is_empty());
        assert!(events_named(&b, "connected").is_empty());

        // B wakes: they are connected again within moments.
        b.thread.send(Command::NetworkChanged).unwrap();
        wait_for_event(&a.events, Duration::from_secs(6), |event| {
            event["event"] == "connected" && event["peer"]["id"] == "sleep-b"
        });

        a.thread.stop_and_join();
        b.thread.stop_and_join();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_network_change_keeps_sessions_that_still_answer() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("change-a", discovery_port);
        let mut b = test_core("change-b", discovery_port);
        wait_for_peers(&a, &["change-b"]);
        a.thread.send(Command::Connect("change-b".into())).unwrap();
        wait_connected(&a, "change-b");

        // Probed, and answering: nothing ends.
        a.thread.send(Command::NetworkChanged).unwrap();
        b.thread.send(Command::NetworkChanged).unwrap();
        std::thread::sleep(Duration::from_secs(4));
        assert!(events_named(&a, "disconnected").is_empty());
        assert!(events_named(&b, "disconnected").is_empty());
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![2.0]);

        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn placed_peers_are_kept_connected_unless_only_paired_ones_may_connect() {
        let _network = network_test();
        let placed = |peer_id: &str| {
            vec![PlacementRecord {
                peer_id: peer_id.to_owned(),
                offset: Some(Offset { x: 1440.0, y: 0.0 }),
                updated_ms: 1,
            }]
        };
        let discovery_port = free_udp_port();
        let mut a = test_core_with("placed-a", discovery_port, |config| {
            config.placements = placed("placed-b");
        });
        let mut b = test_core("placed-b", discovery_port);
        wait_connected(&a, "placed-b");
        a.thread.stop_and_join();
        b.thread.stop_and_join();

        // Not paired and only paired devices may connect: connecting would
        // ask the user to pair out of the blue, so it is left to the user.
        let discovery_port = free_udp_port();
        let mut a = test_core_with("strict-placed-a", discovery_port, |config| {
            config.placements = placed("strict-placed-b");
            config.require_pairing = true;
        });
        let mut b = test_core("strict-placed-b", discovery_port);
        wait_for_peers(&a, &["strict-placed-b"]);
        std::thread::sleep(Duration::from_secs(3));
        assert!(events_named(&a, "pairing_requested").is_empty());
        assert!(events_named(&a, "connected").is_empty());
        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn devices_find_each_other_by_address_where_multicast_does_not_reach() {
        let _network = network_test();
        // Different discovery ports: their multicast never meets.
        let (port_a, port_b) = (free_udp_port(), free_udp_port());
        let address = format!("127.0.0.1:{port_b}");
        let mut b = test_core("addr-b", port_b);
        let manual = address.clone();
        let mut a = test_core_with("addr-a", port_a, move |config| {
            config.manual_addresses = vec![manual, "not an address".to_owned()];
        });
        // Only A was given B's address; both find each other.
        wait_for_peers(&a, &["addr-b"]);
        wait_for_peers(&b, &["addr-a"]);
        let status = wait_for_event(&a.events, Duration::from_secs(6), |event| {
            event["event"] == "manual_addresses" && event["addresses"][0]["status"] == "found"
        });
        assert_eq!(status["addresses"][0]["address"], address);
        assert_eq!(status["addresses"][0]["peer_id"], "addr-b");
        assert_eq!(status["addresses"][1]["status"], "unresolved");
        a.thread.send(Command::Connect("addr-b".into())).unwrap();
        wait_connected(&a, "addr-b");
        wait_connected(&b, "addr-a");
        // No longer looked for.
        a.thread
            .send(Command::SetManualAddresses(Vec::new()))
            .unwrap();
        wait_for_event(&a.events, Duration::from_secs(2), |event| {
            event["event"] == "manual_addresses" && event["addresses"] == serde_json::json!([])
        });
        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn peers_hear_when_this_device_is_used_and_when_it_is_locked() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("presence-a", discovery_port);
        let mut b = test_core("presence-b", discovery_port);
        wait_for_peers(&a, &["presence-b"]);
        a.thread
            .send(Command::Connect("presence-b".into()))
            .unwrap();
        wait_connected(&a, "presence-b");
        wait_connected(&b, "presence-a");
        // Reported as often as the shell likes; sent once in a while.
        a.thread.send(Command::ReportUserActivity).unwrap();
        a.thread.send(Command::ReportUserActivity).unwrap();
        wait_for_event(&b.events, Duration::from_secs(2), |event| {
            event["event"] == "peer_active" && event["peer_id"] == "presence-a"
        });
        a.thread.send(Command::LockPeers).unwrap();
        wait_for_event(&b.events, Duration::from_secs(2), |event| {
            assert_ne!(event["event"], "peer_active", "activity sent twice");
            event["event"] == "lock_requested" && event["peer_id"] == "presence-a"
        });
        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn a_driven_device_whose_pointer_is_held_tells_its_controller() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut a = test_core("lock-a", discovery_port);
        let mut b = test_core("lock-b", discovery_port);
        wait_for_peers(&a, &["lock-b"]);
        a.thread.send(Command::Connect("lock-b".into())).unwrap();
        wait_connected(&a, "lock-b");
        wait_connected(&b, "lock-a");
        let lock_event = |locked: bool| {
            wait_for_event(&a.events, Duration::from_secs(2), move |event| {
                event["event"] == "peer_pointer_lock"
                    && event["peer_id"] == "lock-b"
                    && event["locked"] == locked
            })
        };
        // Nobody drives B yet: nobody to tell.
        b.thread.send(Command::SetPointerLocked(true)).unwrap();
        a.thread.send(Command::SendInput(motion(1.0))).unwrap();
        assert_eq!(received(&b, Duration::from_millis(400)), vec![1.0]);
        // Told as soon as A starts driving it, and of every change.
        lock_event(true);
        b.thread.send(Command::SetPointerLocked(false)).unwrap();
        lock_event(false);
        b.thread.send(Command::SetPointerLocked(true)).unwrap();
        lock_event(true);
        // Control ends and starts again: told again.
        a.thread.send(Command::ResetInput).unwrap();
        wait_for_event(&b.events, Duration::from_secs(2), |event| {
            event["event"] == "remote_control_ended"
        });
        a.thread.send(Command::SendInput(motion(2.0))).unwrap();
        lock_event(true);
        a.thread.stop_and_join();
        b.thread.stop_and_join();
    }

    #[test]
    fn the_modifier_map_can_be_changed_while_connected() {
        let _network = network_test();
        let discovery_port = free_udp_port();
        let mut mac = test_core_with("remap-mac", discovery_port, |config| {
            config.platform = "macos".into();
        });
        let mut pc = test_core_with("remap-pc", discovery_port, |config| {
            config.platform = "windows".into();
        });
        wait_for_peers(&pc, &["remap-mac"]);
        wait_for_peers(&mac, &["remap-pc"]);
        mac.thread
            .send(Command::Connect("remap-pc".into()))
            .unwrap();
        wait_connected(&mac, "remap-pc");
        wait_connected(&pc, "remap-mac");
        let key = |kind: u32| LcInputEvent {
            kind,
            code: 0xE3, // left Command
            ..LcInputEvent::default()
        };
        let control = |kind: u32| LcInputEvent {
            kind,
            code: 0xE0, // left Control
            ..LcInputEvent::default()
        };
        let next_key = |core: &TestCore| loop {
            let event = core
                .input
                .recv_timeout(Duration::from_secs(2))
                .expect("a key arrives");
            if event.kind == LC_INPUT_KEY_DOWN || event.kind == LC_INPUT_KEY_UP {
                return event.code;
            }
        };
        // Kept by default: Command stays the Windows key.
        mac.thread
            .send(Command::SendInput(key(LC_INPUT_KEY_DOWN)))
            .unwrap();
        assert_eq!(next_key(&pc), 0xE3);
        // Swapped while Command is held: it is released, then swapped.
        pc.thread
            .send(Command::SetModifierMap(ModifierMap::SWAP))
            .unwrap();
        assert_eq!(next_key(&pc), 0xE3);
        std::thread::sleep(Duration::from_millis(100));
        mac.thread
            .send(Command::SendInput(key(LC_INPUT_KEY_UP)))
            .unwrap();
        mac.thread
            .send(Command::SendInput(key(LC_INPUT_KEY_DOWN)))
            .unwrap();
        let codes = [next_key(&pc), next_key(&pc)];
        assert_eq!(codes, [0xE0, 0xE0], "Command plays as Control now");
        // And the Mac's Control is the Windows key.
        mac.thread
            .send(Command::SendInput(control(LC_INPUT_KEY_DOWN)))
            .unwrap();
        assert_eq!(next_key(&pc), 0xE3, "Control plays as the Windows key");
        mac.thread.stop_and_join();
        pc.thread.stop_and_join();
    }

    fn free_udp_port() -> u16 {
        UdpSocket::bind(("127.0.0.1", 0))
            .expect("bind temporary UDP port")
            .local_addr()
            .expect("temporary UDP address")
            .port()
    }
}
