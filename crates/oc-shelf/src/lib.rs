//! A compact device shelf with window-independent command dispatch.

use eframe::egui;

mod activity;
pub use activity::{ActionState, Activity, Update};
mod video;
pub use video::{Frames, MirrorDevice, MirrorFrame, run_mirror};

/// One row in the shelf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    pub hostname: String,
    pub os: String,
    pub ip: String,
    pub online: bool,
    pub self_device: bool,
}

impl DeviceRow {
    /// Read the row's Tailscale IPv4.
    /// Takes this row and returns its address, or none for missing or non-tailnet IPs.
    pub fn ipv4(&self) -> Option<std::net::Ipv4Addr> {
        self.ip.parse::<std::net::Ipv4Addr>().ok().filter(|ip| {
            let bytes = ip.octets();
            bytes[0] == 100 && (64..=127).contains(&bytes[1])
        })
    }
}

/// A shelf command for the selected device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Open(String),
    Mount,
    Clipboard,
    Drop,
    AirPlay,
    Display,
    Mirror,
    MirrorStart(String),
    Input,
}

impl Command {
    /// Name an action.
    /// Takes this command and returns the label used by buttons and status messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open(_) => "Open",
            Self::Mount => "Mount",
            Self::Clipboard => "Clipboard",
            Self::Drop => "Drop",
            Self::AirPlay => "AirPlay",
            Self::Display => "Display",
            Self::Mirror | Self::MirrorStart(_) => "Mirror",
            Self::Input => "Input",
        }
    }

    /// Identify a persistent service.
    /// Takes this command and returns whether success leaves an owned service running.
    pub fn is_service(&self) -> bool {
        matches!(
            self,
            Self::Clipboard | Self::AirPlay | Self::Display | Self::MirrorStart(_) | Self::Input
        )
    }

    /// Compare action kinds.
    /// Takes another command and returns whether both use the same action button.
    pub fn same_action(&self, other: &Self) -> bool {
        self.label() == other.label()
    }
}

/// Receives shelf commands. The window calls this. Tests record calls.
pub trait Actions {
    /// Perform a shelf command.
    ///
    /// Takes the selected device and command. Returns after handing off the action.
    fn perform(&mut self, device: &DeviceRow, command: Command);

    /// Collect action updates.
    /// Takes this receiver and returns pending progress, completion, and failure messages.
    fn updates(&mut self) -> Vec<Update> {
        Vec::new()
    }

    /// Collect a phone discovery snapshot.
    /// Takes this receiver and returns new local USB phone choices when available.
    fn mirror_devices(&mut self) -> Option<Vec<MirrorDevice>> {
        None
    }

    /// Receive the newest selected-phone frame.
    /// Takes this receiver and returns one RGB frame or a delivery error without blocking.
    fn frame(&mut self) -> Result<Option<MirrorFrame>, String> {
        Ok(None)
    }
}

/// Devices and the open-target field. Pure. No window.
#[derive(Clone, Debug)]
pub struct Model {
    pub devices: Vec<DeviceRow>,
    pub selected: usize,
    pub open_target: String,
}

impl Model {
    /// Get the selected device.
    ///
    /// Takes this model and returns its selected row, or `None` for an invalid index.
    pub fn selected(&self) -> Option<&DeviceRow> {
        self.devices.get(self.selected)
    }

    /// Check command availability.
    ///
    /// Takes a command and returns whether the selected device can receive it.
    pub fn enabled(&self, command: &Command) -> bool {
        let Some(device) = self.selected() else {
            return false;
        };
        if !device.self_device && !device.online {
            return false;
        }
        match command {
            Command::Open(_) => {
                !self.open_target.is_empty() && (device.self_device || device.ipv4().is_some())
            }
            Command::Drop | Command::AirPlay | Command::Mirror | Command::MirrorStart(_) => true,
            Command::Clipboard | Command::Display | Command::Input => {
                !device.self_device && device.ipv4().is_some()
            }
            _ => !device.self_device,
        }
    }

    /// Prepare an enabled command for dispatch.
    ///
    /// Takes a command and returns an owned device and command, or `None` if disabled.
    pub fn activate(&self, command: Command) -> Option<(DeviceRow, Command)> {
        if !self.enabled(&command) {
            return None;
        }
        self.selected().cloned().map(|device| (device, command))
    }

    fn buttons(&self) -> [(&'static str, Command); 8] {
        [
            ("Open", Command::Open(self.open_target.clone())),
            ("Mount", Command::Mount),
            ("Clipboard", Command::Clipboard),
            ("Drop", Command::Drop),
            ("AirPlay", Command::AirPlay),
            ("Display", Command::Display),
            ("Mirror", Command::Mirror),
            ("Input", Command::Input),
        ]
    }
}

/// Open the shelf window.
///
/// Takes the model and action receiver. Returns `Ok` when the window closes, or a
/// window initialization or event-loop error.
pub fn run(model: Model, actions: &mut dyn Actions) -> Result<(), String> {
    eframe::run_native(
        "OmniContinuity",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("OmniContinuity")
                .with_inner_size([640.0, 360.0])
                .with_min_inner_size([520.0, 340.0]),
            renderer: eframe::Renderer::Glow,
            run_and_return: true,
            ..Default::default()
        },
        Box::new(move |creation| {
            creation.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(Shelf {
                model,
                actions,
                activity: Activity::default(),
                phones: video::Phones::default(),
                video: video::Video::default(),
            }))
        }),
    )
    .map_err(|error| error.to_string())
}

