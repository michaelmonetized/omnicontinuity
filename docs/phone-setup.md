# Prepare USB phone mirroring

Connect the unlocked iPhone or iPad with a data cable. Existing USB trust,
Developer Mode and a compatible mounted developer image are separate
requirements. Normal USB trust can succeed while the Developer Mode switch
is still hidden.

## Find the connected device

~~~sh
./var/install/bin/oc mirror list
idevicepair --udid DEVICE_UDID validate
idevicedevmodectl --udid DEVICE_UDID list
~~~

Use the exact device identifier from the first command. A successful trust
validation does not prove that developer services are ready.

## Reveal the hidden switch

The installed libimobiledevice 1.4.0 provides a dedicated reveal operation:

~~~sh
idevicedevmodectl --udid DEVICE_UDID reveal
~~~

This requests that the phone display its Developer Mode setting. It does not
enable Developer Mode or request a reboot. Its
[upstream implementation](https://github.com/libimobiledevice/libimobiledevice/blob/master/tools/idevicedevmodectl.c)
uses a separate operation for revealing the menu.

After success, leave and reopen Settings, then open Privacy & Security and
find Developer Mode under Security. Enable it, complete the requested restart,
unlock the phone and confirm enabling it. Follow
[Apple's device instructions](https://developer.apple.com/documentation/xcode/enabling-developer-mode-on-a-device).
Apple's guide describes exposing the setting through its Mac development tools;
the reveal command above supplies the Linux preparation step.

## Check readiness

~~~sh
idevicedevmodectl --udid DEVICE_UDID list
ideviceimagemounter --udid DEVICE_UDID list
./var/install/bin/oc mirror start --udid DEVICE_UDID
~~~

The first check must report enabled. The second inspects legacy mounted images;
it does not mount one. On iOS 27, a developer image installed through cryptexd
can be present even when this legacy image list is empty. Use a fresh Remote
Service Discovery handshake and cryptexd's installed-image query to inspect it.
A missing or incompatible image needs separate setup.
The mirror command uses existing preparation and never pairs the phone or
mounts an image. Closing the mirror window stops the session.

Safari camera capture over Tailscale does not require Developer Mode.

## Observed on 2026-10-18

The connected iPhone reported iPhone18,2 and iOS 27.2. USB trust validated,
but Developer Mode was disabled and its switch was hidden. The targeted
reveal command returned success; the subsequent status check still reported
disabled. Michael then enabled Developer Mode, restarted and unlocked the
phone. Fresh queries confirmed enabled status and valid existing USB trust.

An isolated Rust check using pinned MIT idevice 0.1.68 opened the USB tunnel
and found 64 RSD services: cryptexd was available, the display service was absent,
and no DDI was installed. The current
[upstream preparation guide](https://doronz88.github.io/pymobiledevice3/guides/cli-recipes/)
routes iOS 27 through Cryptex1. No Python was run or imported.

Root downloaded the four image payloads and manifest from
[DeveloperDiskImage revision 6eae353](https://github.com/doronz88/DeveloperDiskImage/tree/6eae353ae694bda1c421d4a3eee5459ae59c99a1/PersonalizedImages/Xcode_iOS_DDI_Cryptex).
All four SHA-384 payload digests matched the selected build identity. The
manifest's fixed local paths and DDI bundle identity were checked. The image
is 15,895,040 bytes; its DDI version is 27.1.5228.8. This is pinned repository
provenance, not an independently verified direct Apple download.

The existing Rust library then requested Apple's personalization ticket and
installed the DDI once, replacing no existing image. Fresh cryptexd readback
confirmed com.apple.MobileAsset.DDI version 27.1.5228.8. A new RSD connection
found 85 services, including com.apple.coredevice.displayservice. Reboot
persistence is untested; the library's installation persistence fields also
require separate review before making any persistence claim.

The exact oc-mirror API produced twelve distinct 1328x2896 RGB24 frames and
confirmed its explicit stop request. Only hashes, dimensions and elapsed time
were retained for that probe. The installed native app then rendered the real
phone screen. USB disconnected shortly afterward: the kernel removed both USB
buses and usbmuxd removed the phone. Stream shutdown after that disconnect was
unconfirmed. Sustained native viewing therefore remains open.

The consolidated device receipt is var/device-checks/phone/acceptance.json.
Setup logs and the isolated Rust utility are retained under ignored
var/device-checks/phone/. The product mirror command still performs no pairing
or developer-image installation itself.
