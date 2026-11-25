use super::*;

fn phone(udid: &str) -> oc_mirror::Device {
    oc_mirror::Device {
        udid: udid.into(),
        usbmux_id: Some(1),
        wifi: None,
    }
}

#[test]
fn explicit_phone_selection_never_substitutes_the_first_device() {
    assert_eq!(
        select_mirror(vec![phone("first"), phone("second")], "second")
            .unwrap()
            .udid,
        "second"
    );
    assert!(select_mirror(vec![phone("first")], "missing").is_err());
    assert!(select_mirror(Vec::new(), "missing").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_stream_failure_is_visible_and_releases_its_running_slot() {
    let runtime = Handle::current();
    let task = runtime.spawn(async { Err("stream failed".into()) });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    let (updates, receiver) = mpsc::channel();
    let (devices, _) = mpsc::channel();
    let (_closed, shutdown) = watch::channel(false);
    let (stop, _) = watch::channel(false);
    let device = MirrorDevice {
        udid: "chosen".into(),
    }
    .row();
    let expected = device.clone();
    let worker = std::thread::spawn(move || {
        let mut services = Services::new(
            runtime,
            None,
            shutdown,
            Notifications { updates, devices },
            Frames::default(),
        );
        services.mirror = Some(Owned {
            device,
            handle: Mirror { stop, task },
        });
        services.poll();
        assert!(services.mirror.is_none());
        services.stop().unwrap();
    });
    worker.join().unwrap();
    let update = receiver.recv().unwrap();
    assert_eq!(update.device, expected);
    assert_eq!(update.state, ActionState::Finished);
    assert_eq!(update.result, Err("stream failed".into()));
    let mut activity = oc_shelf::Activity::default();
    activity.apply(update);
    assert!(activity.available(&Command::MirrorStart("retry".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_joins_owned_stream_and_propagates_cleanup_failure() {
    let runtime = Handle::current();
    let (stop, mut stopped) = watch::channel(false);
    let task = runtime.spawn(async move {
        stopped
            .wait_for(|stopped| *stopped)
            .await
            .map_err(|error| error.to_string())?;
        Err("device cleanup failed".into())
    });
    let (updates, receiver) = mpsc::channel();
    let (devices, _) = mpsc::channel();
    let (_closed, shutdown) = watch::channel(false);
    let worker = std::thread::spawn(move || {
        let mut services = Services::new(
            runtime,
            None,
            shutdown,
            Notifications { updates, devices },
            Frames::default(),
        );
        services.mirror = Some(Owned {
            device: MirrorDevice {
                udid: "chosen".into(),
            }
            .row(),
            handle: Mirror { stop, task },
        });
        services.stop()
    });
    assert_eq!(worker.join().unwrap(), Err("device cleanup failed".into()));
    assert!(receiver.recv().unwrap().result.is_err());
}
