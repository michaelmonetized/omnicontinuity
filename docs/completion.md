# Completion record

## Recovery on 2026-10-18

Eleven prior Codex sessions were recovered from their saved conversations. They
had completed their assigned cuts; the integrated project had no completion
record. All source work remained uncommitted above baseline 0d13d47.
The existing README modification belongs to the prior work and is preserved.

| Work | Prior session |
| --- | --- |
| Workspace, status and camera | 01a14ae7-4a47-7140-8c9d-072fc951ab63 |
| AirDrop | 01a14ae7-f5d6-7993-8af3-b51dd05c18fe |
| Phone stream and input | 01a14ae7-f5e7-7a21-b8ff-aa5c53c21486 |
| AirPlay | 01a14ae7-f5f5-7971-a09b-e713543a2da5 |
| Display extension | 01a14ae7-f601-7b73-80aa-82c68ebe83f2 |
| Clipboard | 01a14ae7-f7e7-7ff2-9262-fa02eadddf45 |
| Handoff | 01a14b05-703a-7ab0-a5df-f5636e345bc4 |
| CLI protocol commands | 01a14b05-703e-75b2-b984-0917200d2035 |
| Mounts | 01a14b05-703f-7911-838c-fbf7260ce63d |
| Shelf | 01a14b05-705b-7a31-a896-323f4ef1cfea |
| CLI shelf integration | 01a14b0c-ab5b-7093-8934-571baca6e98d |

The original clipboard, AirPlay and CLI owners were resumed for integration
repairs. The root owns workspace dependencies, centralized validation, install
delivery and this record. Owners do not run competing Cargo jobs.

## Evidence

The first recovered-tree workspace test run passed 148 unit tests and 3 doc
tests. Three optional host tests were skipped. It did not establish device
interoperability. Strict Clippy then found one shelf-test style failure, which
was routed to its owner.

The integrated software checkpoint passed 200 unit tests and 3 doc tests.
Three host tests and one subprocess harness were skipped; the harness ran
through the separate-process regression. Strict workspace Clippy, formatting,
and an actual Cargo/rustc 1.88 all-target check passed. The tested source
manifest is var/validation/source-software-checkpoint.json, digest
96f1440fbd0ef9d9c2f149836763825bc4d91d08440f6255f7bf57dbcaae12aa.

That release is retained at var/validation/oc-before-camera-range, SHA-256
67fb70d0d3ee26aec109329363e7e1188d3f1a6cd3663f394e284d3def7aeb0b.
Its status and help commands passed. Both native shelf and mirror windows
mapped and closed through Hyprland with exit zero and no surviving window.
The mirror smoke used an absent UDID and verified the visible failure route;
it did not start a phone stream. Receipts and screenshots are in
var/validation/native-receipt.json and native-*.png.

The first full repaired-tree run caught a clipboard lock-release race.
The failed log remains final-tests-attempt1.log. Explicit unlock plus a held
duplicate-descriptor regression fixed the invariant; the subsequent full run
passed. One strict-Clippy test allocation warning was fixed before the final
software checkpoint.

## Live device follow-on

Michael selected this Linux host plus his iPhone/iPad. Tailscale reports an
online iPhone; no USB phone was visible and usbmuxd was inactive at the first
probe. Readiness of the device is requested separately.

The installed camera completed real V4L2 write/readback at 1280x720, but its
independent color check failed: the reference JPEG decoded to RGB 33/102/203,
while unforced device readback decoded to 21/100/215. This is a retained
failure, not camera acceptance. The old full-range producer bytes were being
read as limited-range YU12. Its PNG and metrics are preserved with
before-range-fix names under var/device-checks/camera and var/validation.

The camera owner fixed producer range, negotiated V4L2 metadata and frame rate,
and added strict pixel validation. The CLI/UI owner fixed phone-page transition
races, WebSocket queue growth and wake-lock cleanup. Root independently ran all
14 tests in tests/camera-page.test.mjs; these model browser APIs and do not
certify Safari behavior.

