//! Explicit handoff of a user-supplied URL or path over SSH.

use oc_core::Tailnet;
use std::process::Command;

/// SSH target and the remote argv that opens one url or path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub ssh_target: String,
    pub remote_command: Vec<String>,
}

/// Build the SSH plan for one machine and one url or path.
///
/// `machine` is a Tailscale hostname, DNS name, or IPv4. `target` is a url or a path.
/// Reads the local user from `USER`. Returns the plan, or a printable error.
/// Does not open a connection.
pub fn plan(tailnet: &Tailnet, machine: &str, target: &str) -> Result<Plan, String> {
    let user = std::env::var("USER").map_err(|e| format!("local USER: {e}"))?;
    plan_as(tailnet, &user, machine, target)
}

/// Build a handoff plan for an explicit local user.
///
/// `tailnet` supplies peers; `user` is the SSH user; `machine` matches a hostname,
/// DNS name, or Tailscale IPv4. `target` is a URL with a `scheme://` prefix or a
/// path, both passed unchanged as one argument. Returns a plan or a printable
/// error without opening a connection or reading the environment.
pub fn plan_as(tailnet: &Tailnet, user: &str, machine: &str, target: &str) -> Result<Plan, String> {
    if target.is_empty() || target.contains(['\0', '\n', '\r']) {
        return Err("target must be nonempty and contain no NUL or newline".into());
    }
    if user.is_empty()
        || user.starts_with('-')
        || user.contains(|c: char| c.is_whitespace() || c.is_control() || c == '@')
    {
        return Err("local USER is not a valid SSH user".into());
    }
    let matches: Vec<_> = tailnet
        .peers
        .iter()
        .filter(|peer| {
            !machine.is_empty()
                && (peer.hostname.eq_ignore_ascii_case(machine)
                    || peer
                        .dns_name
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case(machine)
                    || peer.ipv4().is_some_and(|ip| ip.to_string() == machine))
        })
        .collect();
    let peer = match matches.as_slice() {
        [] => return Err(format!("no machine matches {machine:?}")),
        [peer] => *peer,
        _ => {
            let names: Vec<_> = matches
                .iter()
                .map(|peer| {
                    format!(
                        "{} (DNS: {}, IPv4: {})",
                        peer.hostname,
                        peer.dns_name.trim_end_matches('.'),
                        peer.ipv4()
                            .map(|ip| ip.to_string())
                            .unwrap_or_else(|| "none".into())
                    )
                })
                .collect();
            return Err(format!(
                "ambiguous machine {machine:?}: {}",
                names.join(", ")
            ));
        }
    };
    if !peer.online {
        return Err(format!("machine {:?} is offline", peer.hostname));
    }
    let ipv4 = peer
        .ipv4()
        .ok_or_else(|| format!("machine {:?} has no Tailscale IPv4", peer.hostname))?;
    let os = peer.os.to_ascii_lowercase();
    let opener = if os.contains("mac") {
        "open"
    } else if matches!(os.as_str(), "ios" | "ipados" | "android" | "tvos") {
        return Err("that machine cannot open a handoff".into());
    } else {
        "xdg-open"
    };
    Ok(Plan {
        ssh_target: format!("{user}@{ipv4}"),
        remote_command: vec![opener.into(), target.into()],
    })
}

/// Quote one argument for the remote POSIX shell.
///
/// Accepts a literal argv element and returns a single-quoted shell word,
/// escaping embedded single quotes without expanding any of its contents.
fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

/// Prepare SSH without running it.
///
/// Accepts a handoff plan and returns a command with each remote argv element
/// quoted at the SSH boundary, where SSH passes it through the remote shell.
fn ssh_command(plan: &Plan) -> Command {
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .arg(&plan.ssh_target)
        .arg("--")
        .args(plan.remote_command.iter().map(|arg| shell_quote(arg)));
    command
}

