use std::io::{Cursor, Read};

use openh264::{
    decoder::Decoder,
    formats::{RgbSliceU8, YUVBuffer, YUVSource},
};
use pipewire::spa::param::video::VideoFormat;
use serde_json::{Value, json};

use crate::{Configuration, capture::packed_rgb, input::Scroll, protocol};

fn hello() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/ipad-hello.json")).unwrap()
}

#[test]
fn cancelled_startup_closes_its_socket_and_joins_cleanup() {
    use std::{
        os::unix::net::UnixStream,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    let (sender, mut receiver) = UnixStream::pair().unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let cleaned = Arc::new(AtomicBool::new(false));
    let worker_cleaned = cleaned.clone();
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || {
        assert_eq!(receiver.read(&mut [0; 1]).unwrap(), 0);
        assert!(worker_stop.load(Ordering::Acquire));
        worker_cleaned.store(true, Ordering::Release);
        Ok(())
    });
    let startup = crate::Startup {
        pending: crate::Pending {
            stop,
            socket: Some(crate::transport::Connection::Usb(sender)),
            armed: true,
        },
        worker: Some(worker),
    };

    drop(startup);
    assert!(cleaned.load(Ordering::Acquire));
}

#[test]
fn hello_sizes_real_desktop_and_rotation() {
    let mut value = hello();
    let landscape = Configuration::from_hello(&value).unwrap();
    assert_eq!(
        (landscape.width, landscape.height, landscape.scale),
        (1920, 1438, 2.0)
    );
    value["panel"]["pixelsWide"] = json!(2048);
    value["panel"]["pixelsHigh"] = json!(2732);
    let portrait = Configuration::from_hello(&value).unwrap();
    assert_eq!((portrait.width, portrait.height), (1438, 1920));
    assert_eq!(landscape.frames_per_second, 30);
}

#[test]
fn malformed_panel_falls_back_as_a_whole_and_ignores_legacy_scale() {
    let mut value = hello();
    value["panel"]["pixelsWide"] = json!(1000);
    value["panel"]["pointsWide"] = json!(500);
    value["scale"] = json!(999);
    assert_eq!(
        Configuration::from_hello(&value).unwrap(),
        Configuration::from_hello(&hello()).unwrap()
    );
    value["panel"] = json!({"pixelsWide":800,"pixelsHigh":600,"scale":1.25});
    assert_eq!(Configuration::from_hello(&value).unwrap().scale, 1.0);
}

#[test]
fn decoder_envelopes_are_alternatives_and_legacy_limits_also_apply() {
    let mut value = hello();
    value["videoCaps"] = json!([
        {"codec":"hevc","maxWidth":8000},
        {"codec":"h264","maxWidth":1280,"maxHeight":720,"maxFrameRate":30},
        {"codec":"h264","maxWidth":640,"maxHeight":480,"maxFrameRate":60}
    ]);
    value["maxEncodeWide"] = json!(1000);
    value["maxEncodeHigh"] = json!(1000);
    let configuration = Configuration::from_hello(&value).unwrap();
    assert!(configuration.width <= 1000 && configuration.height <= 720);
    assert_eq!(configuration.width % 2, 0);
    assert_eq!(configuration.height % 2, 0);
    value["videoCaps"] = json!([{"codec":"hevc"}]);
    assert!(Configuration::from_hello(&value).is_err());
}

#[test]
fn pixel_budget_reduces_raster_without_exceeding_throughput() {
    let mut value = hello();
    value["videoCaps"] = json!([{"codec":"h264", "maxPixelsPerSecond":9_216_000}]);
    let configuration = Configuration::from_hello(&value).unwrap();
    assert_eq!(configuration.frames_per_second, 30);
    assert!(
        configuration.width as u64
            * configuration.height as u64
            * configuration.frames_per_second as u64
            <= 9_216_000
    );
}