The camera software checkpoint passed 203 unit tests and 3 doc tests, with zero failures and
the same four ignored entries. Strict workspace Clippy, formatting, installer
syntax, whitespace checks and actual Cargo/rustc 1.88 all-target checking passed.
The 81-file source manifest is var/validation/source-final.json, digest
ab29621a22d8e26ade71814fa5f74a27b144be31c226c7f0887e3a67929f0be2.
At that checkpoint, the installed release at var/install/bin/oc had SHA-256
a16bdb74ed29df65527ac03492d48443b54a334b643d7f3ab89211538a6022a5.
The consolidated receipt is var/validation/final-receipt.json.

That exact binary passed real V4L2 self-test. Independent unforced readback
decoded every pixel to RGB 34/101/204: maximum channel error one, with all pixels
within the three-level bound. Driver readback confirms 1280x720 YU12, sRGB,
BT.601 limited range and 24 fps. Logs, PNG, pixel metrics and device metadata
carry final names under var/validation and var/device-checks/camera.
The preceding build, SHA-256
4b4827869218d60c35c8e5bc25dd3148ff39578b6de244e0b204620a1e4c8838,
also mapped and closed shelf and mirror windows successfully with exit zero.
Its mirror smoke selected an absent UDID and produced no phone frames.
That software checkpoint added context to a USB developer-service error.
Its subsequent live native phone check is recorded below.

The private camera page is served at
https://m1pro16.tail97f2f8.ts.net:8443/ by the transient user unit
oc-camera-check-20261018.service, limited to one hour. HTTPS certificate
validation and the served page's source hash pass. The listener binds only
100.74.233.121:8443; certificate directory and private-key permissions are
0700 and 0600. Stop it with
systemctl --user stop oc-camera-check-20261018.service.
At the first live check its counters were accepted=0 and published=0.
Safari-to-Linux delivery remains unproven until a phone sends frames.

Michael reported the phone/tablet connected and unlocked. The first probe still
showed no USB devices or root hubs. USB subsequently enumerated an iPhone18,2
running iOS 27.2, and existing trust validated successfully. The actual native
mirror attempt returned service not found. Independent developer-service
queries reported that Developer Mode must be enabled before continuing; image
mount status could not be read. The viewer closed with exit zero after retaining
the failed start in var/validation/phone-mirror-live.log. The mirror owner then
added the service stage and setup prerequisites to the error, preserving its
underlying cause. Fresh workspace validation and installation include that fix.
Root started the existing usbmuxd service for the check. At that checkpoint,
no device pairing, image installation, driver reload or persistent service
enablement had occurred.
AirDrop discovery completed without finding a peer; no transfer was attempted.

The phone's Developer Mode switch was hidden despite valid USB trust. Root
subsequently used the installed idevicedevmodectl 1.4.0 reveal operation against
that exact phone. The device returned success, and a fresh status check still
reported disabled. No enable, arm, confirm or reboot operation was sent. The
Linux preparation steps and their proof boundary are in [phone-setup.md](phone-setup.md).

Michael enabled Developer Mode and unlocked the phone after restarting.
Fresh checks confirmed enabled status and valid USB trust. Root's isolated
Rust utility found no DDI installed. After verifying all four payload SHA-384
digests from pinned DeveloperDiskImage revision 6eae353, the existing MIT
idevice 0.1.68 library personalized through Apple's TSS and installed DDI
27.1.5228.8. Readback confirmed it, and a fresh RSD connection exposed the
previously absent display service. No existing image was replaced; reboot
persistence remains unrun. See phone-setup.md for exact provenance boundaries.

The real oc-mirror API then delivered twelve distinct 1328x2896 RGB24 frames
and confirmed explicit stop. The installed a16bdb74 build displayed the phone
screen in its native viewer. At 08:35:23 EDT the USB phone disconnected; the
kernel removed both USB buses and usbmuxd exited. The viewer retained its last
frame and reported the transport failure with unconfirmed device shutdown.
Root closed that failed viewer; no decoder remained. This proves actual frame
delivery, one successful API stop, and native rendering. It does not qualify
sustained viewing or shutdown following a lost USB connection. The device
receipt is var/device-checks/phone/acceptance.json.

