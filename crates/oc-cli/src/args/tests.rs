use super::*;

#[test]
fn parses_help_version_and_status() {
    for args in [
        &[][..],
        &["help"],
        &["--help"],
        &["-h"],
        &["camera", "--help"],
        &["camera", "-h"],
    ] {
        assert!(matches!(parse(args).unwrap(), Command::Help));
    }
    for flag in ["--version", "-V"] {
        assert!(matches!(parse(&[flag]).unwrap(), Command::Version));
    }
    assert!(matches!(parse(&["status"]).unwrap(), Command::Status));
    for command in ["oc open <machine> <url-or-path>", "oc mount", "oc shelf"] {
        assert!(HELP.contains(command));
    }
}

#[test]
fn parses_open_with_exactly_one_target_argument() {
    for target in [
        "https://example.com/page?q=one&two=three",
        "file.txt",
        "/home/michael/Client's draft.txt",
        "relative path/$(literal);file.txt",
    ] {
        let Command::Open {
            machine,
            target: parsed,
        } = parse(&["open", "desk", target]).unwrap()
        else {
            panic!("expected open");
        };
        assert_eq!(machine, "desk");
        assert_eq!(parsed, target);
    }
}

#[test]
fn rejects_open_with_missing_empty_or_extra_arguments() {
    for args in [
        vec!["open"],
        vec!["open", "desk"],
        vec!["open", "", "file.txt"],
        vec!["open", "desk", ""],
        vec!["open", "desk", "two", "files.txt"],
        vec!["open", "desk", "file.txt", "extra"],
        vec!["open", "desk", "file.txt", "--unknown"],
        vec!["open", "--unknown", "file.txt"],
        vec!["open", "desk", "--unknown"],
    ] {
        assert!(parse(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn parses_mount_and_shelf_without_arguments() {
    assert!(matches!(parse(&["mount"]).unwrap(), Command::Mount));
    assert!(matches!(parse(&["shelf"]).unwrap(), Command::Shelf));
}

#[test]
fn rejects_mount_and_shelf_with_any_extra_arguments() {
    for command in ["mount", "shelf"] {
        for extra in ["extra", "start", "--daemon", "--unknown", "-x", ""] {
            assert!(parse(&[command, extra]).is_err());
        }
        assert!(parse(&[command, "one", "two"]).is_err());
    }
}

#[test]
fn parses_camera_defaults() {
    let Command::Camera(options) = parse(&["camera"]).unwrap() else {
        panic!("expected camera");
    };
    assert!(!options.self_test);
    assert_eq!(options.device, PathBuf::from("/dev/video42"));
    assert_eq!(options.state_dir, PathBuf::from("var/camera"));
    assert!(options.ffmpeg_log.is_none());
    assert!(options.png.is_none());
}

#[test]
fn parses_camera_self_test_and_all_path_overrides() {
    let Command::Camera(options) = parse(&[
        "camera",
        "--self-test",
        "--device",
        "/dev/video43",
        "--state-dir",
        "var/other",
        "--ffmpeg-log",
        "var/camera.log",
        "--png",
        "var/frame.png",
    ])
    .unwrap() else {
        panic!("expected camera");
    };
    assert!(options.self_test);
    assert_eq!(options.device, PathBuf::from("/dev/video43"));
    assert_eq!(options.state_dir, PathBuf::from("var/other"));
    assert_eq!(options.ffmpeg_log, Some(PathBuf::from("var/camera.log")));
    assert_eq!(options.png, Some(PathBuf::from("var/frame.png")));
}

#[test]
fn parses_clipboard_peers_and_stop() {
    for peers in [&["user@host"][..], &["user@host", "other@peer"]] {
        let mut args = vec!["clipboard", "start"];
        args.extend(peers);
        let Command::ClipboardStart(parsed) = parse(&args).unwrap() else {
            panic!("expected clipboard start");
        };
        assert_eq!(parsed, peers);
    }
    assert!(matches!(
        parse(&["clipboard", "stop"]).unwrap(),
        Command::ClipboardStop
    ));
}

#[test]
fn parses_internal_clipboard_entrypoints_without_extra_arguments() {
    assert!(matches!(
        parse(&["clipboard", "bridge"]).unwrap(),
        Command::ClipboardBridge
    ));
    assert!(matches!(
        parse(&["clipboard", "own"]).unwrap(),
        Command::ClipboardOwn
    ));
    for command in ["bridge", "own"] {
        for extra in ["extra", "--unknown", ""] {
            assert!(parse(&["clipboard", command, extra]).is_err());
        }
        assert!(!HELP.contains(&format!("oc clipboard {command}")));
    }
}

#[test]
fn parses_drop_commands_and_preserves_names_and_paths() {
    assert!(matches!(
        parse(&["drop", "peers"]).unwrap(),
        Command::DropPeers
    ));
    for paths in [&["./one.txt"][..], &["./one.txt", "images/two files.png"]] {
        let mut args = vec!["drop", "send", "Michael's Mac"];
        args.extend(paths);
        let Command::DropSend {
            name,
            paths: parsed,
        } = parse(&args).unwrap()
        else {
            panic!("expected drop send");
        };
        assert_eq!(name, "Michael's Mac");
        assert_eq!(parsed, paths.iter().map(PathBuf::from).collect::<Vec<_>>());
    }
    let Command::DropReceive(directory) = parse(&["drop", "receive", "./received files"]).unwrap()
    else {
        panic!("expected drop receive");
    };
    assert_eq!(directory, PathBuf::from("./received files"));
}

#[test]
fn parses_airplay_defaults_and_overrides() {
    let Command::AirplayServe(default) = parse(&["airplay", "serve"]).unwrap() else {
        panic!("expected airplay serve");
    };
    assert!(!default.name.is_empty());
    assert_eq!(default.port, 0);
    assert_eq!(default.bind_address, "0.0.0.0".parse::<IpAddr>().unwrap());
    for ip in ["100.74.233.121", "::1"] {
        let Command::AirplayServe(config) = parse(&[
            "airplay", "serve", "--port", "7000", "--name", "Desk", "--bind", ip,
        ])
        .unwrap() else {
            panic!("expected airplay serve");
        };
        assert_eq!(config.name, "Desk");
        assert_eq!(config.port, 7000);
        assert_eq!(config.bind_address, ip.parse::<IpAddr>().unwrap());
    }
    for port in ["0", "65535"] {
        let Command::AirplayServe(config) = parse(&["airplay", "serve", "--port", port]).unwrap()
        else {
            panic!("expected airplay serve");
        };
        assert_eq!(config.port, port.parse::<u16>().unwrap());
    }
}

#[test]
fn parses_display_wifi_ips_and_usb_udid() {
    for ip in ["100.74.233.121", "fd7a:115c:a1e0::1"] {
        let Command::DisplayExtend(oc_display::Target::Wifi(address)) =
            parse(&["display", "extend", "wifi", ip]).unwrap()
        else {
            panic!("expected display wifi");
        };
        assert_eq!(address, ip.parse::<IpAddr>().unwrap());
    }
    let Command::DisplayExtend(oc_display::Target::Usb { udid }) =
        parse(&["display", "extend", "usb", "device-id"]).unwrap()
    else {
        panic!("expected display usb");
    };
    assert_eq!(udid, "device-id");
}

#[test]
fn parses_mirror_list_and_device_selection() {
    assert!(matches!(
        parse(&["mirror", "list"]).unwrap(),
        Command::MirrorList
    ));
    assert!(matches!(
        parse(&["mirror", "start"]).unwrap(),
        Command::MirrorStart(None)
    ));
    let Command::MirrorStart(Some(udid)) =
        parse(&["mirror", "start", "--udid", "device-id"]).unwrap()
    else {
        panic!("expected selected mirror device");
    };
    assert_eq!(udid, "device-id");
}

#[test]
fn parses_every_input_edge_and_socket_family_in_any_option_order() {
    for (name, edge) in [
        ("left", oc_input::Edge::Left),
        ("right", oc_input::Edge::Right),
        ("top", oc_input::Edge::Top),
        ("bottom", oc_input::Edge::Bottom),
    ] {
        for (peer, local) in [
            ("100.74.233.121:9000", "100.74.233.122:9000"),
            ("[fd7a:115c:a1e0::1]:9000", "[fd7a:115c:a1e0::2]:9000"),
        ] {
            let Command::InputServe(config) = parse(&[
                "input", "serve", "--local", local, "--edge", name, "--peer", peer,
            ])
            .unwrap() else {
                panic!("expected input serve");
            };
            assert_eq!(config.edge, edge);
            assert_eq!(config.peer, peer.parse::<SocketAddr>().unwrap());
            assert_eq!(config.listen, local.parse::<SocketAddr>().unwrap());
            assert!(config.input_devices.is_empty());
            assert!(config.monitor.is_none());
        }
    }
}

#[test]
fn rejects_missing_commands_operands_and_required_options() {
    for args in [
        vec!["clipboard"],
        vec!["clipboard", "start"],
        vec!["drop"],
        vec!["drop", "send"],
        vec!["drop", "send", "Mac"],
        vec!["drop", "receive"],
        vec!["airplay"],
        vec!["display"],
        vec!["display", "extend"],
        vec!["display", "extend", "wifi"],
        vec!["display", "extend", "usb"],
        vec!["mirror"],
        vec!["mirror", "start", "--udid"],
        vec!["input"],
        vec!["input", "serve"],
        vec!["input", "serve", "--edge", "left"],
        vec![
            "input",
            "serve",
            "--peer",
            "100.74.233.121:9000",
            "--local",
            "100.74.233.122:9000",
        ],
        vec![
            "input",
            "serve",
            "--edge",
            "left",
            "--peer",
            "100.74.233.121:9000",
        ],
        vec![
            "input",
            "serve",
            "--edge",
            "left",
            "--local",
            "100.74.233.122:9000",
        ],
        vec!["clipboard", "start", ""],
        vec!["drop", "send", "Mac", ""],
        vec!["drop", "receive", ""],
        vec!["display", "extend", "usb", ""],
        vec!["mirror", "start", "--udid", ""],
    ] {
        assert!(parse(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn rejects_every_missing_flag_value_and_flags_used_as_values() {
    for (prefix, flags) in [
        (
            &["camera"][..],
            &["--device", "--state-dir", "--ffmpeg-log", "--png"][..],
        ),
        (
            &["airplay", "serve"][..],
            &["--name", "--bind", "--port"][..],
        ),
        (&["mirror", "start"][..], &["--udid"][..]),
        (
            &["input", "serve"][..],
            &["--edge", "--peer", "--local"][..],
        ),
    ] {
        for flag in flags {
            let mut args = prefix.to_vec();
            args.push(flag);
            assert!(parse(&args).is_err(), "accepted {args:?}");
            for next in ["", "--unknown", "-x"] {
                let mut args = args.clone();
                args.push(next);
                assert!(parse(&args).is_err(), "accepted {args:?}");
            }
        }
    }
}

#[test]
fn rejects_unknown_flags_and_extra_arguments_for_every_command() {
    for args in [
        vec!["help"],
        vec!["--version"],
        vec!["status"],
        vec!["camera"],
        vec!["clipboard", "start", "user@host"],
        vec!["clipboard", "stop"],
        vec!["drop", "peers"],
        vec!["drop", "send", "Mac", "one.txt"],
        vec!["drop", "receive", "received"],
        vec!["airplay", "serve"],
        vec!["display", "extend", "wifi", "100.74.233.121"],
        vec!["display", "extend", "usb", "device-id"],
        vec!["mirror", "list"],
        vec!["mirror", "start"],
        vec![
            "input",
            "serve",
            "--edge",
            "left",
            "--peer",
            "100.74.233.121:9000",
            "--local",
            "100.74.233.122:9000",
        ],
    ] {
        for unknown in ["--unknown", "-x"] {
            let mut args = args.clone();
            args.push(unknown);
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }
    for args in [
        vec!["unknown"],
        vec!["help", "extra"],
        vec!["status", "extra"],
        vec!["camera", "extra"],
        vec!["clipboard", "stop", "extra"],
        vec!["drop", "peers", "extra"],
        vec!["drop", "receive", "received", "extra"],
        vec!["airplay", "serve", "extra"],
        vec!["display", "extend", "wifi", "100.74.233.121", "extra"],
        vec!["mirror", "list", "extra"],
        vec!["mirror", "start", "extra"],
        vec!["camera", "--unknown", "--help"],
        vec!["camera", "--bind", "0.0.0.0:8443"],
    ] {
        assert!(parse(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn rejects_invalid_ips_ports_edges_and_socket_addresses() {
    for ip in ["localhost", "999.1.1.1", "100.74.233.121:7000", "[::1]", ""] {
        assert!(parse(&["airplay", "serve", "--bind", ip]).is_err());
        assert!(parse(&["display", "extend", "wifi", ip]).is_err());
    }
    for port in ["65536", "-1", "seven", ""] {
        assert!(parse(&["airplay", "serve", "--port", port]).is_err());
    }
    for edge in ["Left", "diagonal", ""] {
        assert!(
            parse(&[
                "input",
                "serve",
                "--edge",
                edge,
                "--peer",
                "100.74.233.121:9000",
                "--local",
                "100.74.233.122:9000",
            ])
            .is_err()
        );
    }
    for address in [
        "localhost:9000",
        "100.74.233.121",
        "::1:9000",
        "100.74.233.121:65536",
        "",
    ] {
        for flag in ["--peer", "--local"] {
            assert!(
                parse(&[
                    "input",
                    "serve",
                    "--edge",
                    "left",
                    "--peer",
                    "100.74.233.121:9000",
                    "--local",
                    "100.74.233.122:9000",
                    flag,
                    address,
                ])
                .is_err()
            );
        }
    }
}
