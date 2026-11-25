use crate::invalid;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// H.264 destination.
///
/// `Window` starts ffplay when a sender delivers codec data. `Fd` writes raw
/// Annex B H.264 to a descriptor duplicated from the caller; the caller keeps
/// its original descriptor. Non-file descriptors must already be nonblocking.
#[derive(Debug)]
pub enum VideoOutput {
    Window,
    Fd(OwnedFd),
}

impl VideoOutput {
    pub(crate) fn validate(&self) -> io::Result<()> {
        match self {
            Self::Fd(fd) => check_fd(fd.as_raw_fd()),
            Self::Window => Ok(()),
        }
    }

    /// Duplicate the caller's output descriptor.
    ///
    /// Takes a borrowed writable descriptor and returns an independently owned
    /// duplicate. Pipes and sockets must have O_NONBLOCK set by the caller.
    pub fn from_fd(fd: BorrowedFd<'_>) -> io::Result<Self> {
        check_fd(fd.as_raw_fd())?;
        Ok(Self::Fd(fd.try_clone_to_owned()?))
    }

    pub(crate) fn open(&self, title: &str) -> io::Result<Sink> {
        match self {
            Self::Fd(fd) => {
                check_fd(fd.as_raw_fd())?;
                Ok(Sink {
                    writer: File::from(fd.try_clone()?),
                    child: None,
                })
            }
            Self::Window => {
                let mut child = Command::new("ffplay")
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-fflags",
                        "nobuffer",
                        "-flags",
                        "low_delay",
                        "-probesize",
                        "32",
                        "-analyzeduration",
                        "0",
                        "-framedrop",
                        "-f",
                        "h264",
                        "-window_title",
                        title,
                        "-i",
                        "pipe:0",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| io::Error::other("ffplay stdin missing"))?;
                let fd: OwnedFd = stdin.into();
                let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
                if flags < 0
                    || unsafe {
                        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                    } < 0
                {
                    let error = io::Error::last_os_error();
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                Ok(Sink {
                    writer: File::from(fd),
                    child: Some(child),
                })
            }
        }
    }
}

fn check_fd(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Video fd must be writable",
        ));
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let regular = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFREG;
    if !regular && flags & libc::O_NONBLOCK == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Video pipe/socket must already be nonblocking",
        ));
    }
    Ok(())
}

pub(crate) struct Sink {
    writer: File,
    child: Option<Child>,
}

