use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Child;
use tokio::time::timeout;

/// Reap a service-owned SSH child.
///
/// Takes its child handle and graceful exit deadline. After pipes close, waits
/// for normal exit, then terminates and reaps only that same child if needed.
/// Returns only after wait succeeds; no PID lookup or process group signal is used.
pub(crate) async fn finish(child: &mut Child, grace: Duration) -> Result<()> {
    match timeout(grace, child.wait()).await {
        Ok(result) => {
            result.context("reap owned SSH child")?;
        }
        Err(_) => terminate(child).await?,
    }
    Ok(())
}

/// Terminate an unresponsive owned child.
///
/// Takes an unreaped child handle and returns after that exact process is
/// terminated and reaped. Never targets a master connection or another PID.
pub(crate) async fn terminate(child: &mut Child) -> Result<()> {
    child
        .kill()
        .await
        .context("terminate and reap owned clipboard child")
}

pub(crate) struct PendingChild(Option<Child>);

impl PendingChild {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("owned pending child")
    }

    /// Relinquish termination responsibility after successful ownership handoff.
    ///
    /// Takes the guard and returns its child without signalling it. Only an
    /// acknowledged selection keeper may outlive the clipboard sync service.
    pub fn release(mut self) -> Child {
        self.0.take().expect("owned pending child")
    }
}

impl Drop for PendingChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::process::Command;

    #[tokio::test]
    async fn stalled_owned_child_is_reaped_without_touching_another_child() {
        let mut sentinel = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owned = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        finish(&mut owned, Duration::from_millis(10)).await.unwrap();
        assert!(owned.id().is_none());
        assert!(owned.try_wait().unwrap().is_some());
        assert!(sentinel.try_wait().unwrap().is_none());
        drop(sentinel.stdin.take());
        assert!(sentinel.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn failed_keeper_is_reaped_and_acknowledged_keeper_can_outlive_guard() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let mut pending = PendingChild::new(child);
        terminate(pending.child()).await.unwrap();
        assert!(pending.child().id().is_none());
        drop(pending.release());

        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut acknowledged = PendingChild::new(child).release();
        assert!(acknowledged.try_wait().unwrap().is_none());
        drop(acknowledged.stdin.take());
        assert!(acknowledged.wait().await.unwrap().success());
    }
}
