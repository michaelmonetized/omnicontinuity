use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::{Actions, Activity, Command, DeviceRow};

/// One phone explicitly chosen from this computer's attached devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorDevice {
    pub udid: String,
}

impl MirrorDevice {
    /// Describe the selected local phone.
    /// Takes this phone and returns an action recipient without claiming a tailnet mapping.
    pub fn row(&self) -> DeviceRow {
        DeviceRow {
            hostname: format!("Phone {}", self.udid),
            os: "iOS".into(),
            ip: "USB".into(),
            online: true,
            self_device: true,
        }
    }
}

/// A decoded RGB24 frame from one selected phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorFrame {
    pub udid: String,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl MirrorFrame {
    /// Validate frame dimensions and storage.
    /// Takes this frame and returns its texture size or an invalid/oversized RGB error.
    pub fn size(&self) -> Result<[usize; 2], String> {
        let width = self.width as usize;
        let height = self.height as usize;
        let length = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or("mirror frame dimensions overflow")?;
        if width == 0 || height == 0 || length > 64 * 1024 * 1024 || length != self.pixels.len() {
            return Err("invalid mirror RGB24 dimensions or byte length".into());
        }
        Ok([width, height])
    }
}

/// A bounded newest-frame slot shared by the stream worker and UI.
#[derive(Clone, Default)]
pub struct Frames(Arc<Mutex<Option<MirrorFrame>>>);

impl Frames {
    /// Replace an unread frame.
    /// Takes validated RGB pixels and retains only this newest frame, or returns a storage error.
    pub fn push(&self, frame: MirrorFrame) -> Result<(), String> {
        frame.size()?;
        *self.0.lock().map_err(|_| "mirror frame slot poisoned")? = Some(frame);
        Ok(())
    }

    /// Receive the newest frame.
    /// Takes this slot and returns its frame once, leaving no queued older frames.
    pub fn take(&self) -> Result<Option<MirrorFrame>, String> {
        Ok(self
            .0
            .lock()
            .map_err(|_| "mirror frame slot poisoned")?
            .take())
    }

    /// Clear a previous stream's frame.
    /// Takes this slot and removes its retained frame before another phone starts.
    pub fn clear(&self) -> Result<(), String> {
        self.take().map(|_| ())
    }
}

#[derive(Default)]
pub(crate) struct Video {
    texture: Option<egui::TextureHandle>,
    udid: String,
    error: Option<String>,
}

impl Video {
    /// Upload a new RGB frame.
    /// Takes the UI context and newest frame; updates one texture or returns a validation error.
    fn receive(
        &mut self,
        ctx: &egui::Context,
        frame: Result<Option<MirrorFrame>, String>,
    ) -> Result<(), String> {
        if let Some(frame) = frame? {
            let image = egui::ColorImage::from_rgb(frame.size()?, &frame.pixels);
            if let Some(texture) = self.texture.as_mut() {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("oc-mirror", image, egui::TextureOptions::LINEAR));
            }
            self.udid = frame.udid;
            self.error = None;
        }
        Ok(())
    }

    /// Draw the selected phone's latest frame.
    /// Takes the UI and height limit; fits RGB pixels without changing their aspect ratio.
    pub(crate) fn show(&mut self, ui: &mut egui::Ui, max_height: f32) {
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(texture) = &self.texture {
            ui.label(format!("Phone {}", self.udid));
            let size = texture.size_vec2();
            let scale = (ui.available_width() / size.x)
                .min(max_height.max(1.0) / size.y)
                .min(1.0);
            ui.image((texture.id(), size * scale));
        } else {
            ui.weak("Waiting for mirror frames");
        }
    }

    /// Receive UI feedback without unwinding.
    /// Takes the UI context and frame source and retains any frame error visibly.
    pub(crate) fn update(&mut self, ctx: &egui::Context, actions: &mut dyn Actions) {
        if let Err(error) = self.receive(ctx, actions.frame()) {
            self.error = Some(error);
        }
    }
}

