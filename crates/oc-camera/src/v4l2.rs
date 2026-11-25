//! Original bindings for the Linux V4L2 format and frame-interval ABI.

use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::{FPS, HEIGHT, WIDTH};

const VIDEO_OUTPUT: u32 = 2;
const YU12: u32 = u32::from_le_bytes(*b"YU12");

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct PixelFormat {
    width: u32,
    height: u32,
    pixel_format: u32,
    field: u32,
    bytes_per_line: u32,
    size_image: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr: u32,
    quantization: u32,
    transfer: u32,
}

#[repr(C)]
union FormatData {
    pixels: PixelFormat,
    bytes: [u8; 200],
    pointer_alignment: *mut c_void,
}

#[repr(C)]
struct Format {
    kind: u32,
    data: FormatData,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct OutputParameters {
    capability: u32,
    mode: u32,
    numerator: u32,
    denominator: u32,
    extended_mode: u32,
    write_buffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
union ParameterData {
    output: OutputParameters,
    bytes: [u8; 200],
}

#[repr(C)]
struct Parameters {
    kind: u32,
    data: ParameterData,
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// Read/write ioctl identifier.
///
/// Accepts a V4L2 command number and ABI buffer type. Returns the Linux generic
/// ioctl request with the exact buffer size, read/write direction and V4L2 type.
const fn request<T>(number: u8) -> c_ulong {
    (3 << 30)
        | ((std::mem::size_of::<T>() as c_ulong) << 16)
        | ((b'V' as c_ulong) << 8)
        | number as c_ulong
}

/// Exchange one V4L2 ABI buffer.
///
/// Accepts an open device, command number and initialized repr(C) buffer whose
/// layout matches that command. Returns the driver's result or its OS error.
fn exchange<T>(file: &File, number: u8, buffer: &mut T) -> Result<(), String> {
    loop {
        let result = unsafe { ioctl(file.as_raw_fd(), request::<T>(number), buffer as *mut T) };
        if result >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("camera V4L2 ioctl {number}: {error}"));
        }
    }
}

/// Validate the negotiated pixel format.
///
/// Accepts the driver's format. Returns success only for tightly packed 720p
/// YU12 with sRGB transfer, BT.601 encoding and limited-range quantization.
fn check_format(pixels: PixelFormat) -> Result<(), String> {
    if pixels.width != u32::from(WIDTH)
        || pixels.height != u32::from(HEIGHT)
        || pixels.pixel_format != YU12
        || pixels.bytes_per_line != u32::from(WIDTH)
        || pixels.size_image != u32::from(WIDTH) * u32::from(HEIGHT) * 3 / 2
        || pixels.field != 1
        || pixels.colorspace != 8
        || !matches!(pixels.transfer, 0 | 2)
        || !matches!(pixels.ycbcr, 0 | 1)
        || !matches!(pixels.quantization, 0 | 2)
    {
        return Err(format!(
            "camera driver changed the required sRGB/BT.601 limited-range YU12 format: {pixels:?}"
        ));
    }
    Ok(())
}

/// Prepare the camera's advertised format and frame rate.
///
/// Accepts the loopback device path. Sets and reads back the same color contract
/// used by ffmpeg, plus a 1/24-second frame interval. Returns errors for rejected
/// or incompatible driver results before any frames are written.
pub(super) fn configure(path: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("camera format device {}: {e}", path.display()))?;
    let mut format = Format {
        kind: VIDEO_OUTPUT,
        data: FormatData { bytes: [0; 200] },
    };
    exchange(&file, 4, &mut format)?;
    format.data.pixels = PixelFormat {
        width: u32::from(WIDTH),
        height: u32::from(HEIGHT),
        pixel_format: YU12,
        field: 1,
        bytes_per_line: u32::from(WIDTH),
        size_image: u32::from(WIDTH) * u32::from(HEIGHT) * 3 / 2,
        colorspace: 8,
        private: 0xfeed_cafe,
        flags: 0,
        ycbcr: 1,
        quantization: 2,
        transfer: 2,
    };
    exchange(&file, 5, &mut format)?;
    exchange(&file, 4, &mut format)?;
    check_format(unsafe { format.data.pixels })?;
    let mut parameters = Parameters {
        kind: VIDEO_OUTPUT,
        data: ParameterData { bytes: [0; 200] },
    };
    exchange(&file, 21, &mut parameters)?;
    parameters.data.output.numerator = 1;
    parameters.data.output.denominator = FPS as u32;
    exchange(&file, 22, &mut parameters)?;
    exchange(&file, 21, &mut parameters)?;
    let output = unsafe { parameters.data.output };
    if output.numerator == 0 || u64::from(output.denominator) != u64::from(output.numerator) * FPS {
        return Err(format!("camera driver rejected {FPS} fps: {output:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_layouts_match_the_linux_abi() {
        assert_eq!(std::mem::size_of::<PixelFormat>(), 48);
        assert_eq!(std::mem::size_of::<Parameters>(), 204);
        assert_eq!(request::<Parameters>(22), 0xc0cc_5616);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<Format>(), 208);
            assert_eq!(request::<Format>(5), 0xc0d0_5605);
        }
    }

    #[test]
    fn rejects_incompatible_colorimetry_and_accepts_equivalent_defaults() {
        let mut pixels = PixelFormat {
            width: 1280,
            height: 720,
            pixel_format: YU12,
            field: 1,
            bytes_per_line: 1280,
            size_image: 1382400,
            colorspace: 8,
            private: 0,
            flags: 0,
            ycbcr: 1,
            quantization: 2,
            transfer: 2,
        };
        assert!(check_format(pixels).is_ok());
        pixels.quantization = 1;
        assert!(check_format(pixels).is_err());
        pixels.quantization = 0;
        pixels.ycbcr = 2;
        assert!(check_format(pixels).is_err());
        pixels.ycbcr = 0;
        pixels.transfer = 0;
        assert!(check_format(pixels).is_ok());
    }
}
