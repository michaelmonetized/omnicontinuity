use std::{
    net::{Ipv4Addr, SocketAddr},
    process::Command as Process,
    sync::mpsc,
};

use oc_shelf::{ActionState, Command, DeviceRow, Frames, MirrorDevice, MirrorFrame};
use tokio::{runtime::Handle, sync::watch, task::JoinHandle};

use super::{Notifications, handoff_machine, until_closed};

struct Owned<T> {
    device: DeviceRow,
    handle: T,
}

struct Mirror {
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<(), String>>,
}

pub(super) struct Services {
    runtime: Handle,
    local_ip: Option<Ipv4Addr>,
    closed: watch::Receiver<bool>,
    notifications: Notifications,
    frames: Frames,
    clipboard: Option<Owned<JoinHandle<Result<(), String>>>>,
    airplay: Option<Owned<oc_airplay::Handle>>,
    display: Option<Owned<oc_display::DisplaySession>>,
    mirror: Option<Owned<Mirror>>,
    input: Option<Owned<oc_input::InputServer>>,
}

impl Services {
    /// Create an empty service owner.
    /// Takes main's runtime, local IP, shutdown receiver, and UI sender; starts no services.
    pub(super) fn new(
        runtime: Handle,
        local_ip: Option<Ipv4Addr>,
        closed: watch::Receiver<bool>,
        notifications: Notifications,
        frames: Frames,
    ) -> Self {
        Self {
            runtime,
            local_ip,
            closed,
            notifications,
            frames,
            clipboard: None,
            airplay: None,
            display: None,
            mirror: None,
            input: None,
        }
    }

    /// Run one shelf action.
    /// Takes a selected row and command; returns a visible startup/outcome message or an error.
    pub(super) fn perform(
        &mut self,
        device: &DeviceRow,
        command: &Command,
    ) -> Result<String, String> {
        match command {
            Command::Open(target) if device.self_device => {
                let status = Process::new("xdg-open")
                    .arg(target)
                    .status()
                    .map_err(|error| format!("xdg-open: {error}"))?;
                if !status.success() {
                    return Err(format!("xdg-open: {status}"));
                }
                Ok(format!("opened {target}"))
            }
            Command::Open(target) => {
                oc_handoff::open(handoff_machine(device)?, target)?;
                Ok(format!("opened {target}"))
            }
            Command::Mount => crate::mount(),
            Command::Drop => crate::drop_peers(),
            Command::Clipboard => {
                if self.clipboard.is_some() {
                    return Err("clipboard is already running".into());
                }
                let ip = device
                    .ipv4()
                    .ok_or("selected device has no Tailscale IPv4")?;
                let user = std::env::var("USER").map_err(|error| format!("USER: {error}"))?;
                self.runtime
                    .block_on(oc_clipboard::start(oc_clipboard::Config {
                        peers: vec![format!("{user}@{ip}")],
                    }))
                    .map_err(|error| error.to_string())?;
                let task = self.runtime.spawn(async {
                    oc_clipboard::wait()
                        .await
                        .map_err(|error| error.to_string())
                });
                self.clipboard = Some(Owned {
                    device: device.clone(),
                    handle: task,
                });
                Ok(format!("syncing with {ip}"))
            }
            Command::AirPlay => {
                if self.airplay.is_some() {
                    return Err("AirPlay is already running".into());
                }
                let handle = oc_airplay::serve(oc_airplay::Config::default())
                    .map_err(|error| error.to_string())?;
                let address = handle.local_addr();
                self.airplay = Some(Owned {
                    device: device.clone(),
                    handle,
                });
                Ok(format!("listening on {address}"))
            }
            Command::Display => {
                if self.display.is_some() {
                    return Err("display is already running".into());
                }
                let ip = device
                    .ipv4()
                    .ok_or("selected device has no Tailscale IPv4")?;
                let session = self
                    .runtime
                    .block_on(until_closed(self.closed.clone(), async {
                        oc_display::extend(oc_display::Target::Wifi(ip.into()))
                            .await
                            .map_err(|error| error.to_string())
                    }))?;
                let output = session.output_name().to_owned();
                self.display = Some(Owned {
                    device: device.clone(),
                    handle: session,
                });
                Ok(format!("{output} on {ip}"))
            }
            Command::Mirror => {
                if self.mirror.is_some() {
                    return Err("mirror is already running".into());
                }
                let devices = self.discover_mirrors()?;
                if devices.is_empty() {
                    return Err("no mirror devices found".into());
                }
                Ok("choose an attached phone to mirror".into())
            }
            Command::MirrorStart(udid) => {
                if self.mirror.is_some() {
                    return Err("mirror is already running".into());
                }
                let phone = select_mirror(self.discover_mirrors()?, udid)?;
                let handle = self.runtime.block_on(start_mirror(
                    phone,
                    self.closed.clone(),
                    self.frames.clone(),
                ))?;
                self.mirror = Some(Owned {
                    device: device.clone(),
                    handle,
                });
                Ok(format!("viewing phone {udid}"))
            }
            Command::Input => {
                if self.input.is_some() {
                    return Err("input is already running".into());
                }
                let ip = device
                    .ipv4()
                    .ok_or("selected device has no Tailscale IPv4")?;
                let peer = SocketAddr::from((ip, 4242));
                let local = SocketAddr::from((
                    self.local_ip.ok_or("this machine has no Tailscale IPv4")?,
                    4242,
                ));
                let handle =
                    oc_input::serve(oc_input::Config::new(oc_input::Edge::Right, peer, local))
                        .map_err(|error| error.to_string())?;
                self.input = Some(Owned {
                    device: device.clone(),
                    handle,
                });
                Ok(format!("right edge to {peer}"))
            }
        }
    }

