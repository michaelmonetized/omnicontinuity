# OmniContinuity workspace

oc is the Rust application. The current implementation targets Linux with
Hyprland, Wayland, PipeWire and Tailscale. It is not yet the complete
Linux/macOS/Android/iOS/iPadOS/tvOS product described in the README.

## Build and install

Rust 1.88 or newer, a C/C++ compiler, pkg-config, libclang, PipeWire development
headers, OpenSSL, libarchive and libusbmuxd are required to build the application.
OpenH264 is built from source. Its Rust wrapper is pinned to 0.9.3 because newer
0.9 releases require a SIMD dependency above the declared Rust minimum.
Camera and phone-stream TLS use the same ring provider.

~~~sh
mkdir -p var/build-tmp
TMPDIR="$PWD/var/build-tmp" CARGO_BUILD_JOBS=2 cargo test --workspace --locked
node --test tests/camera-page.test.mjs
scripts/install
./var/install/bin/oc --version
./var/install/bin/oc status
./var/install/bin/oc shelf
~~~

scripts/install [prefix] builds a release binary and replaces prefix/bin/oc
atomically. Its default prefix is ./var/install; scripts/install "$HOME/.local"
installs to the user's local bin directory. Build outputs and temporary files
stay in this repository. No service is enabled or started by installation.

## Commands

~~~text
oc help
oc --version
oc status
oc shelf
oc open <machine> <url-or-path>
oc mount
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
~~~

status includes offline peers and duplicate hostnames. open matches a hostname,
DNS name or Tailscale IPv4, requires an online peer and uses existing SSH
credentials. Linux peers use xdg-open, Macs use open; mobile peers are rejected.
The remote user is the local $USER.

mount performs one reconciliation through sshfs. Remote homes appear under
~/Machines/<name>; Mac Mobile Documents appear under ~/Cloud/<name>/<user>.
Only mounts bearing oc's own verified source marker are reconciled or removed.
Changed peer addresses remount owned entries. Legacy or unrelated mounts stay
untouched; an unknown mount occupying a desired path produces an error. This
command is not a background automounter. It requires existing SSH access and
does not configure remote accounts or credentials. Filesystem checks are not
atomic against concurrent path replacement.

Camera serves the embedded phone page at this host's MagicDNS name on port
8443, bound only to its Tailscale IPv4. It writes 1280x720 yuv420p at 24 fps to
/dev/video42, labeled iPhone Camera. Setup uses existing noninteractive sudo
permissions for v4l2loopback, device access and Tailscale certificates. State
defaults to ./var/camera; provide --state-dir from an unwritable directory.
--self-test writes a generated frame to the real V4L2 device and reads it back.
It checks dimensions and every pixel's RGB color. Output is explicitly BT.601
limited-range YU12 with sRGB transfer and a negotiated 24 fps frame interval.
Do not run it while another application uses that device. The original camera/
implementation remains excluded from the workspace.

Clipboard exchanges text and PNG over existing authenticated SSH connections.
Both ends require a running clipboard service and a Wayland session. The SSH
bridge is included in oc; oc-clipboard-bridge remains a compatibility binary.
Host keys and credentials are never changed. macOS clipboard support is absent.

AirDrop uses the existing awdl0 interface in Everyone mode. drop receive
explicitly accepts one transfer into its directory, with size and time limits.
Directories must be archived before sending. No kernel driver, Bluetooth setup,
Apple ID integration or Contacts Only mode is supplied.

AirPlay currently supports protocol development with plaintext H.264 clients.
Current Apple screen mirroring is blocked by missing receiver FairPlay key
unwrapping; modern pairing and AAC playback are also absent. Discovery does not
advertise encrypted screen mirroring as supported.

Display extension requires an OpenDisplay receiver listening on TCP 9000.
Select the named OC-DISPLAY monitor in the capture portal. This uses OpenDisplay
v3; Apple's Sidecar protocol is not implemented. Software encoding is limited
to 1920 pixels per axis at 30 fps. Writable /dev/uinput is required for return
input. Closing the session removes the output it created.

Phone mirroring requires existing trust, Developer Mode, a compatible mounted
developer image and FFmpeg HEVC decoding. It never pairs a phone or mounts an
image. mirror start opens a viewer with explicit attached-phone selection;
--udid selects that exact phone. The shelf also displays its received RGB frames
without opening a second window. Close the window to stop and join the stream.
Only the newest unread frame is retained. Attached phones are not inferred from
Tailscale peer names. USB discovery is wired to the UI; the library's configured
Wi-Fi fallback is not exposed there.
See [phone-setup.md](phone-setup.md) for Linux Developer Mode preparation, the
iOS 27 developer-image path and exact live-device results. Short USB streaming,
one explicit API stop and native rendering passed on one prepared iPhone;
sustained viewing remains open after a USB disconnect.

Input sharing supports relative mice and keyboards through evdev/uinput. Both
machines must run compatible services with distinct Tailscale socket addresses.
Absolute touchpads/tablets and non-Linux input backends are absent. Device
permissions and tailnet ACLs must already allow the operation.

## Crates

| Crate | Responsibility |
| --- | --- |
| oc-cli | Command routing, shelf actions and service cleanup |
| oc-core | Device identity and Tailscale discovery |
| oc-camera | Phone page, TLS, JPEG streaming and V4L2 output |
| oc-clipboard | Wayland text/PNG sync, SSH bridge and deduplication |
| oc-drop | AWDL discovery, AirDrop transfer and archive handling |
| oc-airplay | Discovery, partial pairing, RTSP and plaintext H.264 |
| oc-display | OpenDisplay transport, monitor capture and return input |
| oc-mirror | CoreDevice screen stream and HEVC decoding |
| oc-input | Edge crossing and keyboard/mouse transport |
| oc-handoff | Explicit remote URL/path opening |
| oc-mount | sshfs mount planning and reconciliation |
| oc-shelf | egui device selection and actions |

No GPL implementation was imported. Clipboard code retains its MIT license.
Build/test evidence and the remaining completion work are in
[completion.md](completion.md).
