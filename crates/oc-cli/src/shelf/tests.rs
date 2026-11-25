use super::*;
use oc_shelf::Actions as _;

fn device(hostname: &str, ip: &str) -> oc_core::Device {
    oc_core::Device {
        hostname: hostname.into(),
        dns_name: format!("{hostname}.test"),
        os: "linux".into(),
        ips: vec![ip.parse().unwrap()],
        online: true,
    }
}

#[test]
fn model_keeps_self_first_and_every_peer_without_discovery() {
    let mut offline = device("same", "fd7a::2");
    offline.online = false;
    let tailnet = oc_core::Tailnet {
        backend_state: "Running".into(),
        this_device: device("local", "100.64.0.1"),
        peers: vec![device("same", "100.64.0.2"), offline],
    };
    let model = model(&tailnet);
    assert_eq!(model.selected, 0);
    assert!(model.open_target.is_empty());
    assert_eq!(model.devices.len(), 3);
    assert!(model.devices[0].self_device);
    assert!(model.devices[1..].iter().all(|device| !device.self_device));
    assert_eq!(model.devices[2].ip, "-");
    assert!(!model.devices[2].online);
}

#[test]
fn duplicate_hostnames_handoff_to_the_exact_selected_ip() {
    let tailnet = oc_core::Tailnet {
        backend_state: "Running".into(),
        this_device: device("local", "100.64.0.1"),
        peers: vec![device("same", "100.64.0.2"), device("same", "100.64.0.3")],
    };
    let mut model = model(&tailnet);
    model.selected = 2;
    let selected = model.selected().unwrap();
    let plan = oc_handoff::plan_as(
        &tailnet,
        "michael",
        handoff_machine(selected).unwrap(),
        "a path/file.txt",
    )
    .unwrap();
    assert_eq!(plan.ssh_target, "michael@100.64.0.3");
    assert_eq!(plan.remote_command, ["xdg-open", "a path/file.txt"]);
    assert!(oc_handoff::plan_as(&tailnet, "michael", "same", "file").is_err());
    for ip in ["-", "127.0.0.1", "fd7a::1"] {
        let mut missing = selected.clone();
        missing.ip = ip.into();
        assert!(handoff_machine(&missing).is_err());
    }
}

fn channels() -> (Actions, mpsc::Receiver<(DeviceRow, Command)>) {
    let (sender, receiver) = mpsc::channel();
    let (updates_tx, updates) = mpsc::channel();
    let (devices_tx, devices) = mpsc::channel();
    (
        Actions {
            sender,
            updates,
            notifications: Notifications {
                updates: updates_tx,
                devices: devices_tx,
            },
            devices,
            frames: Frames::default(),
        },
        receiver,
    )
}

#[test]
fn dispatch_preserves_target_and_reports_worker_failure_to_the_ui() {
    let (mut actions, receiver) = channels();
    let row = MirrorDevice {
        udid: "phone".into(),
    }
    .row();
    let command = Command::Open("a path/$(literal);name.txt".into());
    actions.perform(&row, command.clone());
    assert_eq!(receiver.recv().unwrap(), (row.clone(), command.clone()));
    assert_eq!(actions.updates()[0].state, ActionState::Pending);
    drop(receiver);
    actions.perform(&row, command);
    let updates = actions.updates();
    assert_eq!(updates.last().unwrap().state, ActionState::Finished);
    assert!(
        updates
            .last()
            .unwrap()
            .result
            .as_ref()
            .unwrap_err()
            .contains("worker stopped")
    );
}

#[test]
fn window_failure_still_signals_and_joins_cleanup_and_keeps_both_errors() {
    let (actions, receiver) = channels();
    let (closed, shutdown) = watch::channel(false);
    let thread = std::thread::spawn(move || {
        while !*shutdown.borrow() {
            std::thread::yield_now();
        }
        assert!(receiver.recv().is_err());
        Err("cleanup failed".into())
    });
    let worker = Worker {
        actions,
        closed,
        thread,
    };
    let error = worker.finish(Err("window failed".into())).unwrap_err();
    assert!(error.contains("window failed"));
    assert!(error.contains("cleanup failed"));
}

#[tokio::test]
async fn close_cancels_pending_startup_and_drops_its_resources() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let cleanup = dropped.clone();
    let (closed, shutdown) = watch::channel(false);
    let (ready, started) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(until_closed(shutdown, async move {
        let _guard = Guard(cleanup);
        let _ = ready.send(());
        std::future::pending::<Result<(), String>>().await
    }));
    started.await.unwrap();
    closed.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn completed_or_failed_service_does_not_wait_for_ctrl_c() {
    assert!(crate::wait_for_shutdown(|| Ok(true)).await.is_ok());
    assert_eq!(
        crate::wait_for_shutdown(|| Err("service failed".into())).await,
        Err("service failed".into())
    );
}
