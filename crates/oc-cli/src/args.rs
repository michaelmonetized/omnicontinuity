use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

pub const HELP: &str = "oc — continuity over your tailnet

Usage:
  oc help
  oc --version
  oc status
  oc open <machine> <url-or-path>
  oc mount
  oc shelf
  oc camera [--self-test] [--device PATH] [--state-dir PATH]
            [--ffmpeg-log PATH] [--png PATH]
  oc clipboard start <user@host>...
  oc clipboard stop
  oc drop peers
  oc drop send <name> <path>...
  oc drop receive <directory>
  oc airplay serve [--name N] [--bind ADDR] [--port N]
  oc display extend wifi <ip>
  oc display extend usb <udid>
  oc mirror list
  oc mirror start [--udid UDID]
  oc input serve --edge left|right|top|bottom --peer ADDR --local ADDR

Camera defaults: /dev/video42, state in ./var/camera, Tailscale IPv4:8443.
Self-test writes a sample frame and grabs a PNG back from the V4L2 device.
Camera setup requires ffmpeg, Tailscale, and v4l2loopback on Linux.
Run from a writable directory or provide --state-dir.
AirPlay defaults: hostname, all IPv4 interfaces, an available port.
AirPlay --bind takes an IP address.
Input --peer and --local take socket addresses, including ports.
Background services stop on Ctrl-C. Mirror viewers stop when their window closes.
";

#[derive(Debug)]
pub enum Command {
    Help,
    Version,
    Status,
    Open { machine: String, target: String },
    Mount,
    Shelf,
    Camera(oc_camera::Options),
    ClipboardStart(Vec<String>),
    ClipboardStop,
    ClipboardBridge,
    ClipboardOwn,
    DropPeers,
    DropSend { name: String, paths: Vec<PathBuf> },
    DropReceive(PathBuf),
    AirplayServe(oc_airplay::Config),
    DisplayExtend(oc_display::Target),
    MirrorList,
    MirrorStart(Option<String>),
    InputServe(oc_input::Config),
}

/// Parse a command.
/// Takes arguments without the binary name and returns a command or usage error.
pub fn parse(args: &[&str]) -> Result<Command, String> {
    match args {
        [] | ["help" | "--help" | "-h"] | ["camera", "--help" | "-h"] => Ok(Command::Help),
        ["--version" | "-V"] => Ok(Command::Version),
        ["status"] => Ok(Command::Status),
        ["open", machine, target] => Ok(Command::Open {
            machine: operand(machine)?.to_owned(),
            target: operand(target)?.to_owned(),
        }),
        ["mount"] => Ok(Command::Mount),
        ["shelf"] => Ok(Command::Shelf),
        ["camera", options @ ..] => camera_options(options).map(Command::Camera),
        ["clipboard", "start", peers @ ..] if !peers.is_empty() => peers
            .iter()
            .map(|peer| operand(peer).map(str::to_owned))
            .collect::<Result<_, _>>()
            .map(Command::ClipboardStart),
        ["clipboard", "stop"] => Ok(Command::ClipboardStop),
        ["clipboard", "bridge"] => Ok(Command::ClipboardBridge),
        ["clipboard", "own"] => Ok(Command::ClipboardOwn),
        ["drop", "peers"] => Ok(Command::DropPeers),
        ["drop", "send", name, paths @ ..] if !paths.is_empty() => Ok(Command::DropSend {
            name: operand(name)?.to_owned(),
            paths: paths
                .iter()
                .map(|path| operand(path).map(PathBuf::from))
                .collect::<Result<_, _>>()?,
        }),
        ["drop", "receive", directory] => {
            Ok(Command::DropReceive(PathBuf::from(operand(directory)?)))
        }
        ["airplay", "serve", options @ ..] => airplay_config(options).map(Command::AirplayServe),
        ["display", "extend", "wifi", ip] => Ok(Command::DisplayExtend(oc_display::Target::Wifi(
            operand(ip)?
                .parse::<IpAddr>()
                .map_err(|error| format!("invalid display IP: {error}"))?,
        ))),
        ["display", "extend", "usb", udid] => Ok(Command::DisplayExtend(oc_display::Target::Usb {
            udid: operand(udid)?.to_owned(),
        })),
        ["mirror", "list"] => Ok(Command::MirrorList),
        ["mirror", "start"] => Ok(Command::MirrorStart(None)),
        ["mirror", "start", "--udid", udid] => {
            Ok(Command::MirrorStart(Some(operand(udid)?.to_owned())))
        }
        ["input", "serve", options @ ..] => input_config(options).map(Command::InputServe),
        _ => Err(format!(
            "invalid command or arguments: {}\n\n{HELP}",
            args.join(" ")
        )),
    }
}

