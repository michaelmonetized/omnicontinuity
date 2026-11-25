# Camera color and frame interval

The 2026-10-18 device check passed dimensions but failed color: source JPEG
RGB `[33,102,203]`, readback PNG `[21,100,215]`.

The failure reproduces without a device. With the old filter, FFmpeg 9.0.2
reports `yuv420p(pc, bt470bg)` after `format=yuv420p`; the sample's Y/U/V values
remain full-range `[93,190,85]`. A plain raw YU12 reader assumes limited range
and produces exactly `[21,100,215]`. Thus this failure is a full/limited range
mismatch, not evidence of a BT.601/709 matrix mismatch.

The camera filter now explicitly converts JPEG BT.601/full-range samples to
BT.601/limited-range samples. It marks the resulting frame with BT.709/sRGB
primaries, sRGB transfer, BT.601 matrix, and limited range. A plain offline raw
YU12 reader produces `[34,101,204]` after that conversion, without reader color
overrides. The actual V4L2 reader used by self-test is unchanged.

Before ffmpeg starts, original Rust V4L2 ABI bindings request tightly packed
1280x720 YU12, sRGB, sRGB transfer, BT.601 encoding, limited range, and a 1/24
second output frame interval. G_FMT and G_PARM readback must match. Equivalent
sRGB-derived default transfer/encoding/quantization values are accepted;
incompatible color metadata, stride, dimensions or frame rate are errors.
This requires no new Cargo dependencies or v4l2-ctl subprocess.

The FFmpeg 9.0.2 V4L2 output backend reads the existing device format, changes
its size and pixel format fields, and writes it back. It does not propagate
AVFrame color properties itself. Explicit device metadata is therefore needed
in addition to converting the actual producer samples. The capture backend
also does not propagate V4L2 color properties into AVCodecParameters, so reader
output alone cannot certify device metadata.

Sources inspected:

- [Kernel sRGB and JPEG definitions](https://docs.kernel.org/userspace-api/media/v4l/colorspaces-details.html#colorspace-srgb-v4l2-colorspace-srgb)
- [Kernel format ABI](https://docs.kernel.org/userspace-api/media/v4l/pixfmt-v4l2.html)
- [Kernel stream parameter ABI](https://docs.kernel.org/userspace-api/media/v4l/vidioc-g-parm.html)
- [FFmpeg scale conversion options](https://ffmpeg.org/ffmpeg-filters.html#scale)
- [FFmpeg 9.0.2 V4L2 output source](https://github.com/FFmpeg/FFmpeg/blob/n9.0.2/libavdevice/v4l2enc.c)
- [FFmpeg 9.0.2 V4L2 capture source](https://github.com/FFmpeg/FFmpeg/blob/n9.0.2/libavdevice/v4l2.c)

Self-test now also decodes its captured PNG to RGB through plain ffmpeg and
requires every channel of every pixel to be within three levels of the known
sample `[34,102,204]`. This is a new acceptance check, not a widened prior
threshold. The bound allows JPEG and 8-bit range conversion rounding; the
observed old failure exceeds it substantially.

Added offline regression `camera_samples_preserve_color_through_an_unforced_yu12_reader`
converts seven solid JPEG colors through the production filter into raw YU12,
then decodes with an unforced raw reader. It compares every channel with the
independently decoded JPEG, with a maximum three-level rounding error. Tests
also verify Linux ABI sizes/request numbers and reject incompatible negotiated
color metadata. These tests do not open any camera device.

The camera owner ran only offline ffmpeg experiments, rustfmt and whitespace
checks. Cargo checks and live device/metadata/frame-rate verification belong
to the orchestrator. No live device, process, driver or configuration changes
were performed by the camera owner. Hardware acceptance remains unclaimed
until that separate validation completes.

Root subsequently validated installed binary
`a16bdb74ed29df65527ac03492d48443b54a334b643d7f3ab89211538a6022a5`
against the real `/dev/video42`. Unforced PNG readback returned RGB
`[34,101,204]` for every pixel, with maximum channel error one. Driver readback
reported 1280x720 YU12, sRGB/BT.601 limited range and 24 fps. The retained
receipts are `var/validation/camera-pixels-final.json`,
`camera-device-format-final.txt` and `camera-self-test-final.log`.
This proves the generated Linux camera path. Safari capture from a physical
phone remains a separate acceptance check.
