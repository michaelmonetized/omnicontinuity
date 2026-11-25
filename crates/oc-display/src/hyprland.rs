use std::{
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::Configuration;

static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(1);

pub fn command(arguments: &[&str]) -> Result<String> {
    let output = Command::new("hyprctl")
        .args(arguments)
        .output()
        .context("running hyprctl")?;
    let response = String::from_utf8(output.stdout)?;
    ensure!(
        output.status.success()
            || (arguments.contains(&"--help") && response.starts_with("usage:")),
        "hyprctl failed: {}{}",
        response,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(response)
}

fn monitors() -> Result<Vec<Value>> {
    Ok(serde_json::from_str(&command(&["-j", "monitors", "all"])?)?)
}

pub struct Output {
    pub name: String,
    pub configuration: Configuration,
    pub x: i32,
    pub y: i32,
    removed: bool,
}

impl Output {
    pub fn create(configuration: Configuration) -> Result<Self> {
        let help = command(&["output", "--help"])?;
        ensure!(
            help.contains("create") && help.contains("headless"),
            "Hyprland does not advertise headless output creation"
        );
        let existing = monitors()?;
        ensure!(
            existing.iter().any(|m| m["focused"] == true),
            "an active physical desktop is required"
        );
        let before: Value = serde_json::from_str(&command(&["-j", "activeworkspace"])?)?;
        let name = format!(
            "OC-DISPLAY-{}-{}",
            std::process::id(),
            NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed)
        );
        let result = command(&["output", "create", "headless", &name])?;
        ensure!(
            result.trim() == "ok",
            "headless output creation failed: {result}"
        );
        let mut output = Self {
            name,
            configuration,
            x: 0,
            y: 0,
            removed: false,
        };
        for _ in 0..40 {
            if monitors()?.iter().any(|m| m["name"] == output.name) {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        ensure!(
            monitors()?.iter().any(|m| m["name"] == output.name),
            "created output did not appear"
        );
        output.x = existing
            .iter()
            .map(|m| {
                let x = m["x"].as_f64().unwrap_or(0.0);
                let width = m["width"].as_f64().unwrap_or(0.0);
                let scale = m["scale"].as_f64().unwrap_or(1.0).max(0.1);
                (x + width / scale).ceil() as i32
            })
            .max()
            .unwrap_or(0);
        output.configure(configuration)?;
        let after: Value = serde_json::from_str(&command(&["-j", "activeworkspace"])?)?;
        ensure!(
            before["id"] == after["id"] && before["monitor"] == after["monitor"],
            "Hyprland changed the active workspace while creating the output; extension cancelled"
        );
        Ok(output)
    }

    pub fn configure(&mut self, configuration: Configuration) -> Result<()> {
        let code = format!(
            "hl.monitor({{ output = \"{}\", mode = \"{}x{}@{}\", position = \"{}x{}\", scale = {} }})",
            self.name,
            configuration.width,
            configuration.height,
            configuration.frames_per_second,
            self.x,
            self.y,
            configuration.scale
        );
        ensure!(
            command(&["eval", &code])?.trim() == "ok",
            "Hyprland rejected monitor configuration"
        );
        for _ in 0..40 {
            let all = monitors()?;
            if let Some(monitor) = all.iter().find(|m| {
                m["name"] == self.name
                    && m["width"] == configuration.width
                    && m["height"] == configuration.height
                    && m["scale"].as_f64() == Some(configuration.scale)
            }) {
                self.x = monitor["x"].as_i64().context("monitor x missing")? as i32;
                self.y = monitor["y"].as_i64().context("monitor y missing")? as i32;
                self.configuration = configuration;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        anyhow::bail!("Hyprland did not apply the requested display size")
    }

    pub fn pointer(&self, x: f64, y: f64) -> Result<()> {
        ensure!(
            x.is_finite() && y.is_finite(),
            "invalid pointer coordinates"
        );
        let x = self.x as f64
            + x.clamp(0.0, 1.0)
                * (self.configuration.width as f64 / self.configuration.scale - 1.0);
        let y = self.y as f64
            + y.clamp(0.0, 1.0)
                * (self.configuration.height as f64 / self.configuration.scale - 1.0);
        let code = format!(
            "hl.dsp.cursor.move({{ x = {}, y = {} }})",
            x.round() as i32,
            y.round() as i32
        );
        ensure!(
            command(&["dispatch", &code])?.trim() == "ok",
            "Hyprland rejected pointer input"
        );
        Ok(())
    }

    pub fn remove(&mut self) -> Result<()> {
        if !self.removed {
            let result = command(&["output", "remove", &self.name])?;
            ensure!(
                result.trim() == "ok" || result.contains("output not found"),
                "could not remove output: {result}"
            );
            self.removed = true;
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}
