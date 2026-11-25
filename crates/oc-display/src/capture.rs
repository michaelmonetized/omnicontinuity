use std::{cell::RefCell, io::Cursor, os::fd::OwnedFd, rc::Rc, time::Duration};

use anyhow::{Context, Result, ensure};
use pipewire::{self as pw, properties::properties, spa};
use spa::{
    param::{
        ParamType,
        format::{FormatProperties, MediaSubtype, MediaType},
        video::{VideoFormat, VideoInfoRaw},
    },
    pod::Pod,
};

use crate::Configuration;

pub struct Frame {
    pub rgb: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub captured_at: u64,
}

struct State {
    format: VideoInfoRaw,
    latest: Option<Frame>,
    error: Option<String>,
}

pub struct Capture {
    _core_listener: pw::core::Listener,
    _listener: pw::stream::StreamListener<Rc<RefCell<State>>>,
    stream: pw::stream::StreamRc,
    main_loop: pw::main_loop::MainLoopRc,
    state: Rc<RefCell<State>>,
    node: u32,
}

impl Capture {
    pub fn new(remote: OwnedFd, node: u32, configuration: Configuration) -> Result<Self> {
        pw::init();
        let main_loop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&main_loop, None)?;
        let core = context.connect_fd_rc(remote, None)?;
        let state = Rc::new(RefCell::new(State {
            format: VideoInfoRaw::new(),
            latest: None,
            error: None,
        }));
        let core_state = state.clone();
        let core_listener = core
            .add_listener_local()
            .error(move |_, _, _, message| {
                core_state.borrow_mut().error =
                    Some(format!("PipeWire connection error: {message}"));
            })
            .register();
        let stream = pw::stream::StreamRc::new(
            core,
            "OpenDisplay monitor",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )?;
        let listener = stream
            .add_local_listener_with_user_data(state.clone())
            .state_changed(|_, state, _, new| {
                if let pw::stream::StreamState::Error(error) = new {
                    state.borrow_mut().error = Some(error.to_string());
                }
            })
            .param_changed(|_, state, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                if let Some(param) = param {
                    let mut state = state.borrow_mut();
                    if let Err(error) = state.format.parse(param) {
                        state.error = Some(format!("invalid PipeWire video format: {error:?}"));
                    }
                }
            })
            .process(|stream, state| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let mut state = state.borrow_mut();
                let format = state.format;
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                if size == 0 {
                    return;
                }
                let stride = data.chunk().stride();
                let Some(bytes) = data.data() else {
                    state.error = Some("PipeWire supplied an unmapped video buffer".into());
                    return;
                };
                let result = packed_rgb(
                    bytes,
                    offset,
                    size,
                    stride,
                    format.size().width as usize,
                    format.size().height as usize,
                    format.format(),
                );
                match result {
                    Ok(rgb) => {
                        state.latest = Some(Frame {
                            rgb,
                            width: format.size().width as usize,
                            height: format.size().height as usize,
                            captured_at: crate::now_ms(),
                        })
                    }
                    Err(error) => state.error = Some(error.to_string()),
                }
            })
            .register()?;
        let capture = Self {
            _core_listener: core_listener,
            _listener: listener,
            stream,
            main_loop,
            state,
            node,
        };
        capture.connect(configuration)?;
        Ok(capture)
    }

    pub fn reconfigure(&mut self, configuration: Configuration) -> Result<()> {
        self.stream.disconnect()?;
        *self.state.borrow_mut() = State {
            format: VideoInfoRaw::new(),
            latest: None,
            error: None,
        };
        self.connect(configuration)
    }

    fn connect(&self, configuration: Configuration) -> Result<()> {
        let format = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(
                FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::BGRx,
                VideoFormat::BGRx,
                VideoFormat::BGRA,
                VideoFormat::RGBx,
                VideoFormat::RGBA
            ),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                spa::utils::Rectangle {
                    width: configuration.width,
                    height: configuration.height
                }
            ),
            spa::pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                spa::utils::Fraction {
                    num: configuration.frames_per_second,
                    denom: 1
                },
                spa::utils::Fraction { num: 0, denom: 1 },
                spa::utils::Fraction {
                    num: configuration.frames_per_second,
                    denom: 1
                }
            )
        );
        let bytes = spa::pod::serialize::PodSerializer::serialize(
            Cursor::new(Vec::new()),
            &spa::pod::Value::Object(format),
        )
        .map_err(|e| anyhow::anyhow!("could not serialize capture format: {e:?}"))?
        .0
        .into_inner();
        self.stream.connect(
            spa::utils::Direction::Input,
            Some(self.node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [Pod::from_bytes(&bytes).context("invalid capture format pod")?],
        )?;
        Ok(())
    }

    pub fn next(&self) -> Result<Option<Frame>> {
        self.main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        let mut state = self.state.borrow_mut();
        if let Some(error) = state.error.take() {
            anyhow::bail!("{error}");
        }
        Ok(state.latest.take())
    }
}

pub fn packed_rgb(
    bytes: &[u8],
    offset: usize,
    size: usize,
    stride: i32,
    width: usize,
    height: usize,
    format: VideoFormat,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0 && width <= 32768 && height <= 32768,
        "invalid capture dimensions"
    );
    ensure!(
        stride > 0 && stride as usize >= width * 4,
        "invalid capture stride"
    );
    let required = (height - 1)
        .checked_mul(stride as usize)
        .and_then(|n| n.checked_add(width * 4))
        .context("capture size overflow")?;
    ensure!(required <= size, "truncated PipeWire chunk");
    let pixels = bytes
        .get(
            offset
                ..offset
                    .checked_add(required)
                    .context("capture offset overflow")?,
        )
        .context("capture exceeds mapped buffer")?;
    let bgr = format == VideoFormat::BGRx || format == VideoFormat::BGRA;
    ensure!(
        bgr || format == VideoFormat::RGBx || format == VideoFormat::RGBA,
        "unsupported capture pixel format"
    );
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in pixels.chunks(stride as usize).take(height) {
        for pixel in row[..width * 4].as_chunks::<4>().0 {
            if bgr {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            } else {
                rgb.extend_from_slice(&pixel[..3]);
            }
        }
    }
    Ok(rgb)
}
