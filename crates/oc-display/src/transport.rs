use std::{
    ffi::CString,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    os::{fd::FromRawFd, unix::net::UnixStream},
    time::Duration,
};

use anyhow::{Context, Result, ensure};

use crate::{Target, protocol::PORT};

pub enum Connection {
    Wifi(TcpStream),
    Usb(UnixStream),
}

impl Connection {
    pub fn connect(target: &Target) -> Result<Self> {
        let connection = match target {
            Target::Wifi(address) => {
                let socket = TcpStream::connect_timeout(
                    &SocketAddr::new(*address, PORT),
                    Duration::from_secs(5),
                )?;
                socket.set_nodelay(true)?;
                Self::Wifi(socket)
            }
            Target::Usb { udid } => {
                let udid = CString::new(udid.as_str())?;
                let mut info = DeviceInfo {
                    handle: 0,
                    product_id: 0,
                    udid: [0; 44],
                    connection_type: 0,
                    connection_data: [0; 200],
                };
                let found = unsafe { usbmuxd_get_device(udid.as_ptr(), &mut info, 2) };
                ensure!(found == 1, "USB device not found (usbmuxd result {found})");
                let fd = unsafe { usbmuxd_connect(info.handle, PORT) };
                ensure!(
                    fd >= 0,
                    "usbmuxd could not connect to port {PORT}: {}",
                    std::io::Error::from_raw_os_error(-fd)
                );
                Self::Usb(unsafe { UnixStream::from_raw_fd(fd) })
            }
        };
        connection.timeouts(Duration::from_secs(5), Duration::from_secs(2))?;
        Ok(connection)
    }

    pub fn try_clone(&self) -> Result<Self> {
        Ok(match self {
            Self::Wifi(socket) => Self::Wifi(socket.try_clone()?),
            Self::Usb(socket) => Self::Usb(socket.try_clone()?),
        })
    }

    pub fn shutdown(&self) {
        let _ = match self {
            Self::Wifi(socket) => socket.shutdown(Shutdown::Both),
            Self::Usb(socket) => socket.shutdown(Shutdown::Both),
        };
    }

    fn timeouts(&self, read: Duration, write: Duration) -> Result<()> {
        match self {
            Self::Wifi(socket) => {
                socket.set_read_timeout(Some(read))?;
                socket.set_write_timeout(Some(write))?;
            }
            Self::Usb(socket) => {
                socket.set_read_timeout(Some(read))?;
                socket.set_write_timeout(Some(write))?;
            }
        }
        Ok(())
    }
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Wifi(socket) => socket.read(buffer),
            Self::Usb(socket) => socket.read(buffer),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Wifi(socket) => socket.write(buffer),
            Self::Usb(socket) => socket.write(buffer),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Wifi(socket) => socket.flush(),
            Self::Usb(socket) => socket.flush(),
        }
    }
}

#[repr(C)]
struct DeviceInfo {
    handle: u32,
    product_id: u32,
    udid: [std::ffi::c_char; 44],
    connection_type: std::ffi::c_int,
    connection_data: [u8; 200],
}

#[link(name = "usbmuxd-2.0")]
unsafe extern "C" {
    fn usbmuxd_get_device(
        udid: *const std::ffi::c_char,
        info: *mut DeviceInfo,
        options: std::ffi::c_int,
    ) -> std::ffi::c_int;
    fn usbmuxd_connect(handle: u32, port: u16) -> std::ffi::c_int;
}

pub fn hello(socket: &mut Connection) -> Result<serde_json::Value> {
    loop {
        let payload = crate::protocol::read_frame(socket).context("waiting for receiver hello")?;
        let Some(hello) = crate::protocol::control(&payload) else {
            continue;
        };
        ensure!(hello["type"] == "hello", "receiver must send hello first");
        return Ok(hello);
    }
}