#[test]
fn receiver_control_loop_echoes_clock_and_ignores_future_messages() {
    use crate::transport::Connection;
    use std::{
        os::unix::net::UnixStream,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };
    let (sender, mut receiver) = UnixStream::pair().unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let writer = Mutex::new(Connection::Usb(sender.try_clone().unwrap()));
    let stop = Arc::new(AtomicBool::new(false));
    let (events_tx, events_rx) = mpsc::sync_channel(8);
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || {
        crate::receive(Connection::Usb(sender), &writer, &worker_stop, events_tx)
    });
    protocol::write_frame(&mut receiver, b"not JSON").unwrap();
    protocol::write_control(&mut receiver, &json!({"type":"futureEvent"})).unwrap();
    protocol::write_control(&mut receiver, &json!({"type":"ping","t":1234.25})).unwrap();
    let pong = protocol::control(&protocol::read_frame(&mut receiver).unwrap()).unwrap();
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["t"], 1234.25);
    assert!(pong["mt"].as_u64().unwrap() > 0);
    protocol::write_control(&mut receiver, &json!({"type":"kf"})).unwrap();
    protocol::write_control(&mut receiver, &json!({"type":"closing"})).unwrap();
    worker.join().unwrap().unwrap();
    let events: Vec<_> = events_rx.try_iter().collect();
    assert_eq!(
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["kf", "closing"]
    );
    assert!(stop.load(Ordering::Acquire));
}

#[test]
fn malformed_control_before_hello_is_ignored() {
    use crate::transport::Connection;
    use std::{os::unix::net::UnixStream, thread};
    let (sender, mut receiver) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || crate::transport::hello(&mut Connection::Usb(sender)));
    protocol::write_frame(&mut receiver, b"{}").unwrap();
    protocol::write_control(&mut receiver, &hello()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), hello());
}

#[test]
fn negotiated_ipad_raster_encodes_and_decodes() {
    let configuration = Configuration::from_hello(&hello()).unwrap();
    let rgb = vec![100; configuration.width as usize * configuration.height as usize * 3];
    let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(
        &rgb,
        (configuration.width as usize, configuration.height as usize),
    ));
    let mut encoder = crate::encoder(configuration).unwrap();
    let bytes = protocol::annex_b(&encoder.encode(&yuv).unwrap().to_vec()).unwrap();
    let mut decoder = Decoder::new().unwrap();
    let frame = decoder.decode(&bytes).unwrap().unwrap();
    assert_eq!(
        frame.dimensions(),
        (configuration.width as usize, configuration.height as usize)
    );
}

#[test]
fn legacy_hello_and_unknown_fields_work_but_future_version_does_not() {
    let value = json!({"type":"hello","pixelsWide":800,"pixelsHigh":600,"scale":3,"extra":123});
    assert_eq!(Configuration::from_hello(&value).unwrap().width, 800);
    let mut value = value;
    value["pv"] = json!(4);
    assert!(Configuration::from_hello(&value).is_err());
    value["type"] = json!("ping");
    assert!(Configuration::from_hello(&value).is_err());
}

struct Fragmented(Cursor<Vec<u8>>);
impl Read for Fragmented {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let size = bytes.len().min(1);
        self.0.read(&mut bytes[..size])
    }
}

#[test]
fn framing_handles_fragmented_and_coalesced_messages() {
    let mut bytes = Vec::new();
    protocol::write_control(&mut bytes, &hello()).unwrap();
    protocol::write_control(&mut bytes, &json!({"type":"ping","t":123.5})).unwrap();
    let mut reader = Fragmented(Cursor::new(bytes));
    assert_eq!(
        protocol::control(&protocol::read_frame(&mut reader).unwrap()).unwrap(),
        hello()
    );
    assert_eq!(
        protocol::control(&protocol::read_frame(&mut reader).unwrap()).unwrap()["t"],
        123.5
    );
}

