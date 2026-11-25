use super::*;

fn frame(udid: &str, width: u32, height: u32, pixels: &[u8]) -> MirrorFrame {
    MirrorFrame {
        udid: udid.into(),
        width,
        height,
        pixels: pixels.to_vec(),
    }
}

#[test]
fn rejects_invalid_rgb_dimensions_and_lengths_before_texture_upload() {
    for frame in [
        frame("p", 0, 1, &[]),
        frame("p", 1, 0, &[]),
        frame("p", 2, 1, &[1, 2, 3]),
        frame("p", u32::MAX, u32::MAX, &[]),
        frame("p", 8192, 8192, &[]),
    ] {
        assert!(frame.size().is_err());
    }
    assert_eq!(frame("p", 1, 1, &[1, 2, 3]).size().unwrap(), [1, 1]);
}

#[test]
fn bounded_delivery_keeps_only_the_newest_frame_and_can_reset_streams() {
    let frames = Frames::default();
    for value in 0..100 {
        frames.push(frame("chosen", 1, 1, &[value, 0, 0])).unwrap();
    }
    assert_eq!(frames.take().unwrap().unwrap().pixels, [99, 0, 0]);
    assert!(frames.take().unwrap().is_none());
    frames.push(frame("old", 1, 1, &[1, 2, 3])).unwrap();
    frames.clear().unwrap();
    assert!(frames.take().unwrap().is_none());
    frames.push(frame("new", 1, 1, &[4, 5, 6])).unwrap();
    assert!(frames.push(frame("bad", 0, 0, &[])).is_err());
    assert_eq!(frames.take().unwrap().unwrap().udid, "new");
}

#[test]
fn rgb_frames_upload_exact_pixels_and_update_the_same_texture() {
    let ctx = egui::Context::default();
    let mut video = Video::default();
    let output = ctx.run(egui::RawInput::default(), |ctx| {
        video
            .receive(
                ctx,
                Ok(Some(frame("chosen", 2, 1, &[255, 0, 0, 0, 255, 0]))),
            )
            .unwrap();
        egui::CentralPanel::default().show(ctx, |ui| video.show(ui, 200.0));
    });
    let texture = video.texture.as_ref().unwrap().id();
    let delta = output
        .textures_delta
        .set
        .iter()
        .find(|(id, _)| *id == texture)
        .unwrap();
    let egui::ImageData::Color(image) = &delta.1.image;
    assert_eq!(image.size, [2, 1]);
    assert_eq!(image.pixels, [egui::Color32::RED, egui::Color32::GREEN]);
    let output = ctx.run(egui::RawInput::default(), |ctx| {
        video
            .receive(ctx, Ok(Some(frame("chosen", 1, 1, &[0, 0, 255]))))
            .unwrap();
    });
    assert_eq!(video.texture.as_ref().unwrap().id(), texture);
    let delta = output
        .textures_delta
        .set
        .iter()
        .find(|(id, _)| *id == texture)
        .unwrap();
    let egui::ImageData::Color(image) = &delta.1.image;
    assert_eq!(image.size, [1, 1]);
    assert_eq!(image.pixels, [egui::Color32::BLUE]);
}

#[test]
fn multiple_attached_phones_require_an_explicit_choice_and_keep_valid_selection() {
    let mut phones = Phones::default();
    let first = MirrorDevice {
        udid: "first".into(),
    };
    let second = MirrorDevice {
        udid: "second".into(),
    };
    phones.replace(vec![first.clone(), second.clone()]);
    assert!(phones.selected.is_none());
    phones.selected = Some("second".into());
    phones.replace(vec![second.clone(), first]);
    assert_eq!(phones.selected.as_deref(), Some("second"));
    phones.replace(Vec::new());
    assert!(phones.selected.is_none());
    phones.replace(vec![second]);
    assert_eq!(phones.selected.as_deref(), Some("second"));
}