#[derive(Default)]
pub(crate) struct Phones {
    devices: Vec<MirrorDevice>,
    selected: Option<String>,
    discovered: bool,
}

impl Phones {
    /// Replace discovered phone choices.
    /// Takes a snapshot and keeps an explicit valid selection, choosing automatically only when unique.
    pub(crate) fn replace(&mut self, devices: Vec<MirrorDevice>) {
        if !devices
            .iter()
            .any(|device| Some(&device.udid) == self.selected.as_ref())
        {
            self.selected = if devices.len() == 1 {
                Some(devices[0].udid.clone())
            } else {
                None
            };
        }
        self.devices = devices;
        self.discovered = true;
    }

    /// Check for a completed discovery request.
    /// Takes these choices and returns whether the phone controls should be visible.
    pub(crate) fn discovered(&self) -> bool {
        self.discovered
    }

    /// Draw explicit local-phone selection.
    /// Takes UI availability and returns a chosen start command or a new discovery request.
    pub(crate) fn show(&mut self, ui: &mut egui::Ui, available: bool) -> Option<Command> {
        ui.label("Phones attached to this computer");
        egui::ComboBox::from_id_salt("mirror_phone")
            .selected_text(self.selected.as_deref().unwrap_or("Choose a phone"))
            .show_ui(ui, |ui| {
                for device in &self.devices {
                    ui.selectable_value(
                        &mut self.selected,
                        Some(device.udid.clone()),
                        &device.udid,
                    );
                }
            });
        let mut command = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    available && self.selected.is_some(),
                    egui::Button::new("Start mirror"),
                )
                .clicked()
                && let Some(udid) = self.selected.clone()
            {
                command = Some(Command::MirrorStart(udid));
            }
            if ui
                .add_enabled(available, egui::Button::new("Refresh phones"))
                .clicked()
            {
                command = Some(Command::Mirror);
            }
        });
        command
    }
}

/// Open a standalone mirror viewer.
/// Takes an optional phone UDID and action receiver; returns after window close or initialization failure.
pub fn run_mirror(udid: Option<String>, actions: &mut dyn Actions) -> Result<(), String> {
    let device = MirrorDevice {
        udid: udid.clone().unwrap_or_else(|| "selection".into()),
    }
    .row();
    let command = udid.map_or(Command::Mirror, Command::MirrorStart);
    let auto_start = matches!(command, Command::Mirror);
    actions.perform(&device, command);
    eframe::run_native(
        "OmniContinuity Mirror",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("OmniContinuity Mirror")
                .with_inner_size([480.0, 720.0]),
            renderer: eframe::Renderer::Glow,
            run_and_return: true,
            ..Default::default()
        },
        Box::new(move |_| {
            Ok(Box::new(MirrorWindow {
                actions,
                phones: Phones::default(),
                video: Video::default(),
                activity: Activity::default(),
                auto_start,
            }))
        }),
    )
    .map_err(|error| error.to_string())
}

struct MirrorWindow<'a> {
    actions: &'a mut dyn Actions,
    phones: Phones,
    video: Video,
    activity: Activity,
    auto_start: bool,
}

impl eframe::App for MirrorWindow<'_> {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for update in self.actions.updates() {
            self.activity.apply(update);
        }
        if let Some(devices) = self.actions.mirror_devices() {
            self.phones.replace(devices);
            if self.auto_start
                && let Some(udid) = self.phones.selected.clone()
            {
                let command = Command::MirrorStart(udid.clone());
                self.actions.perform(&MirrorDevice { udid }.row(), command);
            }
            self.auto_start = false;
        }
        self.video.update(ctx, self.actions);
        crate::show_activity(ctx, &self.activity);
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(command) = self
                .phones
                .show(ui, self.activity.available(&Command::Mirror))
            {
                let udid = match &command {
                    Command::MirrorStart(udid) => udid.clone(),
                    _ => "selection".into(),
                };
                self.actions.perform(&MirrorDevice { udid }.row(), command);
            }
            ui.separator();
            let height = ui.available_height() - 28.0;
            self.video.show(ui, height);
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

#[cfg(test)]
mod tests;