impl Sink {
    pub fn write(&mut self, mut bytes: &[u8], stop: &AtomicBool) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !bytes.is_empty() {
            if stop.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Receiver stopped",
                ));
            }
            match self.writer.write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "Video output closed",
                    ));
                }
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Video output stalled",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
        if let Some(child) = &mut self.child
            && let Some(status) = child.try_wait()?
        {
            return Err(io::Error::other(format!("ffplay exited: {status}")));
        }
        Ok(())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Packet {
    pub kind: u16,
    pub timestamp: u64,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Framer {
    pending: Vec<u8>,
}

impl Framer {
    pub fn finish(&self) -> io::Result<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(invalid("Mirror stream closed during a packet"))
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.pending.len() + bytes.len() > MAX_PAYLOAD + 128 + 8192 {
            return Err(invalid("Mirror buffer limit exceeded"));
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    pub fn next(&mut self) -> io::Result<Option<Packet>> {
        if self.pending.len() < 128 {
            return Ok(None);
        }
        let size = u32::from_le_bytes(self.pending[..4].try_into().unwrap()) as usize;
        let kind = u16::from_le_bytes(self.pending[4..6].try_into().unwrap()) & 0xff;
        if size > MAX_PAYLOAD {
            return Err(invalid("Mirror payload too large"));
        }
        if kind > 2 {
            return Err(invalid("Unsupported mirror packet type"));
        }
        if kind == 2 && size != 0 {
            return Err(invalid("Heartbeat has a payload"));
        }
        if self.pending.len() < 128 + size {
            return Ok(None);
        }
        let packet = Packet {
            kind,
            timestamp: u64::from_le_bytes(self.pending[8..16].try_into().unwrap()),
            data: self.pending[128..128 + size].to_vec(),
        };
        self.pending.drain(..128 + size);
        Ok(Some(packet))
    }
}

#[derive(Default)]
pub(crate) struct Decoder {
    nal_length: Option<usize>,
}

impl Decoder {
    pub fn decode(&mut self, packet: Packet) -> io::Result<Vec<u8>> {
        match packet.kind {
            1 => {
                self.nal_length = None;
                let data = &packet.data;
                if data.len() < 7
                    || data[0] != 1
                    || data[4] & 0xfc != 0xfc
                    || data[5] & 0xe0 != 0xe0
                {
                    return Err(invalid("Invalid AVCDecoderConfigurationRecord"));
                }
                let length = (data[4] & 3) as usize + 1;
                if length == 3 {
                    return Err(invalid("Reserved AVC NAL length size"));
                }
                let mut cursor = 6;
                let mut output = Vec::new();
                let sps_count = data[5] & 31;
                if sps_count == 0 {
                    return Err(invalid("AVC configuration has no SPS"));
                }
                for _ in 0..sps_count {
                    read_parameter(data, &mut cursor, &mut output, 7)?;
                }
                let count = *data
                    .get(cursor)
                    .ok_or_else(|| invalid("Missing PPS count"))?;
                cursor += 1;
                if count == 0 {
                    return Err(invalid("AVC configuration has no PPS"));
                }
                for _ in 0..count {
                    read_parameter(data, &mut cursor, &mut output, 8)?;
                }
                self.nal_length = Some(length);
                Ok(output)
            }
            0 => {
                let length = self
                    .nal_length
                    .ok_or_else(|| invalid("Video arrived before codec configuration"))?;
                let mut data = packet.data.as_slice();
                let mut output = Vec::with_capacity(data.len() + 32);
                while !data.is_empty() {
                    if data.len() < length {
                        return Err(invalid("Truncated AVC NAL length"));
                    }
                    let size = data[..length]
                        .iter()
                        .fold(0usize, |n, byte| (n << 8) | *byte as usize);
                    data = &data[length..];
                    if size == 0 || size > data.len() {
                        return Err(invalid("Invalid AVC NAL size"));
                    }
                    if data[0] & 0x80 != 0 || data[0] & 31 == 0 || data[0] & 31 > 23 {
                        return Err(invalid("Invalid H.264 NAL header"));
                    }
                    output.extend_from_slice(&[0, 0, 0, 1]);
                    output.extend_from_slice(&data[..size]);
                    data = &data[size..];
                }
                Ok(output)
            }
            2 => Ok(Vec::new()),
            _ => Err(invalid("Unsupported mirror packet type")),
        }
    }
}

fn read_parameter(
    data: &[u8],
    cursor: &mut usize,
    output: &mut Vec<u8>,
    expected: u8,
) -> io::Result<()> {
    let length = data
        .get(*cursor..*cursor + 2)
        .ok_or_else(|| invalid("Truncated AVC parameter length"))?;
    let size = u16::from_be_bytes(length.try_into().unwrap()) as usize;
    *cursor += 2;
    let parameter = data
        .get(*cursor..*cursor + size)
        .ok_or_else(|| invalid("Truncated AVC parameter"))?;
    if parameter.is_empty() || parameter[0] & 0x9f != expected {
        return Err(invalid("Invalid AVC parameter NAL type"));
    }
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(parameter);
    *cursor += size;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn eof_rejects_partial_packet_headers_and_payloads() {
        for length in [1, 127, 128, 130] {
            let mut wire = [0; 131];
            wire[..4].copy_from_slice(&3u32.to_le_bytes());
            let mut framer = Framer::default();
            framer.push(&wire[..length]).unwrap();
            assert!(framer.next().unwrap().is_none());
            assert!(framer.finish().is_err());
        }
        Framer::default().finish().unwrap();
    }

    fn packet(kind: u16, data: &[u8]) -> Packet {
        Packet {
            kind,
            timestamp: 123,
            data: data.to_vec(),
        }
    }

    pub(crate) fn config() -> Vec<u8> {
        vec![
            1, 66, 0, 31, 255, 225, 0, 3, 0x67, 0x42, 0, 1, 0, 2, 0x68, 1,
        ]
    }

    #[test]
    fn mirror_header_is_little_endian_and_fragment_safe() {
        let mut wire = vec![0; 128];
        wire[..4].copy_from_slice(&3u32.to_le_bytes());
        wire[4..6].copy_from_slice(&0x101u16.to_le_bytes());
        wire[8..16].copy_from_slice(&0x1234567890abcdefu64.to_le_bytes());
        wire.extend_from_slice(&[1, 2, 3]);
        let mut framer = Framer::default();
        for byte in &wire[..wire.len() - 1] {
            framer.push(&[*byte]).unwrap();
            assert!(framer.next().unwrap().is_none());
        }
        framer.push(&wire[wire.len() - 1..]).unwrap();
        assert_eq!(
            framer.next().unwrap().unwrap(),
            Packet {
                kind: 1,
                timestamp: 0x1234567890abcdef,
                data: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn avcc_parameters_and_multiple_nals_become_annex_b() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder.decode(packet(1, &config())).unwrap(),
            [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 0, 1, 0x68, 1]
        );
        assert_eq!(
            decoder
                .decode(packet(0, &[0, 0, 0, 2, 0x65, 9, 0, 0, 0, 1, 0x61]))
                .unwrap(),
            [0, 0, 0, 1, 0x65, 9, 0, 0, 0, 1, 0x61]
        );
    }

    #[test]
    fn malformed_and_unconfigured_video_fails_without_panicking() {
        let mut decoder = Decoder::default();
        assert!(decoder.decode(packet(0, &[0, 0, 0, 1, 0x65])).is_err());
        for length in 0..config().len() {
            assert!(decoder.decode(packet(1, &config()[..length])).is_err());
        }
        decoder.decode(packet(1, &config())).unwrap();
        for data in [
            &[0, 0, 0, 9, 0x65][..],
            &[0, 0, 0, 0],
            &[0, 0],
            &[0, 0, 0, 1, 0xff],
        ] {
            assert!(decoder.decode(packet(0, data)).is_err());
        }
        let mut wire = vec![0; 128];
        wire[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut framer = Framer::default();
        framer.push(&wire).unwrap();
        assert!(framer.next().is_err());
    }

    #[test]
    fn caller_fd_stays_open_and_keeps_original_flags() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        assert!(VideoOutput::from_fd(writer.as_fd()).is_err());
        writer.set_nonblocking(true).unwrap();
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        {
            let output = VideoOutput::from_fd(writer.as_fd()).unwrap();
            let mut sink = output.open("test").unwrap();
            sink.write(b"h264", &AtomicBool::new(false)).unwrap();
        }
        assert_eq!(
            unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) },
            flags
        );
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"h264");
        (&writer).write_all(b"live").unwrap();
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"live");
    }

    #[test]
    fn cancelled_output_returns_without_waiting_for_a_reader() {
        let (writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let output = VideoOutput::from_fd(writer.as_fd()).unwrap();
        let mut sink = output.open("test").unwrap();
        assert_eq!(
            sink.write(b"data", &AtomicBool::new(true))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }
}
