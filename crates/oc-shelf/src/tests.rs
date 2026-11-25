use super::*;

fn model(self_device: bool, online: bool, open_target: &str) -> Model {
    Model {
        devices: vec![DeviceRow {
            hostname: "studio".into(),
            os: "Linux".into(),
            ip: "100.64.0.1".into(),
            online,
            self_device,
        }],
        selected: 0,
        open_target: open_target.into(),
    }
}

#[test]
fn no_selection_disables_every_command() {
    let mut model = model(false, true, "https://example.com");
    for selected in [1, usize::MAX] {
        model.selected = selected;
        assert!(model.selected().is_none());
        for (_, command) in model.buttons() {
            assert!(!model.enabled(&command));
            assert_eq!(model.activate(command), None);
        }
    }
    model.devices.clear();
    model.selected = 0;
    assert!(model.selected().is_none());
    for (_, command) in model.buttons() {
        assert!(!model.enabled(&command));
        assert_eq!(model.activate(command), None);
    }
}

#[test]
fn this_machine_only_allows_open_and_local_actions() {
    for online in [false, true] {
        for target in ["", "https://example.com"] {
            let model = model(true, online, target);
            for (_, command) in model.buttons() {
                let expected = match command {
                    Command::Open(_) => !target.is_empty(),
                    Command::Drop | Command::AirPlay | Command::Mirror => true,
                    _ => false,
                };
                assert_eq!(model.enabled(&command), expected, "{command:?}");
                assert_eq!(model.activate(command).is_some(), expected);
            }
        }
    }
}

#[test]
fn online_peer_allows_every_command_with_a_target() {
    for target in ["", "https://example.com"] {
        let model = model(false, true, target);
        for (_, command) in model.buttons() {
            let expected = !matches!(command, Command::Open(_)) || !target.is_empty();
            assert_eq!(model.enabled(&command), expected, "{command:?}");
            assert_eq!(model.activate(command).is_some(), expected);
        }
    }
}

#[test]
fn offline_peer_disables_every_command_even_with_a_target() {
    for target in ["", "https://example.com"] {
        let model = model(false, false, target);
        for (_, command) in model.buttons() {
            assert!(!model.enabled(&command), "{command:?}");
            assert_eq!(model.activate(command), None);
        }
    }
}

#[derive(Default)]
struct RecordedActions(Vec<(DeviceRow, Command)>);

impl Actions for RecordedActions {
    fn perform(&mut self, device: &DeviceRow, command: Command) {
        self.0.push((device.clone(), command));
    }
}

#[test]
fn activation_sends_each_ui_command_to_the_selected_device() {
    let mut model = model(true, true, "https://example.com/path?q=shelf");
    let mut peer = model.devices[0].clone();
    peer.hostname = "laptop".into();
    peer.ip = "100.64.0.2".into();
    peer.self_device = false;
    model.devices.push(peer.clone());
    model.selected = 1;
    assert_eq!(model.selected(), Some(&peer));

    let mut actions = RecordedActions::default();
    let expected = vec![
        Command::Open("https://example.com/path?q=shelf".into()),
        Command::Mount,
        Command::Clipboard,
        Command::Drop,
        Command::AirPlay,
        Command::Display,
        Command::Mirror,
        Command::Input,
    ];
    for (_, command) in model.buttons() {
        if let Some((device, command)) = model.activate(command) {
            actions.perform(&device, command);
        }
    }
    assert_eq!(
        actions.0,
        expected
            .into_iter()
            .map(|command| (peer.clone(), command))
            .collect::<Vec<_>>()
    );
}

#[test]
fn activation_rechecks_changed_device_and_target_state() {
    let mut model = model(false, true, "https://example.com");
    let open = model.buttons()[0].1.clone();
    model.open_target.clear();
    assert_eq!(model.activate(open), None);
    model.devices[0].online = false;
    assert_eq!(model.activate(Command::Mount), None);
    model.devices.clear();
    assert_eq!(model.activate(Command::Drop), None);
}