The mirror owner subsequently added the exact display-service stage and image
prerequisite to its downstream connection error, preserving the original cause.
The resulting tree again passed 206 Rust tests, strict workspace Clippy,
formatting and actual Cargo/rustc 1.88 all-target checking. The 14 browser tests
remain retained passes against unchanged HTML and test sources. Latest source
manifest var/validation/source-rsd-context.json has digest
b82e4b7ef03a9d34d285e56e1e702c7b52e4ec1c8277f517a81e7d6f34a604b3;
the latest installed release has SHA-256
e7af0b7247ca4e560cad9ea0e6ee597e934616a91635ee10a03d4091980dac72.
The new receipt is var/validation/rsd-context-receipt.json. Its runtime phone and
camera evidence explicitly retains the earlier a16bdb74 executable. A stable
USB connection is still required to rerun native viewing on the latest binary.
Michael suspects the cable; the cause of the USB loss is not yet established.

A replacement cable later supplied power, but the phone still did not
enumerate. Root's single targeted USB-controller reset triggered a kernel
NULL-pointer fault in dwc3_apple_remove and left that controller unbound.
No further driver writes or automatic reboot followed. Exact failure evidence,
current state and recovery steps are in [usb-controller-failure.md](usb-controller-failure.md).
The sustained stream check remains unrun. This new kernel failure was triggered
by the reset; it does not establish the cause of the earlier USB losses.

All logs and runtime artifacts live under ignored var/; documentation stays here.

## Work still required for the full product

| Area | Remaining work and acceptance |
| --- | --- |
| Apple AirPlay | Receiver FairPlay key unwrapping, modern pairing, audio playback, then an actual current Apple sender session |
| Phone mirroring | USB frame delivery, one explicit API stop and native rendering pass on the prepared iPhone; qualify sustained viewing/reconnect and expose existing Wi-Fi credentials |
| Native platforms | macOS, Android, iOS/iPadOS and tvOS applications/backends and platform packaging; the Linux workspace is not evidence of these |
| Automatic mounts | An owned background lifecycle and recovery policy; current command only reconciles once |
| Nearby and automatic handoff | Proximity discovery and platform app-state handoff are absent; current handoff opens an explicit URL or path over SSH |
| Shelf workflows | File/URL transfer choice and supported-platform capability controls; phone selection/viewing and action feedback are implemented |
| Real devices | AirDrop send/receive, Safari-to-camera delivery, OpenDisplay video/return input, sustained phone viewing and two-host input sharing; Linux V4L2 readback and short USB phone streaming pass |
| Live clipboard | Two isolated Wayland/SSH endpoints, text/PNG, reconnect, echo suppression, cross-process stop and copied-data retention |
| Distribution | Per-platform packages and licensing/release policy after the missing platform implementations and device checks |

The AirPlay blocker is the receiver's
[FairPlay-wrapped session key](https://openairplay.github.io/airplay-spec/screen_mirroring/http_requests.html).
The investigated openairplay2 candidate
[excludes its copied FairPlay tables from its MIT grant](https://docs.rs/crate/openairplay2/0.1.0/source/NOTICE.md);
AirPlayServer's
[PlayFair component is GPL-3.0](https://github.com/xenos1337/AirPlayServer/blob/master/THIRD_PARTY_NOTICES.md).
The independently researched sender SAP project
[does not extract content keys](https://github.com/objevovat/fairplay-sap-core-airplay2-sender-authentication-handshake/blob/main/docs/09-limitations.md).
None was imported. This is an implementation and provenance gap; it does not
establish that an independent implementation is impossible.

Framing, loopback transfers, decoded synthetic frames and headless widget tests
prove their exact software paths. They do not prove Apple compatibility,
physical input, audibility or human acceptance. No commits, pushes, PRs,
phone pairing or unrelated process termination was performed. The later single
USB-controller reset and resulting kernel failure are recorded above; no
persistent driver configuration changed.