    /// Discover attached phones.
    /// Takes this worker and publishes explicit USB choices without selecting a tailnet peer.
    fn discover_mirrors(&self) -> Result<Vec<oc_mirror::Device>, String> {
        let devices = self
            .runtime
            .block_on(until_closed(self.closed.clone(), async {
                oc_mirror::list_devices()
                    .await
                    .map_err(|error| error.to_string())
            }))?;
        let _ = self.notifications.devices.send(
            devices
                .iter()
                .map(|device| MirrorDevice {
                    udid: device.udid.clone(),
                })
                .collect(),
        );
        Ok(devices)
    }

    /// Reap completed services and publish runtime failures.
    /// Takes this worker, joins only finished handles, and makes their actions available again.
    pub(super) fn poll(&mut self) {
        if self
            .clipboard
            .as_ref()
            .is_some_and(|service| service.handle.is_finished())
            && let Some(service) = self.clipboard.take()
        {
            let waited = self
                .runtime
                .block_on(service.handle)
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            let stopped = self
                .runtime
                .block_on(oc_clipboard::stop_local())
                .map_err(|error| error.to_string());
            self.finished(
                &service.device,
                Command::Clipboard,
                crate::finish(waited, stopped),
            );
        }
        if self
            .display
            .as_ref()
            .is_some_and(|service| service.handle.is_finished())
            && let Some(service) = self.display.take()
        {
            let result = self
                .runtime
                .block_on(service.handle.wait())
                .map_err(|error| error.to_string());
            self.finished(&service.device, Command::Display, result);
        }
        if self
            .mirror
            .as_ref()
            .is_some_and(|service| service.handle.task.is_finished())
            && let Some(service) = self.mirror.take()
        {
            let result = self
                .runtime
                .block_on(service.handle.task)
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            self.finished(&service.device, Command::Mirror, result);
        }
        if self
            .input
            .as_ref()
            .is_some_and(|service| service.handle.is_finished())
            && let Some(mut service) = self.input.take()
        {
            self.finished(
                &service.device,
                Command::Input,
                service.handle.stop().map_err(|error| error.to_string()),
            );
        }
        let ended = self.airplay.as_ref().is_some_and(|service| {
            loop {
                let result = match service.handle.try_event() {
                    Ok(oc_airplay::Event::Error(error)) => Err(error),
                    Ok(oc_airplay::Event::Unsupported(operation)) => {
                        Err(format!("unsupported AirPlay operation: {operation}"))
                    }
                    Ok(oc_airplay::Event::Connected(address)) => Ok(format!("connected {address}")),
                    Ok(oc_airplay::Event::Disconnected(address)) => {
                        Ok(format!("disconnected {address}; listening"))
                    }
                    Err(mpsc::TryRecvError::Empty) => return false,
                    Err(mpsc::TryRecvError::Disconnected) => return true,
                };
                self.notifications.send(
                    &service.device,
                    Command::AirPlay,
                    ActionState::Running,
                    result,
                );
            }
        });
        if ended && let Some(mut service) = self.airplay.take() {
            self.finished(
                &service.device,
                Command::AirPlay,
                service.handle.stop().map_err(|error| error.to_string()),
            );
        }
    }

