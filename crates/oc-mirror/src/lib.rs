//! iPhone display streaming. Requires existing trust, Developer Mode and a compatible
//! mounted developer image. This crate never pairs, mounts images, or handles passcodes.
//! USB is preferred; Wi-Fi uses an existing remote pairing record and endpoint.
//! FFmpeg with HEVC decoding must be on PATH; decoder tests also require libx265.
//!
//! ```no_run
//! # async fn example() -> oc_mirror::Result<()> {
//! let Some(device) = oc_mirror::list_devices().await?.into_iter().next() else { return Ok(()) };
//! let mut mirror = oc_mirror::start(device).await?;
//! let frame = mirror.next_frame().await?;
//! mirror.stop().await?;
//! # Ok(()) }
//! ```

use idevice::{
    IdeviceService, ReadWrite, RsdService,
    core_device::{
        CallInfoBlob, DisplayServiceClient, HevcDepacketizer, RtpPacket, build_frame_ack,
        build_keyframe_request, build_rctl, build_screen_audio_offer, build_screen_video_offer,
        build_start_audio_parameters, build_start_video_parameters, is_rtcp,
    },
    core_device_proxy::CoreDeviceProxy,
    remote_pairing::{
        RemotePairingClient, RpPairingFile, RpPairingSocket, connect_tls_psk_tunnel_native,
    },
    rsd::RsdHandshake,
    tcp::{
        adapter::Adapter,
        handle::{AdapterHandle, UdpSocketHandle},
    },
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice},
};
use std::{io, net::SocketAddr, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, timeout},
};

/// A mirror operation result, carrying transport, service, decoder or sink errors.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MEDIA_TIMEOUT: Duration = Duration::from_secs(15);

/// An existing Wi-Fi connection. `endpoint` is the phone's `_remotepairing._tcp`
/// address, and `pairing_record` is a pre-existing idevice remote pairing file.
#[derive(Debug, Clone)]
pub struct WifiConnection {
    pub endpoint: SocketAddr,
    pub pairing_record: PathBuf,
}

/// A discovered phone. Add `wifi` to enable fallback after USB failure.
#[derive(Debug, Clone)]
pub struct Device {
    pub udid: String,
    pub usbmux_id: Option<u32>,
    pub wifi: Option<WifiConnection>,
}

/// A decoded RGB24 frame. `pixels` has exactly `width * height * 3` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// A caller-owned frame destination. Errors stop forwarding and are returned.
pub trait FrameSink {
    /// Accept a frame. Takes one RGB frame; returns success or a destination error.
    fn frame(&mut self, frame: Frame) -> io::Result<()>;
}
impl<F: FnMut(Frame) -> io::Result<()>> FrameSink for F {
    fn frame(&mut self, frame: Frame) -> io::Result<()> {
        self(frame)
    }
}

/// A running mirror. Explicitly call `stop` to confirm the device-side stop request.
/// Dropping requests cleanup but cannot confirm the phone received it.
pub struct MirrorHandle {
    stop: watch::Sender<bool>,
    frames: mpsc::Receiver<Frame>,
    task: Option<JoinHandle<Result<()>>>,
}

