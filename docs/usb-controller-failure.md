# USB controller failure on 2026-10-18

The replacement cable eventually supplied power. Michael confirmed the iPhone
was connected directly to m1pro16 and charging. Linux reported a partner on
Type-C port3 with host data role and source power role, but no USB root buses
or phone. usbmuxd was inactive because no USB phone had enumerated.

## Reset and kernel fault

Root attempted one targeted reset of the USB controller supplying port3:
f02280000.usb, using the existing dwc3-apple driver bind/unbind interface. The
preconditions checked that there were no enumerated USB devices, the expected
port was connected in host role, and its controller mapping still matched.
No other USB controller, PHY or power-delivery controller was targeted.

At 09:03:21 EDT, writing that controller name to the driver's unbind interface
triggered a kernel NULL pointer dereference at address 0x558. The kernel stack
was mutex_lock -> dwc3_apple_remove -> platform_remove -> device_remove ->
device_release_driver_internal -> unbind_store. This is a kernel failure
during root's reset attempt. It does not prove the same defect caused the
earlier disconnects or the replacement cable's failure to enumerate.

Kernel: 7.1.13-3-2-ARCH. Hardware: MacBookPro18,1/J316s.
Boot ID: a9789444-ef2f-4456-98c0-2503dc1fcfe0.
The kernel identified tee PID 707898 as the current task; sudo PID 707870
propagated SIGSEGV. No userspace coredump was found for either PID. There was
3.6 GiB available memory and no OOM kill in the fault interval.

## State after the fault

The controller's driver link was absent after the failed write. Therefore the
unbind partially took effect even though the command did not return success.
The initial reset receipt's false flags mean command completion failed; they
do not describe the resulting driver attachment. The later failure receipt
records the observed unbound state. No bind or second reset was attempted
after the kernel fault. The other two USB controllers remain bound.

No persistent driver configuration was changed. The installed application,
source, README and earlier device receipts remain present. No project-file
loss was observed; unsaved application state was not inspected. The laptop
was not rebooted automatically. A laptop restart is the recommended clean
recovery from the faulted kernel state; it is not a verified fix for the
original data-connection problem.

The 60-second streaming test is prepared but remains unrun. Before this cable
attempt, one prepared phone delivered twelve real decoded frames, confirmed
an explicit stream stop, and displayed in the native viewer before USB loss.
Those earlier results remain retained passes with their own executable and
source provenance.

## Retained evidence

- var/device-checks/phone/usb-reset-failure/kernel.txt
- var/device-checks/phone/usb-reset-failure/receipt.json
- var/device-checks/phone/usb-reset-failure/coredump-info.txt
- var/device-checks/phone/usb-reset-failure/tee-coredump-info.txt
- var/device-checks/phone/new-cable/controller-reset.json
- var/validation/rsd-context-receipt.json
- var/device-checks/phone/acceptance.json

After reboot, check phone discovery, existing trust, Developer Mode and the
already installed DDI before starting the bounded stream check. Do not repeat
the controller reset as an automatic recovery step.