    /// Publish service completion.
    /// Takes the owning device, command, and cleanup result; preserves errors in the UI.
    fn finished(&self, device: &DeviceRow, command: Command, result: Result<(), String>) {
        self.notifications.send(
            device,
            command,
            ActionState::Finished,
            result.map(|()| "stopped".into()),
        );
    }

    /// Stop owned services.
    /// Takes this worker, attempts every cleanup, and returns all shutdown failures after joining.
    pub(super) fn stop(mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        let notifications = self.notifications.clone();
        let mut stopped = |device: &DeviceRow, command, result: Result<(), String>| {
            if let Err(error) = &result {
                errors.push(error.clone());
            }
            notifications.send(
                device,
                command,
                ActionState::Finished,
                result.map(|()| "stopped".into()),
            );
        };
        if let Some(service) = self.clipboard.take() {
            let result = self
                .runtime
                .block_on(oc_clipboard::stop_local())
                .map_err(|error| error.to_string());
            let waited = self
                .runtime
                .block_on(service.handle)
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            stopped(
                &service.device,
                Command::Clipboard,
                crate::finish(result, waited),
            );
        }
        if let Some(mut service) = self.airplay.take() {
            stopped(
                &service.device,
                Command::AirPlay,
                service.handle.stop().map_err(|error| error.to_string()),
            );
        }
        if let Some(service) = self.display.take() {
            stopped(
                &service.device,
                Command::Display,
                self.runtime
                    .block_on(service.handle.stop())
                    .map_err(|error| error.to_string()),
            );
        }
        if let Some(service) = self.mirror.take() {
            let _ = service.handle.stop.send(true);
            let result = self
                .runtime
                .block_on(service.handle.task)
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            stopped(&service.device, Command::Mirror, result);
        }
        if let Some(mut service) = self.input.take() {
            stopped(
                &service.device,
                Command::Input,
                service.handle.stop().map_err(|error| error.to_string()),
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Select an attached phone by its explicit identity.
/// Takes discovered phones and a UDID; returns that phone or an error without substituting another.
fn select_mirror(devices: Vec<oc_mirror::Device>, udid: &str) -> Result<oc_mirror::Device, String> {
    devices
        .into_iter()
        .find(|device| device.udid == udid)
        .ok_or_else(|| format!("mirror device not found: {udid}"))
}

/// Start and display an explicitly selected mirror device.
/// Takes the phone, close receiver, and bounded frame slot; returns owned shutdown controls.
async fn start_mirror(
    device: oc_mirror::Device,
    closed: watch::Receiver<bool>,
    frames: Frames,
) -> Result<Mirror, String> {
    let udid = device.udid.clone();
    frames.clear()?;
    let mut handle = oc_mirror::start(device)
        .await
        .map_err(|error| error.to_string())?;
    if *closed.borrow() {
        handle.stop().await.map_err(|error| error.to_string())?;
        return Err("shelf closed during mirror startup".into());
    }
    let stop = handle.stop_signal();
    let task = tokio::spawn(async move {
        let result = loop {
            match handle.next_frame().await {
                Ok(Some(frame)) => {
                    if let Err(error) = frames.push(MirrorFrame {
                        udid: udid.clone(),
                        width: frame.width,
                        height: frame.height,
                        pixels: frame.pixels,
                    }) {
                        break Err(error);
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error.to_string()),
            }
        };
        crate::finish(
            result,
            handle.stop().await.map_err(|error| error.to_string()),
        )
    });
    Ok(Mirror { stop, task })
}

#[cfg(test)]
mod tests;