impl MirrorHandle {
    /// Receive a decoded frame. Takes no arguments; returns a frame or the stream error.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>> {
        match self.frames.recv().await {
            Some(frame) => Ok(Some(frame)),
            None => {
                self.finish().await?;
                Ok(None)
            }
        }
    }

    /// Forward decoded frames. Takes the caller's sink; returns at stream end or error.
    pub async fn decode_into(&mut self, sink: &mut impl FrameSink) -> Result<()> {
        while let Some(frame) = self.next_frame().await? {
            if let Err(error) = sink.frame(frame) {
                let _ = self.stop().await;
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Request shutdown from another task. Takes no arguments; returns a cloneable signal.
    pub fn stop_signal(&self) -> watch::Sender<bool> {
        self.stop.clone()
    }

    /// Stop the stream. Takes no arguments; returns confirmed cleanup or an error.
    pub async fn stop(&mut self) -> Result<()> {
        let _ = self.stop.send(true);
        self.frames.close();
        self.finish().await
    }

    async fn finish(&mut self) -> Result<()> {
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}
impl Drop for MirrorHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// List phones without changing trust. Takes no arguments; returns USB phones first.
/// An absent usbmuxd socket or an empty device list returns an empty vector.
/// Network mux entries are omitted: Wi-Fi requires a separate remote pairing record.
pub async fn list_devices() -> Result<Vec<Device>> {
    list_devices_at(UsbmuxdAddr::from_env_var()?).await
}

async fn list_devices_at(addr: UsbmuxdAddr) -> Result<Vec<Device>> {
    let mut mux = match timeout(CONNECT_TIMEOUT, addr.connect(1)).await? {
        Ok(mux) => mux,
        Err(idevice::IdeviceError::Socket(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.into()),
    };
    let devices = timeout(CONNECT_TIMEOUT, mux.get_devices()).await??;
    Ok(usb_devices(devices))
}

fn usb_devices(devices: Vec<UsbmuxdDevice>) -> Vec<Device> {
    let mut phones: Vec<_> = devices
        .into_iter()
        .filter(|d| d.connection_type == Connection::Usb)
        .map(|d| Device {
            udid: d.udid,
            usbmux_id: Some(d.device_id),
            wifi: None,
        })
        .collect();
    phones.sort_by(|a, b| a.udid.cmp(&b.udid));
    phones.dedup_by(|a, b| a.udid == b.udid);
    phones
}

/// Start mirroring. Takes a discovered USB phone or an explicitly configured Wi-Fi
/// phone; returns a handle with `next_frame`, `decode_into` and `stop`.
/// USB is tried first. Only existing Wi-Fi credentials are verified on fallback.
pub async fn start(device: Device) -> Result<MirrorHandle> {
    let (mut adapter, port) = timeout(CONNECT_TIMEOUT, connect(&device)).await??;
    let setup = async {
        let mut rsd = RsdHandshake::new(adapter.connect(port).await?).await?;
        let client = DisplayServiceClient::connect_rsd(&mut adapter, &mut rsd)
            .await
            .map_err(|error| format!(
                "CoreDevice display service com.apple.coredevice.displayservice connection failed: {error}. Mirroring requires Developer Mode enabled and a compatible Apple developer image mounted on the device."
            ))?;
        let audio = adapter.bind_udp(0).await?;
        let video = adapter.bind_udp(0).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((client, audio, video))
    };
    let (mut client, audio, video) = match timeout(CONNECT_TIMEOUT, setup).await {
        Ok(Ok(session)) => session,
        result => {
            let _ = adapter.close().await;
            return Err(match result {
                Ok(Err(e)) => e,
                Err(e) => e.into(),
                _ => unreachable!(),
            });
        }
    };
    let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let negotiated = timeout(
        CONNECT_TIMEOUT,
        negotiate(&mut client, &adapter, &audio, &video, ssrc),
    )
    .await;
    if !matches!(negotiated, Ok(Ok(()))) {
        let _ = timeout(Duration::from_secs(5), client.stop_media_stream()).await;
        let _ = adapter.close().await;
        return Err(match negotiated {
            Ok(Err(e)) => e,
            Err(e) => e.into(),
            _ => unreachable!(),
        });
    }
    let (stop, stop_rx) = watch::channel(false);
    let (frames_tx, frames) = mpsc::channel(2);
    let task = tokio::spawn(async move {
        let result = receive(&video, &audio, ssrc, stop_rx, frames_tx).await;
        let stopped = timeout(Duration::from_secs(5), client.stop_media_stream()).await;
        let _ = adapter.close().await;
        match stopped {
            Ok(Ok(_)) => result,
            Ok(Err(error)) => Err(format!(
                "device shutdown unconfirmed: {error}; stream result: {result:?}"
            )
            .into()),
            Err(_) => Err("device shutdown unconfirmed: stop request timed out".into()),
        }
    });
    Ok(MirrorHandle {
        stop,
        frames,
        task: Some(task),
    })
}

async fn connect(device: &Device) -> Result<(AdapterHandle, u16)> {
    let usb_error = if let Some(id) = device.usbmux_id {
        let mux = UsbmuxdDevice {
            connection_type: Connection::Usb,
            udid: device.udid.clone(),
            device_id: id,
        };
        let provider = mux.to_provider(UsbmuxdAddr::from_env_var()?, "oc-mirror");
        match timeout(Duration::from_secs(10), CoreDeviceProxy::connect(&provider)).await {
            Ok(Ok(proxy)) => {
                let port = proxy.tunnel_info().server_rsd_port;
                return Ok((proxy.create_software_tunnel()?.to_async_handle(), port));
            }
            Ok(Err(e)) => Some(format!(
                "USB CoreDevice developer service connection failed: {e}. Mirroring requires Developer Mode enabled and a compatible developer image already mounted."
            )),
            Err(_) => Some("USB connection timed out".into()),
        }
    } else {
        None
    };
    if let Some(wifi) = &device.wifi {
        let mut record = RpPairingFile::read_from_file(&wifi.pairing_record).await?;
        let socket = tokio::net::TcpStream::connect(wifi.endpoint).await?;
        let mut remote = RemotePairingClient::new(RpPairingSocket::new(socket), "oc-mirror");
        remote.attempt_pair_verify().await?;
        remote.validate_pairing(&mut record).await?;
        let port = remote.create_tcp_listener().await?;
        let stream =
            tokio::net::TcpStream::connect(SocketAddr::new(wifi.endpoint.ip(), port)).await?;
        let tunnel = connect_tls_psk_tunnel_native(stream, remote.encryption_key()).await?;
        let rsd_port = tunnel.info.server_rsd_port;
        let host = tunnel.info.client_address.parse()?;
        let peer = tunnel.info.server_address.parse()?;
        let mtu = tunnel.info.mtu as usize;
        let mut adapter = Adapter::new(Box::new(tunnel.into_inner()), host, peer);
        adapter.set_mss(mtu.saturating_sub(60));
        return Ok((adapter.to_async_handle(), rsd_port));
    }
    Err(usb_error
        .unwrap_or_else(|| "no USB connection or existing Wi-Fi configuration".into())
        .into())
}

async fn negotiate(
    client: &mut DisplayServiceClient<Box<dyn ReadWrite>>,
    adapter: &AdapterHandle,
    audio: &UdpSocketHandle,
    video: &UdpSocketHandle,
    ssrc: u32,
) -> Result<()> {
    let call = CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "Mac17,7".into(),
        framework_version: "2205.3.1".into(),
        os_version: "25F71".into(),
        device_name: None,
        audio_device_uid: None,
    };
    let session = uuid::Uuid::new_v4();
    let host = adapter.host_ip().to_string();
    let peer = adapter.peer_ip().to_string();
    let offer = build_screen_audio_offer(&uuid::Uuid::new_v4().to_string().to_uppercase(), &call)?;
    client
        .start_media_stream(build_start_audio_parameters(
            &host,
            audio.local_port(),
            &peer,
            50000,
            offer,
            140,
            session,
        ))
        .await?;
    let offer = build_screen_video_offer(
        &uuid::Uuid::new_v4().to_string().to_uppercase(),
        &call,
        ssrc,
    )?;
    client
        .start_media_stream(build_start_video_parameters(
            &host,
            video.local_port(),
            &peer,
            50001,
            offer,
            140,
            1,
            session,
        ))
        .await?;
    Ok(())
}

async fn receive(
    video: &UdpSocketHandle,
    audio: &UdpSocketHandle,
    ssrc: u32,
    mut stop: watch::Receiver<bool>,
    frames: mpsc::Sender<Frame>,
) -> Result<()> {
    let mut decoder = spawn_decoder()?;
    let mut input = decoder.stdin.take().ok_or("decoder input unavailable")?;
    let output = decoder.stdout.take().ok_or("decoder output unavailable")?;
    let mut reader = tokio::spawn(async move {
        let mut output = BufReader::new(output);
        while let Some(frame) = read_frame(&mut output).await? {
            frames
                .send(frame)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "frame sink closed"))?;
        }
        Ok::<(), io::Error>(())
    });
    let result = async {
        let mut hevc = HevcDepacketizer::new();
        let mut feedback = tokio::time::interval(Duration::from_millis(50));
        let epoch = Instant::now();
        let mut last_packet = Instant::now();
        let mut highest_seq = 0;
        let mut count = 0u16;
        let mut remote_port = None;
        let mut media_ssrc = None;
        let mut fir = 0u8;
        loop {
            if *stop.borrow() { break; }
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                decoded = &mut reader => {
                    decoded??;
                    return Err("HEVC decoder ended before the media stream".into());
                }
                _ = feedback.tick() => {
                    if last_packet.elapsed() > MEDIA_TIMEOUT { return Err("phone stopped sending video".into()); }
                    if let Some(port) = remote_port {
                        let mut report = build_rctl(ssrc, epoch.elapsed().as_millis() as u16, count, highest_seq);
                        if !hevc.has_parameter_sets() && let Some(media) = media_ssrc {
                            report.extend(build_keyframe_request(ssrc, "oc-mirror", media, &[], fir));
                            fir = fir.wrapping_add(1);
                        }
                        video.send_to(port, report).await?;
                    }
                }
                packet = audio.recv() => { packet?; }
                packet = video.recv() => {
                    let packet = packet?;
                    if is_rtcp(&packet.data) { continue; }
                    let Some(rtp) = RtpPacket::parse(&packet.data) else { continue };
                    if rtp.payload_type != 100 { continue; }
                    last_packet = Instant::now();
                    remote_port = Some(packet.source_port);
                    media_ssrc = Some(rtp.ssrc);
                    highest_seq = rtp.sequence_number;
                    hevc.push(rtp.sequence_number, rtp.timestamp, rtp.payload);
                    let bytes = hevc.take_output();
                    if !bytes.is_empty() {
                        tokio::select! {
                            _ = stop.changed() => break,
                            written = timeout(Duration::from_secs(5), input.write_all(&bytes)) => { written??; }
                        }
                    }
                    if rtp.marker {
                        count = count.wrapping_add(1);
                        video.send_to(packet.source_port, build_frame_ack(ssrc, rtp.timestamp)).await?;
                    }
                }
            }
        }
        Ok(())
    }.await;
    reader.abort();
    let _ = decoder.kill().await;
    let _ = decoder.wait().await;
    result
}

