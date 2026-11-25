//! Shelf dispatch and cleanup using the main Tokio runtime.

use std::{sync::mpsc, thread, time::Duration};

use oc_shelf::{ActionState, Command, DeviceRow, Frames, MirrorDevice, Model, Update};
use tokio::{runtime::Handle, sync::watch};

mod services;
use services::Services;

struct Actions {
    sender: mpsc::Sender<(DeviceRow, Command)>,
    updates: mpsc::Receiver<Update>,
    notifications: Notifications,
    devices: mpsc::Receiver<Vec<MirrorDevice>>,
    frames: Frames,
}

impl oc_shelf::Actions for Actions {
    fn perform(&mut self, device: &DeviceRow, command: Command) {
        if matches!(command, Command::MirrorStart(_))
            && let Err(error) = self.frames.clear()
        {
            self.notifications
                .send(device, command, ActionState::Finished, Err(error));
            return;
        }
        self.notifications.send(
            device,
            command.clone(),
            ActionState::Pending,
            Ok("starting".into()),
        );
        if self.sender.send((device.clone(), command.clone())).is_err() {
            self.notifications.send(
                device,
                command,
                ActionState::Finished,
                Err("shelf action worker stopped".into()),
            );
        }
    }

    fn updates(&mut self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }

    fn mirror_devices(&mut self) -> Option<Vec<MirrorDevice>> {
        self.devices.try_iter().last()
    }

    fn frame(&mut self) -> Result<Option<oc_shelf::MirrorFrame>, String> {
        self.frames.take()
    }
}

#[derive(Clone)]
struct Notifications {
    updates: mpsc::Sender<Update>,
    devices: mpsc::Sender<Vec<MirrorDevice>>,
}

impl Notifications {
    /// Publish progress or an outcome.
    /// Takes the original row, action, state, and result; sends UI feedback and prints errors.
    fn send(
        &self,
        device: &DeviceRow,
        command: Command,
        state: ActionState,
        result: Result<String, String>,
    ) {
        if let Err(error) = &result {
            eprintln!("oc: {error}");
        }
        let _ = self.updates.send(Update {
            device: device.clone(),
            command,
            state,
            result,
        });
    }
}

/// Resolve a selected handoff peer.
/// Takes a row and returns its unique Tailscale IPv4 or a missing-address error.
fn handoff_machine(device: &DeviceRow) -> Result<&str, String> {
    device
        .ipv4()
        .ok_or("selected device has no Tailscale IPv4")?;
    Ok(&device.ip)
}

/// Build the shelf's initial rows.
/// Takes an already discovered tailnet and returns self first, then every peer.
fn model(tailnet: &oc_core::Tailnet) -> Model {
    let devices = std::iter::once((&tailnet.this_device, true))
        .chain(tailnet.peers.iter().map(|device| (device, false)))
        .map(|(device, self_device)| DeviceRow {
            hostname: device.hostname.clone(),
            os: device.os.clone(),
            ip: device
                .ipv4()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "-".into()),
            online: device.online,
            self_device,
        })
        .collect();
    Model {
        devices,
        selected: 0,
        open_target: String::new(),
    }
}

/// Cancel a startup future when the shelf closes.
/// Takes the close receiver and startup operation; returns its value or a close/error result.
async fn until_closed<T>(
    mut closed: watch::Receiver<bool>,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = closed.wait_for(|closed| *closed) => Err("shelf closed during startup".into()),
        result = operation => result,
    }
}

/// Run the device shelf.
/// Takes no arguments; discovers rows and returns after window and owned-service cleanup.
pub fn run() -> Result<(), String> {
    let tailnet = oc_core::Tailnet::discover()?;
    let model = model(&tailnet);
    let mut worker = Worker::new(tailnet.this_device.ipv4())?;
    let result = oc_shelf::run(model, &mut worker.actions);
    worker.finish(result)
}

/// Run a standalone selected-phone viewer.
/// Takes an optional explicit UDID and returns after the viewer and stream worker stop.
pub fn mirror(udid: Option<String>) -> Result<(), String> {
    let mut worker = Worker::new(None)?;
    let result = oc_shelf::run_mirror(udid, &mut worker.actions);
    worker.finish(result)
}

struct Worker {
    actions: Actions,
    closed: watch::Sender<bool>,
    thread: thread::JoinHandle<Result<(), String>>,
}

impl Worker {
    /// Start the action dispatcher.
    /// Takes the optional local tailnet IP and returns channels plus one owned worker thread.
    fn new(local_ip: Option<std::net::Ipv4Addr>) -> Result<Self, String> {
        let runtime = Handle::try_current().map_err(|error| error.to_string())?;
        let (sender, receiver) = mpsc::channel();
        let (updates_tx, updates) = mpsc::channel();
        let (devices_tx, devices) = mpsc::channel();
        let notifications = Notifications {
            updates: updates_tx,
            devices: devices_tx,
        };
        let frames = Frames::default();
        let worker_frames = frames.clone();
        let worker_notifications = notifications.clone();
        let (closed, shutdown) = watch::channel(false);
        let worker = thread::Builder::new()
            .name("oc-shelf-actions".into())
            .spawn(move || {
                let mut services = Services::new(
                    runtime,
                    local_ip,
                    shutdown.clone(),
                    worker_notifications.clone(),
                    worker_frames,
                );
                while !*shutdown.borrow() {
                    services.poll();
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok((device, command)) => {
                            if *shutdown.borrow() {
                                break;
                            }
                            let result = services.perform(&device, &command);
                            let state = if command.is_service() && result.is_ok() {
                                ActionState::Running
                            } else {
                                ActionState::Finished
                            };
                            if !*shutdown.borrow() {
                                worker_notifications.send(&device, command, state, result);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                services.stop()
            })
            .map_err(|error| format!("shelf worker: {error}"))?;
        let actions = Actions {
            sender,
            updates,
            notifications,
            devices,
            frames,
        };
        Ok(Self {
            actions,
            closed,
            thread: worker,
        })
    }

    /// Stop this window's workers.
    /// Takes the window outcome, cancels startup, joins cleanup, and returns every failure.
    fn finish(self, result: Result<(), String>) -> Result<(), String> {
        let _ = self.closed.send(true);
        drop(self.actions);
        let joined = self
            .thread
            .join()
            .map_err(|_| "shelf action worker panicked".to_owned())
            .and_then(|result| result);
        crate::finish(result, joined)
    }
}

#[cfg(test)]
mod tests;