/// Check an operand.
/// Takes one positional value and returns it or an empty-value/unknown-option error.
fn operand(value: &str) -> Result<&str, String> {
    if value.is_empty() {
        Err("argument must not be empty".into())
    } else if value.starts_with('-') {
        Err(format!("unknown option: {value}"))
    } else {
        Ok(value)
    }
}

/// Read an option value.
/// Takes remaining arguments and a flag name; returns a value or missing-value error.
fn value<'a>(args: &mut impl Iterator<Item = &'a str>, flag: &str) -> Result<&'a str, String> {
    args.next()
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or_else(|| format!("missing value for {flag}"))
}

/// Parse camera options.
/// Takes camera arguments and returns options or a usage error without accessing devices.
fn camera_options(args: &[&str]) -> Result<oc_camera::Options, String> {
    let mut options = oc_camera::Options::default();
    let mut args = args.iter().copied();
    while let Some(flag) = args.next() {
        match flag {
            "--self-test" => options.self_test = true,
            "--device" => options.device = PathBuf::from(value(&mut args, flag)?),
            "--state-dir" => options.state_dir = PathBuf::from(value(&mut args, flag)?),
            "--ffmpeg-log" => options.ffmpeg_log = Some(PathBuf::from(value(&mut args, flag)?)),
            "--png" => options.png = Some(PathBuf::from(value(&mut args, flag)?)),
            _ => return Err(format!("unknown camera option: {flag}")),
        }
    }
    Ok(options)
}

/// Parse AirPlay settings.
/// Takes serve arguments and returns default settings with overrides or a usage error.
fn airplay_config(args: &[&str]) -> Result<oc_airplay::Config, String> {
    let mut config = oc_airplay::Config::default();
    let mut args = args.iter().copied();
    while let Some(flag) = args.next() {
        match flag {
            "--name" => config.name = value(&mut args, flag)?.to_owned(),
            "--bind" => {
                config.bind_address = value(&mut args, flag)?
                    .parse::<IpAddr>()
                    .map_err(|error| format!("invalid AirPlay bind IP: {error}"))?;
            }
            "--port" => {
                config.port = value(&mut args, flag)?
                    .parse::<u16>()
                    .map_err(|error| format!("invalid AirPlay port: {error}"))?;
            }
            _ => return Err(format!("unknown airplay option: {flag}")),
        }
    }
    Ok(config)
}

/// Parse input sharing settings.
/// Takes serve arguments and returns required edge/socket settings or a usage error.
fn input_config(args: &[&str]) -> Result<oc_input::Config, String> {
    let (mut edge, mut peer, mut local) = (None, None, None);
    let mut args = args.iter().copied();
    while let Some(flag) = args.next() {
        match flag {
            "--edge" => {
                edge = Some(match value(&mut args, flag)? {
                    "left" => oc_input::Edge::Left,
                    "right" => oc_input::Edge::Right,
                    "top" => oc_input::Edge::Top,
                    "bottom" => oc_input::Edge::Bottom,
                    edge => return Err(format!("invalid input edge: {edge}")),
                });
            }
            "--peer" | "--local" => {
                let address = value(&mut args, flag)?
                    .parse::<SocketAddr>()
                    .map_err(|error| format!("invalid {flag} socket address: {error}"))?;
                if flag == "--peer" {
                    peer = Some(address);
                } else {
                    local = Some(address);
                }
            }
            _ => return Err(format!("unknown input option: {flag}")),
        }
    }
    Ok(oc_input::Config::new(
        edge.ok_or("missing --edge")?,
        peer.ok_or("missing --peer")?,
        local.ok_or("missing --local")?,
    ))
}

#[cfg(test)]
mod tests;
