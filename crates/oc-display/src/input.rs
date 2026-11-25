use anyhow::{Result, ensure};
use evdev::{
    AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
};
use serde_json::Value;

use crate::hyprland::Output;

#[derive(Default)]
pub struct Scroll {
    fractional: [f64; 2],
    discrete: [i32; 2],
}

impl Scroll {
    pub fn events(&mut self, dx: f64, dy: f64, scale: f64) -> Vec<InputEvent> {
        let mut events = Vec::new();
        for (axis, delta) in [-dx, dy].into_iter().enumerate() {
            self.fractional[axis] += delta.clamp(-10000.0, 10000.0) / scale * 8.0;
            let high = self.fractional[axis].trunc() as i32;
            self.fractional[axis] -= high as f64;
            self.discrete[axis] += high;
            let low = self.discrete[axis] / 120;
            self.discrete[axis] %= 120;
            let (hi_code, code) = if axis == 0 {
                (
                    RelativeAxisCode::REL_HWHEEL_HI_RES,
                    RelativeAxisCode::REL_HWHEEL,
                )
            } else {
                (
                    RelativeAxisCode::REL_WHEEL_HI_RES,
                    RelativeAxisCode::REL_WHEEL,
                )
            };
            if high != 0 {
                events.push(InputEvent::new(EventType::RELATIVE.0, hi_code.0, high));
            }
            if low != 0 {
                events.push(InputEvent::new(EventType::RELATIVE.0, code.0, low));
            }
        }
        events
    }
}

pub struct Input {
    device: VirtualDevice,
    scroll: Scroll,
    pressed: bool,
}

impl Input {
    pub fn new() -> Result<Self> {
        let device = VirtualDevice::builder()?
            .name("OpenDisplay pointer")
            .with_keys(&AttributeSet::from_iter([KeyCode::BTN_LEFT]))?
            .with_relative_axes(&AttributeSet::from_iter([
                RelativeAxisCode::REL_X,
                RelativeAxisCode::REL_Y,
                RelativeAxisCode::REL_WHEEL,
                RelativeAxisCode::REL_HWHEEL,
                RelativeAxisCode::REL_WHEEL_HI_RES,
                RelativeAxisCode::REL_HWHEEL_HI_RES,
            ]))?
            .build()?;
        Ok(Self {
            device,
            scroll: Scroll::default(),
            pressed: false,
        })
    }

    pub fn handle(&mut self, message: &Value, output: &Output) -> Result<()> {
        match message["type"].as_str() {
            Some("touch" | "pencil" | "proximity") => {
                let phase = message["phase"].as_str().unwrap_or("hover");
                if !matches!(
                    phase,
                    "began" | "moved" | "ended" | "cancelled" | "down" | "move" | "up" | "hover"
                ) {
                    return Ok(());
                }
                if let (Some(x), Some(y)) = (message["x"].as_f64(), message["y"].as_f64()) {
                    output.pointer(x, y)?;
                }
                let pressed = match phase {
                    "began" | "down" => true,
                    "ended" | "cancelled" | "up" => false,
                    _ => self.pressed,
                };
                if pressed != self.pressed {
                    self.device.emit(&[InputEvent::new(
                        EventType::KEY.0,
                        KeyCode::BTN_LEFT.0,
                        i32::from(pressed),
                    )])?;
                    self.pressed = pressed;
                }
            }
            Some("scroll") => {
                if let (Some(dx), Some(dy)) = (message["dx"].as_f64(), message["dy"].as_f64()) {
                    ensure!(dx.is_finite() && dy.is_finite(), "invalid scroll delta");
                    self.device
                        .emit(&self.scroll.events(dx, dy, output.configuration.scale))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        if self.pressed {
            let _ = self
                .device
                .emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 0)]);
        }
    }
}