fn spawn_decoder() -> io::Result<tokio::process::Child> {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "2",
            "-probesize",
            "32768",
            "-analyzeduration",
            "0",
            "-flags",
            "low_delay",
            "-f",
            "hevc",
            "-i",
            "pipe:0",
            "-an",
            "-threads",
            "2",
            "-f",
            "image2pipe",
            "-vcodec",
            "ppm",
            "-pix_fmt",
            "rgb24",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Frame>> {
    let mut header = String::new();
    if reader.read_line(&mut header).await? == 0 {
        return Ok(None);
    }
    if header != "P6\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid decoder frame magic",
        ));
    }
    header.clear();
    reader.take(128).read_line(&mut header).await?;
    let dims: Vec<_> = header.split_whitespace().map(str::parse::<u32>).collect();
    let [Ok(width), Ok(height)] = dims.as_slice() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame size",
        ));
    };
    let size = frame_size(*width, *height)?;
    let (width, height) = (*width, *height);
    header.clear();
    reader.take(16).read_line(&mut header).await?;
    if header != "255\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid RGB depth",
        ));
    }
    let mut pixels = vec![0; size];
    reader.read_exact(&mut pixels).await?;
    Ok(Some(Frame {
        width,
        height,
        pixels,
    }))
}

fn frame_size(width: u32, height: u32) -> io::Result<usize> {
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame dimensions outside 1..8192",
        ));
    }
    Ok(width as usize * height as usize * 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_phone_is_empty_and_usb_is_deduplicated() {
        assert!(usb_devices(vec![]).is_empty());
        let usb = UsbmuxdDevice {
            connection_type: Connection::Usb,
            udid: "phone".into(),
            device_id: 1,
        };
        let network = UsbmuxdDevice {
            connection_type: Connection::Network("127.0.0.1".parse().unwrap()),
            ..usb.clone()
        };
        assert_eq!(usb_devices(vec![network, usb.clone(), usb]).len(), 1);
    }
    #[tokio::test]
    async fn reads_rgb_frames_with_binary_newlines_and_size_changes() {
        let bytes = b"P6\n1 1\n255\n\x00\x0a\xffP6\n2 1\n255\nabcdef";
        let mut reader = BufReader::new(&bytes[..]);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap().pixels,
            [0, 10, 255]
        );
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap().width, 2);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn rejects_truncated_and_oversized_frames() {
        for data in [
            b"P6\n1 1\n255\na".as_slice(),
            b"P6\n99999 1\n255\n",
            b"P6\n0 1\n255\n",
            b"P5\n1 1\n255\nabc",
        ] {
            assert!(read_frame(&mut BufReader::new(data)).await.is_err());
        }
    }
    #[tokio::test]
    async fn missing_transport_fails_without_pairing() {
        let device = Device {
            udid: "absent".into(),
            usbmux_id: None,
            wifi: None,
        };
        assert!(start(device).await.is_err());
    }

    #[tokio::test]
    async fn absent_daemon_returns_empty() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(
            list_devices_at(UsbmuxdAddr::TcpSocket(addr))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn usbmux_empty_list_needs_no_phone() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let daemon = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 16];
            socket.read_exact(&mut header).await.unwrap();
            let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let mut request = vec![0; length - 16];
            socket.read_exact(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request).contains("ListDevices"));
            let body = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>DeviceList</key><array/></dict></plist>";
            header[..4].copy_from_slice(&((body.len() + 16) as u32).to_le_bytes());
            socket.write_all(&header).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        assert!(
            list_devices_at(UsbmuxdAddr::TcpSocket(addr))
                .await
                .unwrap()
                .is_empty()
        );
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn real_hevc_decoder_delivers_rgb_to_caller_sink() {
        timeout(Duration::from_secs(15), async {
            let encoded = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red:size=64x32:rate=1",
                    "-frames:v",
                    "10",
                    "-c:v",
                    "libx265",
                    "-threads",
                    "1",
                    "-bf",
                    "0",
                    "-x265-params",
                    "pools=none:frame-threads=1:log-level=error",
                    "-f",
                    "hevc",
                    "pipe:1",
                ])
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                encoded.status.success(),
                "{}",
                String::from_utf8_lossy(&encoded.stderr)
            );
            let mut decoder = spawn_decoder().unwrap();
            let mut input = decoder.stdin.take().unwrap();
            let bytes = encoded.stdout;
            let (finish, hold_open) = tokio::sync::oneshot::channel();
            let writer = tokio::spawn(async move {
                input.write_all(&bytes).await.unwrap();
                let _ = hold_open.await;
                input.shutdown().await.unwrap();
            });
            let mut output = BufReader::new(decoder.stdout.take().unwrap());
            let mut count = 0;
            let mut sink = |frame: Frame| {
                assert_eq!((frame.width, frame.height), (64, 32));
                assert_eq!(frame.pixels.len(), 64 * 32 * 3);
                assert!(frame.pixels[0] > 240 && frame.pixels[1] < 10 && frame.pixels[2] < 10);
                count += 1;
                Ok(())
            };
            let first = timeout(Duration::from_secs(3), read_frame(&mut output))
                .await
                .expect("decoder must deliver frames before input EOF")
                .unwrap()
                .unwrap();
            sink.frame(first).unwrap();
            let _ = finish.send(());
            while let Some(frame) = read_frame(&mut output).await.unwrap() {
                sink.frame(frame).unwrap();
            }
            writer.await.unwrap();
            assert!(decoder.wait().await.unwrap().success());
            assert_eq!(count, 10);
        })
        .await
        .unwrap();
    }
}