struct Shelf<'a> {
    model: Model,
    actions: &'a mut dyn Actions,
    activity: Activity,
    phones: video::Phones,
    video: video::Video,
}

/// Draw retained action outcomes.
/// Takes the UI context and activity state; reserves a visible, scrollable feedback area.
fn show_activity(ctx: &egui::Context, activity: &Activity) {
    if !activity.updates().is_empty() {
        egui::TopBottomPanel::bottom("activity").show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .max_height(90.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for update in activity.updates() {
                        let text = update.text();
                        if update.result.is_err() {
                            ui.colored_label(ui.visuals().error_fg_color, text);
                        } else {
                            ui.label(text);
                        }
                    }
                    for error in activity.errors() {
                        if !activity.updates().contains(error) {
                            ui.colored_label(ui.visuals().error_fg_color, error.text());
                        }
                    }
                });
        });
    }
}

impl Shelf<'_> {
    fn show(&mut self, ctx: &egui::Context) {
        for update in self.actions.updates() {
            self.activity.apply(update);
        }
        if let Some(devices) = self.actions.mirror_devices() {
            self.phones.replace(devices);
        }
        self.video.update(ctx, self.actions);
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
        show_activity(ctx, &self.activity);
        egui::SidePanel::left("devices")
            .exact_width(210.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.heading("Devices");
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.model.devices.is_empty() {
                        ui.weak("No devices");
                    }
                    for (index, device) in self.model.devices.iter().enumerate() {
                        let duplicate = self
                            .model
                            .devices
                            .iter()
                            .filter(|row| row.hostname == device.hostname)
                            .count()
                            > 1;
                        let label = if device.self_device {
                            format!("{} · this machine", device.hostname)
                        } else if duplicate {
                            format!("{} · {}", device.hostname, device.ip)
                        } else {
                            device.hostname.clone()
                        };
                        if ui
                            .push_id(index, |ui| {
                                ui.add_sized(
                                    [ui.available_width(), 30.0],
                                    egui::Button::selectable(self.model.selected == index, label),
                                )
                                .clicked()
                            })
                            .inner
                        {
                            self.model.selected = index;
                        }
                    }
                });
            });

        let mut clicked = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(device) = self.model.selected() {
                    ui.heading(&device.hostname);
                    egui::Grid::new("device_details").show(ui, |ui| {
                        ui.weak("OS");
                        ui.label(&device.os);
                        ui.end_row();
                        ui.weak("IP");
                        ui.label(&device.ip);
                        ui.end_row();
                        ui.weak("Status");
                        ui.label(if device.online { "online" } else { "offline" });
                        ui.end_row();
                    });
                } else {
                    ui.heading("No device selected");
                }
                ui.separator();
                let label = ui.label("Open target");
                ui.add(
                    egui::TextEdit::singleline(&mut self.model.open_target)
                        .desired_width(f32::INFINITY),
                )
                .labelled_by(label.id);
                ui.add_space(6.0);
                egui::Grid::new("commands").num_columns(2).show(ui, |ui| {
                    let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                    for (index, (label, command)) in self.model.buttons().into_iter().enumerate() {
                        let button = egui::Button::new(label).min_size(egui::vec2(width, 28.0));
                        if ui
                            .add_enabled(
                                self.model.enabled(&command) && self.activity.available(&command),
                                button,
                            )
                            .clicked()
                        {
                            clicked = Some(command);
                        }
                        if index % 2 == 1 {
                            ui.end_row();
                        }
                    }
                });
                if self.phones.discovered() {
                    ui.separator();
                    if let Some(command) = self
                        .phones
                        .show(ui, self.activity.available(&Command::Mirror))
                    {
                        clicked = Some(command);
                    }
                    self.video.show(ui, 240.0);
                }
            });
        });
        if let Some((mut device, command)) =
            clicked.and_then(|command| self.model.activate(command))
        {
            if matches!(command, Command::Mirror | Command::MirrorStart(_)) {
                let udid = match &command {
                    Command::MirrorStart(udid) => udid.clone(),
                    _ => "selection".into(),
                };
                device = MirrorDevice { udid }.row();
            }
            self.activity.apply(Update {
                device: device.clone(),
                command: command.clone(),
                state: ActionState::Pending,
                result: Ok("starting".into()),
            });
            self.actions.perform(&device, command);
        }
    }
}

impl eframe::App for Shelf<'_> {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.show(ctx);
    }
}

#[cfg(test)]
mod tests;
