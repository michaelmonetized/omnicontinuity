use std::io::{Read, Write};

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

pub const VERSION: u64 = 3;
pub const PORT: u16 = 9000;
pub const MAX_CONTROL: usize = (1 << 20) - 1;

/// Selected desktop and video size.
/// Derived from a receiver hello and intersected decoder limits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Configuration {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub scale: f64,
}

impl Configuration {
    /// Choose video settings.
    /// `hello` supplies panel facts and decoder limits. Returns an even H.264
    /// raster capped at 1920 pixels per axis and 30 FPS for software encoding.
    pub fn from_hello(hello: &Value) -> Result<Self> {
        ensure!(hello["type"] == "hello", "receiver must send hello first");
        let version = hello.get("pv").and_then(Value::as_u64).unwrap_or(1);
        ensure!(
            (1..=VERSION).contains(&version),
            "unsupported receiver protocol {version}"
        );
        let panel = hello.get("panel").filter(|panel| {
            dimension(panel, "pixelsWide").is_some()
                && dimension(panel, "pixelsHigh").is_some()
                && panel["scale"]
                    .as_f64()
                    .is_some_and(|v| v.is_finite() && v > 0.0)
                && match (panel.get("pointsWide"), panel.get("pointsHigh")) {
                    (None, None) => true,
                    (Some(_), Some(_)) => {
                        dimension(panel, "pointsWide").is_some()
                            && dimension(panel, "pointsHigh").is_some()
                    }
                    _ => false,
                }
        });
        let facts = panel.unwrap_or(hello);
        let width =
            dimension(facts, "pixelsWide").ok_or_else(|| anyhow::anyhow!("invalid panel width"))?;
        let height = dimension(facts, "pixelsHigh")
            .ok_or_else(|| anyhow::anyhow!("invalid panel height"))?;
        let scale = if panel.is_some_and(|p| p["scale"].as_f64().unwrap() >= 1.5) {
            2.0
        } else if panel.is_some() {
            1.0
        } else {
            2.0
        };
        let rate = limit(hello, "displayMaxFrameRate", 60).min(30);
        let envelopes: Vec<&Value> = match hello.get("videoCaps").and_then(Value::as_array) {
            Some(caps) => caps.iter().filter(|v| v["codec"] == "h264").collect(),
            None => vec![&Value::Null],
        };
        ensure!(!envelopes.is_empty(), "receiver does not offer H.264");
        envelopes
            .into_iter()
            .filter_map(|cap| {
                let max_w = limit(cap, "maxWidth", 1920)
                    .min(limit(hello, "maxEncodeWide", 1920))
                    .min(1920);
                let max_h = limit(cap, "maxHeight", 1920)
                    .min(limit(hello, "maxEncodeHigh", 1920))
                    .min(1920);
                let pixel_rate = cap["maxPixelsPerSecond"]
                    .as_u64()
                    .filter(|v| *v > 0)
                    .unwrap_or(u64::MAX);
                let desired_fps = rate.min(limit(cap, "maxFrameRate", 30));
                let factor = (max_w as f64 / width as f64)
                    .min(max_h as f64 / height as f64)
                    .min(
                        (pixel_rate as f64 / (width as f64 * height as f64 * desired_fps as f64))
                            .sqrt(),
                    )
                    .min(1.0);
                let w = ((width as f64 * factor) as u32) & !1;
                let h = ((height as f64 * factor) as u32) & !1;
                if w < 2 || h < 2 {
                    return None;
                }
                let fps = desired_fps
                    .min((pixel_rate / (w as u64 * h as u64)).min(u32::MAX as u64) as u32);
                (fps > 0).then_some(Self {
                    width: w,
                    height: h,
                    frames_per_second: fps,
                    scale,
                })
            })
            .max_by_key(|c| c.width as u64 * c.height as u64 * c.frames_per_second as u64)
            .ok_or_else(|| anyhow::anyhow!("no supported H.264 operating point"))
    }

    /// Announce video settings.
    /// Consumes a copied configuration and returns a v3 streamConfig object.
    pub fn message(self) -> Value {
        json!({"type":"streamConfig", "codec":"h264", "width":self.width,
            "height":self.height, "framesPerSecond":self.frames_per_second})
    }
}

fn dimension(value: &Value, key: &str) -> Option<u32> {
    value[key]
        .as_u64()
        .filter(|n| (2..=32768).contains(n))
        .map(|n| n as u32)
}

fn limit(value: &Value, key: &str, fallback: u32) -> u32 {
    value[key]
        .as_u64()
        .filter(|n| *n > 0)
        .map(|n| n.min(u32::MAX as u64) as u32)
        .unwrap_or(fallback)
}

/// Read a receiver frame.
/// `reader` supplies a byte stream. Returns one bounded payload or a framing error.
pub fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_CONTROL).contains(&length),
        "invalid control frame length {length}"
    );
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

/// Send a complete frame.
/// `writer` receives a big-endian length and `payload`. Returns after both are written.
pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len())?;
    ensure!(length > 0, "empty wire frame");
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(payload)?;
    Ok(())
}

/// Send a control message.
/// `writer` receives `message` as JSON. Returns an error if v3 demux rules fail.
pub fn write_control(writer: &mut impl Write, message: &Value) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    ensure!(
        payload.len() < 32768 && payload.first() == Some(&b'{') && !payload.contains(&0),
        "control violates v3 demux"
    );
    write_frame(writer, &payload)
}

/// Parse receiver control.
/// `payload` contains one frame. Returns a typed JSON object or None to ignore it.
pub fn control(payload: &[u8]) -> Option<Value> {
    let message: Value = serde_json::from_slice(payload).ok()?;
    message["type"].as_str()?;
    Some(message)
}

/// Normalize encoded video.
/// `nalus` contains one Annex B access unit. Returns four-byte start codes throughout.
pub fn annex_b(nalus: &[u8]) -> Result<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= nalus.len() {
        let len = if nalus[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if nalus[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, len));
        i += len;
    }
    ensure!(
        !starts.is_empty() && starts[0].0 == 0,
        "encoder did not return Annex B"
    );
    let mut output = Vec::with_capacity(nalus.len() + starts.len());
    for (n, &(start, length)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(nalus.len(), |v| v.0);
        if start + length >= end {
            bail!("empty NAL unit");
        }
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&nalus[start + length..end]);
    }
    Ok(output)
}