/// Open a url or path on another machine.
///
/// `machine` names a Tailscale peer; `target` is a URL or path. Discovers the
/// tailnet, plans using the local `USER`, and runs SSH. Returns success or a
/// printable error including stderr when SSH exits unsuccessfully.
pub fn open(machine: &str, target: &str) -> Result<(), String> {
    let tailnet = Tailnet::discover()?;
    let plan = plan(&tailnet, machine, target)?;
    let output = ssh_command(&plan)
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_core::Device;

    /// Create an online peer fixture with a Tailscale IPv4, without discovery.
    ///
    /// `hostname`, `dns_name`, and `os` identify the returned device.
    fn device(hostname: &str, dns_name: &str, os: &str) -> Device {
        Device {
            hostname: hostname.into(),
            dns_name: dns_name.into(),
            os: os.into(),
            ips: vec!["100.64.0.2".parse().unwrap()],
            online: true,
        }
    }

    /// Create a network map from peer fixtures.
    ///
    /// `peers` supplies the returned map's peers; no environment or network is read.
    fn tailnet(peers: Vec<Device>) -> Tailnet {
        Tailnet {
            backend_state: "Running".into(),
            this_device: device("local", "local.example.ts.net", "linux"),
            peers,
        }
    }

    #[test]
    fn chooses_mac_open_and_linux_xdg_open() {
        for (os, opener) in [
            ("MaCoS", "open"),
            ("mac", "open"),
            ("LiNuX", "xdg-open"),
            ("windows", "xdg-open"),
        ] {
            let net = tailnet(vec![device("desk", "desk.example.ts.net", os)]);
            let result = plan_as(&net, "michael", "DESK", "https://example.com").unwrap();
            assert_eq!(result.ssh_target, "michael@100.64.0.2");
            assert_eq!(result.remote_command, [opener, "https://example.com"]);
        }
    }

    #[test]
    fn matches_hostname_dns_without_trailing_dot_or_ipv4() {
        let net = tailnet(vec![device("desk", "desk.example.ts.net.", "linux")]);
        for machine in ["dEsK", "DESK.EXAMPLE.TS.NET", "100.64.0.2"] {
            let result = plan_as(&net, "alice", machine, "/home/alice/file.txt").unwrap();
            assert_eq!(result.ssh_target, "alice@100.64.0.2");
        }
    }

    #[test]
    fn preserves_urls_and_paths_as_one_literal_argument() {
        let net = tailnet(vec![device("desk", "desk.example.ts.net", "linux")]);
        for target in [
            "https://example.com/a?q=one&two=three#four",
            "a1+.-://example.com/a path",
            "/home/michael/Client's draft.txt",
            "relative path/file.txt",
            "report:final.txt",
            "1://a path",
            "--a-path",
        ] {
            let result = plan_as(&net, "michael", "desk", target).unwrap();
            assert_eq!(result.remote_command, ["xdg-open", target]);
        }
    }

    #[test]
    fn rejects_offline_peer() {
        let mut peer = device("desk", "desk.example.ts.net", "linux");
        peer.online = false;
        let error = plan_as(&tailnet(vec![peer]), "michael", "desk", "/file").unwrap_err();
        assert!(error.contains("offline"), "{error}");
    }

    #[test]
    fn rejects_peers_without_a_tailscale_ipv4() {
        for ips in [vec![], vec!["fd7a::1", "192.168.1.2", "100.128.0.2"]] {
            let mut peer = device("desk", "desk.example.ts.net", "linux");
            peer.ips = ips.into_iter().map(|ip| ip.parse().unwrap()).collect();
            let error = plan_as(&tailnet(vec![peer]), "michael", "desk", "/file").unwrap_err();
            assert!(error.contains("no Tailscale IPv4"), "{error}");
        }
    }

    #[test]
    fn lists_every_ambiguous_hostname_match_including_offline_peers() {
        let first = device("desk", "desk.example.ts.net", "macOS");
        let mut second = device("DESK", "desk-2.example.ts.net", "linux");
        second.ips = vec!["100.64.0.3".parse().unwrap()];
        second.online = false;
        let net = tailnet(vec![first, second]);
        let error = plan_as(&net, "michael", "desk", "/file").unwrap_err();
        for detail in [
            "ambiguous",
            "desk.example.ts.net",
            "desk-2.example.ts.net",
            "100.64.0.2",
            "100.64.0.3",
        ] {
            assert!(error.contains(detail), "{error}");
        }
        assert!(plan_as(&net, "michael", "desk.example.ts.net", "/file").is_ok());
    }

    #[test]
    fn rejects_unknown_or_empty_machine() {
        let net = tailnet(vec![device("desk", "", "linux")]);
        for machine in ["missing", "", "192.168.1.2"] {
            let error = plan_as(&net, "michael", machine, "/file").unwrap_err();
            assert!(error.contains("no machine matches"), "{error}");
        }
    }

    #[test]
    fn rejects_mobile_and_tv_operating_systems() {
        for os in ["iOS", "iPadOS", "AnDrOiD", "tvOS"] {
            let net = tailnet(vec![device("mobile", "mobile.example.ts.net", os)]);
            assert_eq!(
                plan_as(&net, "michael", "mobile", "https://example.com").unwrap_err(),
                "that machine cannot open a handoff"
            );
        }
    }

    #[test]
    fn rejects_empty_nul_and_newline_targets() {
        let net = tailnet(vec![device("desk", "desk.example.ts.net", "linux")]);
        for target in ["", "a\0b", "https://example.com\nanything", "a\rb", "\n"] {
            let error = plan_as(&net, "michael", "desk", target).unwrap_err();
            assert!(error.contains("target"), "{error}");
        }
    }

    #[test]
    fn rejects_invalid_ssh_users() {
        let net = tailnet(vec![device("desk", "desk.example.ts.net", "linux")]);
        for user in ["", "-option", "user@host", "two words", "a\0b", "a\nb"] {
            let error = plan_as(&net, user, "desk", "/file").unwrap_err();
            assert!(error.contains("USER"), "{error}");
        }
    }

    #[test]
    fn quotes_remote_argv_only_at_the_ssh_boundary() {
        let net = tailnet(vec![device("desk", "desk.example.ts.net", "macOS")]);
        let plan = plan_as(&net, "michael", "desk", "/Client's draft.txt").unwrap();
        let command = ssh_command(&plan);
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "michael@100.64.0.2",
                "--",
                "'open'",
                "'/Client'\\''s draft.txt'",
            ]
        );
        assert_eq!(plan.remote_command, ["open", "/Client's draft.txt"]);
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("'"), "''\\'''");
    }

    #[test]
    fn posix_shell_preserves_quotes_spaces_and_metacharacters() {
        for target in [
            "",
            "'",
            "/Client's draft.txt",
            "https://example.com/?q='quoted'&next=$HOME",
            "$(printf expanded); `printf expanded` \\\" * ? [abc]\tend",
        ] {
            let output = Command::new("sh")
                .args(["-c", &format!("printf '%s' {}", shell_quote(target))])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, target.as_bytes());
        }
    }
}
