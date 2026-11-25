use super::*;
use std::os::unix::fs::{DirBuilderExt, symlink};

pub(crate) struct TestDirectory(pub PathBuf);

impl TestDirectory {
    pub(crate) fn new() -> Self {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/i")
            .join(&suffix[..8]);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }

    pub(crate) fn paths(&self) -> Paths {
        Paths::in_directory(&self.0)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn service(paths: &Paths, outcome: Outcome) -> crate::ClipboardSync {
    let Bindings {
        relay,
        control,
        guard,
    } = bind(paths).unwrap();
    let (shutdown, mut requested) = watch::channel(false);
    let cancel = shutdown.clone();
    let (completed, finished) = watch::channel(None);
    let task = tokio::spawn(async move {
        let (ready, receiver) = watch::channel(None);
        let control_task = tokio::spawn(serve(control, cancel, receiver));
        while !*requested.borrow() {
            requested.changed().await.unwrap();
        }
        drop(relay);
        drop(guard);
        ready.send_replace(Some(outcome.clone()));
        control_task.await.unwrap().unwrap();
        completed.send_replace(Some(outcome.clone()));
        outcome.map_err(anyhow::Error::msg)
    });
    crate::ClipboardSync {
        shutdown,
        finished,
        task: Some(task),
    }
}

#[tokio::test]
async fn stop_acknowledgement_waits_for_worker_and_socket_cleanup() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let Bindings {
        relay,
        control,
        guard,
    } = bind(&paths).unwrap();
    assert_eq!(
        std::fs::metadata(&paths.relay).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&paths.control).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&paths.lock).unwrap().mode() & 0o777,
        0o600
    );
    let (cancel, mut requested) = watch::channel(false);
    let (ready, completion) = watch::channel(None);
    let control = tokio::spawn(serve(control, cancel, completion));
    assert!(is_running(&paths).await);
    assert!(!*requested.borrow());
    let destination = paths.clone();
    let stop = tokio::spawn(async move { super::stop(&destination).await });
    timeout(Duration::from_secs(1), requested.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(!stop.is_finished());
    assert!(paths.relay.exists());
    tokio::task::yield_now().await;
    assert!(!stop.is_finished());
    let mut concurrent = UnixStream::connect(&paths.control).await.unwrap();
    concurrent.write_all(STOP).await.unwrap();
    let mut status = UnixStream::connect(&paths.control).await.unwrap();
    status.write_all(STATUS).await.unwrap();
    let mut reply = [0; 4];
    timeout(Duration::from_secs(1), status.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, STOPPED);
    assert!(
        timeout(Duration::from_millis(10), concurrent.read_u8())
            .await
            .is_err()
    );
    drop(relay);
    drop(guard);
    assert!(!paths.relay.exists());
    assert!(!paths.control.exists());
    ready.send_replace(Some(Ok(())));
    timeout(Duration::from_secs(1), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(1), concurrent.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, DONE);
    control.await.unwrap().unwrap();
    assert!(!is_running(&paths).await);
    super::stop(&paths).await.unwrap();
}

#[tokio::test]
async fn cleanup_unlocks_even_when_a_fork_style_descriptor_survives() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let binding = bind(&paths).unwrap();
    let inherited = binding.guard.lock.try_clone().unwrap();
    drop(binding);
    assert!(!paths.relay.exists());
    assert!(!paths.control.exists());
    super::stop(&paths).await.unwrap();
    let replacement = bind(&paths).unwrap();
    drop(inherited);
    assert!(bind(&paths).is_err());
    drop(replacement);
    super::stop(&paths).await.unwrap();
}

#[tokio::test]
async fn control_errors_are_returned_to_the_stopping_invocation() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let mut service = service(&paths, Err("worker cleanup failed".into()));
    let error = super::stop(&paths).await.unwrap_err();
    assert!(error.to_string().contains("worker cleanup failed"));
    assert!(service.task.take().unwrap().await.unwrap().is_err());
}

#[tokio::test]
async fn malformed_control_request_cannot_stop_service() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let mut service = service(&paths, Ok(()));
    let mut socket = UnixStream::connect(&paths.control).await.unwrap();
    socket.write_all(b"OCC1").await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), socket.read_u8())
            .await
            .unwrap()
            .unwrap_err()
            .kind(),
        ErrorKind::UnexpectedEof
    );
    assert!(!*service.shutdown.borrow());
    assert!(is_running(&paths).await);
    super::stop(&paths).await.unwrap();
    service.task.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test]
async fn active_collisions_and_legacy_listeners_are_preserved() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let binding = bind(&paths).unwrap();
    let inode = std::fs::metadata(&paths.relay).unwrap().ino();
    assert!(bind(&paths).is_err());
    assert_eq!(std::fs::metadata(&paths.relay).unwrap().ino(), inode);
    drop(binding);
    let legacy = std::os::unix::net::UnixListener::bind(&paths.relay).unwrap();
    assert!(bind(&paths).is_err());
    assert!(std::os::unix::net::UnixStream::connect(&paths.relay).is_ok());
    assert!(
        super::stop(&paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("legacy")
    );
    drop(legacy);
}

#[tokio::test]
async fn stale_sockets_recover_but_files_and_symlinks_are_not_replaced() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    drop(std::os::unix::net::UnixListener::bind(&paths.relay).unwrap());
    drop(std::os::unix::net::UnixListener::bind(&paths.control).unwrap());
    let binding = bind(&paths).unwrap();
    drop(binding);
    std::fs::write(&paths.control, b"unrelated contents").unwrap();
    assert!(bind(&paths).is_err());
    assert_eq!(
        std::fs::read(&paths.control).unwrap(),
        b"unrelated contents"
    );
    assert!(!paths.relay.exists());
    std::fs::remove_file(&paths.control).unwrap();
    let target = directory.0.join("unrelated");
    std::fs::write(&target, b"preserved").unwrap();
    symlink(&target, &paths.control).unwrap();
    assert!(bind(&paths).is_err());
    assert!(
        std::fs::symlink_metadata(&paths.control)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(target).unwrap(), b"preserved");
}

#[tokio::test]
async fn cleanup_never_removes_a_replacement_endpoint() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let binding = bind(&paths).unwrap();
    std::fs::remove_file(&paths.relay).unwrap();
    let replacement = UnixListener::bind(&paths.relay).unwrap();
    let inode = std::fs::metadata(&paths.relay).unwrap().ino();
    drop(binding);
    assert_eq!(std::fs::metadata(&paths.relay).unwrap().ino(), inode);
    assert!(UnixStream::connect(&paths.relay).await.is_ok());
    drop(replacement);
}

#[test]
#[ignore = "subprocess harness"]
fn child_control_service() {
    let path = std::env::var_os("OC_CLIPBOARD_CONTROL_TEST_DIRECTORY")
        .expect("subprocess harness requires its private test directory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut service = service(&Paths::in_directory(Path::new(&path)), Ok(()));
        timeout(Duration::from_secs(10), service.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    });
}

#[tokio::test]
async fn another_process_can_query_and_stop_the_owning_service() {
    let directory = TestDirectory::new();
    let paths = directory.paths();
    let executable = std::env::current_exe().unwrap();
    let mut child = tokio::process::Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "ipc::tests::child_control_service",
            "--nocapture",
        ])
        .env("OC_CLIPBOARD_CONTROL_TEST_DIRECTORY", &directory.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while !is_running(&paths).await {
            assert!(
                child.try_wait().unwrap().is_none(),
                "control test subprocess exited before readiness"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    super::stop(&paths).await.unwrap();
    assert!(!paths.relay.exists());
    assert!(!paths.control.exists());
    assert!(
        timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