#[test]
fn shelf_draws_without_a_display_or_dispatching_actions() {
    for devices in [
        vec![],
        model(true, true, "").devices,
        model(false, false, "").devices,
    ] {
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let mut actions = RecordedActions::default();
        let mut shelf = Shelf {
            model: Model {
                devices,
                selected: 0,
                open_target: String::new(),
            },
            actions: &mut actions,
            activity: Activity::default(),
            phones: video::Phones::default(),
            video: video::Video::default(),
        };
        let output = draw(&mut shelf, &ctx, vec![]);
        assert!(!output.shapes.is_empty());
        assert_eq!(output.viewport_output.len(), 1);
        assert!(actions.0.is_empty());
    }
}

fn draw(shelf: &mut Shelf<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
    ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(640.0, 360.0),
            )),
            events,
            ..Default::default()
        },
        |ctx| shelf.show(ctx),
    )
}

fn click(shelf: &mut Shelf<'_>, ctx: &egui::Context, label: &str) {
    draw(shelf, ctx, vec![]);
    let output = draw(shelf, ctx, vec![]);
    let position = output
        .shapes
        .iter()
        .find_map(|shape| {
            if let egui::epaint::Shape::Text(text) = &shape.shape
                && text.galley.text() == label
            {
                let rect = text.galley.rect.translate(text.pos.to_vec2());
                assert!(shape.clip_rect.contains_rect(rect), "{label} is clipped");
                return Some(rect.center());
            }
            None
        })
        .unwrap_or_else(|| panic!("missing label: {label}"));
    for pressed in [true, false] {
        draw(
            shelf,
            ctx,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
}

#[test]
fn clicking_each_enabled_button_dispatches_through_activation() {
    for (label, command) in model(false, true, "https://example.com").buttons() {
        let ctx = egui::Context::default();
        let mut actions = RecordedActions::default();
        let model = model(false, true, "https://example.com");
        let device = model.devices[0].clone();
        let mut shelf = Shelf {
            model,
            actions: &mut actions,
            activity: Activity::default(),
            phones: video::Phones::default(),
            video: video::Video::default(),
        };
        click(&mut shelf, &ctx, label);
        let recipient = if matches!(command, Command::Mirror) {
            MirrorDevice {
                udid: "selection".into(),
            }
            .row()
        } else {
            device
        };
        assert_eq!(actions.0, vec![(recipient, command)], "{label}");
    }
}

#[test]
fn clicking_disabled_buttons_never_dispatches() {
    for (self_device, online, target) in [
        (true, true, ""),
        (false, true, ""),
        (false, false, "https://example.com"),
    ] {
        let model = model(self_device, online, target);
        for (label, command) in model.buttons() {
            if model.enabled(&command) {
                continue;
            }
            let ctx = egui::Context::default();
            let mut actions = RecordedActions::default();
            let mut shelf = Shelf {
                model: model.clone(),
                actions: &mut actions,
                activity: Activity::default(),
                phones: video::Phones::default(),
                video: video::Video::default(),
            };
            click(&mut shelf, &ctx, label);
            assert!(actions.0.is_empty(), "{label}");
        }
    }
}

#[test]
fn clicking_a_device_row_changes_the_action_recipient() {
    let ctx = egui::Context::default();
    let mut actions = RecordedActions::default();
    let mut model = model(false, true, "https://example.com");
    let mut local = model.devices[0].clone();
    local.self_device = true;
    local.hostname = "desktop".into();
    model.devices.push(local.clone());
    let mut shelf = Shelf {
        model,
        actions: &mut actions,
        activity: Activity::default(),
        phones: video::Phones::default(),
        video: video::Video::default(),
    };
    click(&mut shelf, &ctx, "desktop · this machine");
    assert_eq!(shelf.model.selected, 1);
    click(&mut shelf, &ctx, "Drop");
    assert_eq!(actions.0, vec![(local, Command::Drop)]);
}

#[test]
fn peers_without_tailnet_ipv4_cannot_receive_network_actions() {
    let mut model = model(false, true, "file.txt");
    for ip in ["-", "192.168.1.2", "fd7a::2"] {
        model.devices[0].ip = ip.into();
        for command in [
            Command::Open("file.txt".into()),
            Command::Clipboard,
            Command::Display,
            Command::Input,
        ] {
            assert!(!model.enabled(&command));
        }
        assert!(model.enabled(&Command::Mirror));
    }
}

#[test]
fn activity_blocks_duplicates_releases_finished_actions_and_retains_runtime_errors() {
    let device = model(false, true, "").devices.remove(0);
    let mut activity = Activity::default();
    let mut update = Update {
        device,
        command: Command::AirPlay,
        state: ActionState::Pending,
        result: Ok("starting".into()),
    };
    activity.apply(update.clone());
    assert!(!activity.available(&Command::AirPlay));
    update.state = ActionState::Running;
    update.result = Err("connection failed".into());
    activity.apply(update.clone());
    assert!(!activity.available(&Command::AirPlay));
    update.result = Ok("listening".into());
    activity.apply(update.clone());
    assert!(activity.errors()[0].text().contains("connection failed"));
    update.state = ActionState::Finished;
    activity.apply(update);
    assert!(activity.available(&Command::AirPlay));
    assert_eq!(activity.updates().len(), 1);
}

#[derive(Default)]
struct FeedbackActions {
    calls: Vec<(DeviceRow, Command)>,
    updates: Vec<Update>,
}

impl Actions for FeedbackActions {
    fn perform(&mut self, device: &DeviceRow, command: Command) {
        self.calls.push((device.clone(), command));
    }
    fn updates(&mut self) -> Vec<Update> {
        std::mem::take(&mut self.updates)
    }
}

#[test]
fn worker_errors_are_visible_after_the_selected_device_changes() {
    let ctx = egui::Context::default();
    let mut model = model(false, true, "file.txt");
    let original = model.devices[0].clone();
    let mut other = original.clone();
    other.hostname = "other".into();
    other.ip = "100.64.0.2".into();
    model.devices.push(other);
    model.selected = 1;
    let mut actions = FeedbackActions {
        updates: vec![Update {
            device: original,
            command: Command::Open("file.txt".into()),
            state: ActionState::Finished,
            result: Err("handoff refused".into()),
        }],
        ..Default::default()
    };
    let mut shelf = Shelf {
        model,
        actions: &mut actions,
        activity: Activity::default(),
        phones: video::Phones::default(),
        video: video::Video::default(),
    };
    draw(&mut shelf, &ctx, vec![]);
    let output = draw(&mut shelf, &ctx, vec![]);
    let visible = output.shapes.iter().any(|shape| {
        if let egui::epaint::Shape::Text(text) = &shape.shape
            && text.galley.text().contains("handoff refused")
        {
            let rect = text.galley.rect.translate(text.pos.to_vec2());
            return shape.clip_rect.contains_rect(rect);
        }
        false
    });
    assert!(
        visible,
        "the actual feedback label must fit inside its viewport"
    );
    assert!(shelf.activity.available(&Command::Open("retry".into())));
    assert!(actions.calls.is_empty());
}

#[test]
fn running_service_feedback_prevents_a_duplicate_start() {
    let ctx = egui::Context::default();
    let model = model(false, true, "file.txt");
    let mut actions = FeedbackActions {
        updates: vec![Update {
            device: model.devices[0].clone(),
            command: Command::Input,
            state: ActionState::Running,
            result: Ok("sharing".into()),
        }],
        ..Default::default()
    };
    let mut shelf = Shelf {
        model,
        actions: &mut actions,
        activity: Activity::default(),
        phones: video::Phones::default(),
        video: video::Video::default(),
    };
    click(&mut shelf, &ctx, "Input");
    assert!(actions.calls.is_empty());
}

#[test]
fn duplicate_hostname_buttons_select_distinct_ip_rows() {
    let ctx = egui::Context::default();
    let mut model = model(false, true, "file.txt");
    let mut second = model.devices[0].clone();
    second.ip = "100.64.0.2".into();
    model.devices.push(second.clone());
    let mut actions = RecordedActions::default();
    let mut shelf = Shelf {
        model,
        actions: &mut actions,
        activity: Activity::default(),
        phones: video::Phones::default(),
        video: video::Video::default(),
    };
    click(&mut shelf, &ctx, "studio · 100.64.0.2");
    assert_eq!(shelf.model.selected, 1);
    click(&mut shelf, &ctx, "Open");
    assert_eq!(actions.0, [(second, Command::Open("file.txt".into()))]);
}
