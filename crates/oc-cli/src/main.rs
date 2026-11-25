//! The installable oc app for continuity services.

use std::path::PathBuf;
use std::process::ExitCode;

mod args;
mod shelf;

use args::{Command, HELP};

/// Sync peer mounts.
/// Takes no arguments, reads HOME and USER, and prints changes or returns an error.
fn mount() -> Result<String, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or("HOME must be set")?;
    let user = std::env::var("USER").map_err(|error| format!("USER: {error}"))?;
    let report = oc_mount::sync(&PathBuf::from(home), &user)?;
    let mut lines = Vec::new();
    for (label, paths) in [
        ("mounted", report.mounted),
        ("unmounted", report.unmounted),
        ("unchanged", report.unchanged),
    ] {
        for path in paths {
            let line = format!("{label} {}", path.display());
            println!("{line}");
            lines.push(line);
        }
    }
    Ok(if lines.is_empty() {
        "no peer mounts".into()
    } else {
        lines.join("\n")
    })
}

/// Print nearby AirDrop names.
/// Takes no arguments and returns success or a discovery error.
fn drop_peers() -> Result<String, String> {
    let peers = oc_drop::peers().map_err(|error| error.to_string())?;
    for peer in &peers {
        println!("{peer}");
    }
    Ok(if peers.is_empty() {
        "no AirDrop peers found".into()
    } else {
        peers.join("\n")
    })
}

/// Print device status. Takes no arguments and returns an error if discovery fails.
fn status() -> Result<(), String> {
    let tailnet = oc_core::Tailnet::discover()?;
    println!(
        "{:<6} {:<24} {:<10} {:<39} ONLINE",
        "", "HOSTNAME", "OS", "TAILSCALE IP"
    );
    for (kind, device) in std::iter::once(("self", &tailnet.this_device))
        .chain(tailnet.peers.iter().map(|device| ("peer", device)))
    {
        let ip = device
            .ipv4()
            .map(|ip| ip.to_string())
            .or_else(|| device.ips.first().map(ToString::to_string))
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{kind:<6} {:<24} {:<10} {ip:<39} {}",
            device.hostname, device.os, device.online
        );
    }
    Ok(())
}

/// Combine operation and cleanup outcomes.
/// Takes both results and returns success or every reported failure.
fn finish(result: Result<(), String>, cleanup: Result<(), String>) -> Result<(), String> {
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup: {cleanup}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

/// Wait for interruption or service completion.
/// Takes a process-local completion check and returns after Ctrl-C, exit, or a check error.
async fn wait_for_shutdown(
    mut finished: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        if finished()? {
            return Ok(());
        }
        tokio::select! {
            signal = &mut interrupt => {
                return signal.map_err(|error| format!("Ctrl-C handler: {error}"));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

/// Dispatch a parsed command.
/// Takes a command and returns success or a printable library/shutdown error.
async fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Help => {
            print!("{HELP}");
            Ok(())
        }
        Command::Version => {
            println!("oc {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Status => status(),
        Command::Open { machine, target } => oc_handoff::open(&machine, &target),
        Command::Mount => mount().map(|_| ()),
        Command::Shelf => shelf::run(),
        Command::Camera(options) => oc_camera::run(options).await,
        Command::ClipboardStart(peers) => {
            oc_clipboard::start(oc_clipboard::Config { peers })
                .await
                .map_err(|error| error.to_string())?;
            let result = tokio::select! {
                result = oc_clipboard::wait() => result.map_err(|error| error.to_string()),
                signal = tokio::signal::ctrl_c() => signal.map_err(|error| format!("Ctrl-C handler: {error}")),
            };
            finish(
                result,
                oc_clipboard::stop_local()
                    .await
                    .map_err(|error| error.to_string()),
            )
        }
        Command::ClipboardStop => oc_clipboard::stop()
            .await
            .map_err(|error| error.to_string()),
        Command::ClipboardBridge => oc_clipboard::bridge()
            .await
            .map_err(|error| error.to_string()),
        Command::ClipboardOwn => oc_clipboard::own().await.map_err(|error| error.to_string()),
        Command::DropPeers => drop_peers().map(|_| ()),
        Command::DropSend { name, paths } => {
            oc_drop::send(&name, &paths).map_err(|error| error.to_string())
        }
        Command::DropReceive(directory) => {
            for path in oc_drop::receive(directory).map_err(|error| error.to_string())? {
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::AirplayServe(config) => {
            let mut handle = oc_airplay::serve(config).map_err(|error| error.to_string())?;
            let result = wait_for_shutdown(|| {
                loop {
                    match handle.try_event() {
                        Ok(oc_airplay::Event::Error(error)) => eprintln!("oc: {error}"),
                        Ok(oc_airplay::Event::Unsupported(operation)) => {
                            eprintln!("oc: unsupported AirPlay operation: {operation}")
                        }
                        Ok(_) => {}
                        Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(false),
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(true),
                    }
                }
            })
            .await;
            finish(result, handle.stop().map_err(|error| error.to_string()))
        }
        Command::DisplayExtend(target) => {
            let session = tokio::select! {
                result = oc_display::extend(target) => result.map_err(|error| error.to_string())?,
                signal = tokio::signal::ctrl_c() => return signal.map_err(|error| format!("Ctrl-C handler: {error}")),
            };
            let result = wait_for_shutdown(|| Ok(session.is_finished())).await;
            finish(
                result,
                session.stop().await.map_err(|error| error.to_string()),
            )
        }
        Command::MirrorList => {
            for device in oc_mirror::list_devices()
                .await
                .map_err(|error| error.to_string())?
            {
                println!("{} usb={}", device.udid, device.usbmux_id.is_some());
            }
            Ok(())
        }
        Command::MirrorStart(udid) => shelf::mirror(udid),
        Command::InputServe(config) => {
            let mut handle = oc_input::serve(config).map_err(|error| error.to_string())?;
            let result = wait_for_shutdown(|| Ok(handle.is_finished())).await;
            finish(result, handle.stop().map_err(|error| error.to_string()))
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let result = match args::parse(&args) {
        Ok(command) => run(command).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("oc: {error}");
            ExitCode::FAILURE
        }
    }
}