#[test]
fn invalid_lengths_truncation_and_malformed_controls() {
    for length in [0u32, 1 << 20, u32::MAX] {
        assert!(protocol::read_frame(&mut Cursor::new(length.to_be_bytes())).is_err());
    }
    assert!(protocol::read_frame(&mut Cursor::new([0, 0, 0, 5, b'{'])).is_err());
    assert!(protocol::control(b"not json").is_none());
    assert!(protocol::control(b"{}").is_none());
    assert!(protocol::control(br#"{"type":"futureInput","new":42}"#).is_some());
    assert!(
        protocol::write_control(&mut Vec::new(), &json!({"type":"x","x":"a".repeat(32768)}))
            .is_err()
    );
}

#[test]
fn capture_respects_padding_offsets_channel_order_and_bounds() {
    let bytes = [
        99, 99, 1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, 7, 8, 9, 255, 10, 11, 12, 255,
    ];
    let rgb = packed_rgb(&bytes, 2, 20, 12, 2, 2, VideoFormat::BGRx).unwrap();
    assert_eq!(rgb, [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    assert!(packed_rgb(&bytes, 2, 19, 12, 2, 2, VideoFormat::BGRx).is_err());
    assert!(packed_rgb(&bytes, 2, 20, -12, 2, 2, VideoFormat::BGRx).is_err());
    assert!(packed_rgb(&bytes, 3, 20, 12, 2, 2, VideoFormat::BGRx).is_err());
}

#[test]
fn scroll_keeps_fractional_movement_and_natural_sign() {
    let mut scroll = Scroll::default();
    let mut events = Vec::new();
    for _ in 0..8 {
        events.extend(scroll.events(0.0, 0.125, 2.0));
    }
    assert_eq!(events.iter().map(|e| e.value()).sum::<i32>(), 4);
    let events = scroll.events(30.0, 30.0, 2.0);
    assert!(
        events
            .iter()
            .any(|e| e.code() == evdev::RelativeAxisCode::REL_WHEEL.0 && e.value() == 1)
    );
    assert!(
        events
            .iter()
            .any(|e| e.code() == evdev::RelativeAxisCode::REL_HWHEEL.0 && e.value() == -1)
    );
}

fn nal_types(bytes: &[u8]) -> Vec<u8> {
    bytes
        .windows(5)
        .filter(|w| w[..4] == [0, 0, 0, 1])
        .map(|w| w[4] & 31)
        .collect()
}

#[test]
fn h264_encodes_decodes_and_recovery_idr_contains_parameter_sets() {
    let configuration = Configuration {
        width: 64,
        height: 48,
        frames_per_second: 30,
        scale: 2.0,
    };
    let rgb: Vec<u8> = (0..64 * 48)
        .flat_map(|i| [(i % 256) as u8, (i / 64 * 5) as u8, 90])
        .collect();
    let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (64, 48)));
    let mut encoder = crate::encoder(configuration).unwrap();
    for index in 0..4 {
        if index == 3 {
            encoder.force_intra_frame();
        }
        let bytes = protocol::annex_b(&encoder.encode(&yuv).unwrap().to_vec()).unwrap();
        if index == 0 || index == 3 {
            let types = nal_types(&bytes);
            assert!(
                types.contains(&7) && types.contains(&8) && types.contains(&5),
                "{types:?}"
            );
            let mut decoder = Decoder::new().unwrap();
            let decoded = decoder.decode(&bytes).unwrap().unwrap();
            assert_eq!(decoded.dimensions(), (64, 48));
        }
        assert!(
            bytes
                .windows(3)
                .enumerate()
                .filter(|(_, w)| *w == [0, 0, 1])
                .all(|(i, _)| i > 0 && bytes[i - 1] == 0)
        );
    }
}

#[test]
fn annex_b_expands_three_byte_start_codes_and_rejects_empty_nalus() {
    assert_eq!(
        protocol::annex_b(&[0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x68, 4]).unwrap(),
        [0, 0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x68, 4]
    );
    assert!(protocol::annex_b(&[0, 0, 0, 1]).is_err());
    assert!(protocol::annex_b(&[5, 6, 7]).is_err());
}

#[test]
#[ignore = "creates a temporary headless monitor in a live Hyprland session"]
fn live_headless_output_keeps_workspace_and_is_removed() {
    let before: Value =
        serde_json::from_str(&crate::hyprland::command(&["-j", "activeworkspace"]).unwrap())
            .unwrap();
    let mut output = crate::hyprland::Output::create(Configuration {
        width: 800,
        height: 600,
        frames_per_second: 30,
        scale: 2.0,
    })
    .unwrap();
    eprintln!(
        "verified output: {} at {}x{}",
        output.name, output.x, output.y
    );
    output.remove().unwrap();
    let monitors: Value =
        serde_json::from_str(&crate::hyprland::command(&["-j", "monitors", "all"]).unwrap())
            .unwrap();
    assert!(
        !monitors
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == output.name)
    );
    let after: Value =
        serde_json::from_str(&crate::hyprland::command(&["-j", "activeworkspace"]).unwrap())
            .unwrap();
    assert_eq!(
        (&before["id"], &before["monitor"]),
        (&after["id"], &after["monitor"])
    );
}
