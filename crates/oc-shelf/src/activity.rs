use crate::{Command, DeviceRow};

/// An action's current lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionState {
    Pending,
    Running,
    Finished,
}

/// Progress or an outcome for one action and its original device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub device: DeviceRow,
    pub command: Command,
    pub state: ActionState,
    pub result: Result<String, String>,
}

impl Update {
    /// Describe an action's state.
    /// Takes this update and returns visible text, including the original peer IP and any error.
    pub fn text(&self) -> String {
        let state = match self.state {
            ActionState::Pending => "starting",
            ActionState::Running => "running",
            ActionState::Finished if self.result.is_err() => "failed",
            ActionState::Finished => "finished",
        };
        let message = match &self.result {
            Ok(message) | Err(message) => message,
        };
        format!(
            "{} · {} ({}): {state} · {message}",
            self.command.label(),
            self.device.hostname,
            self.device.ip
        )
    }
}

/// Latest action outcomes, retained while the selected row changes.
#[derive(Default)]
pub struct Activity {
    updates: Vec<Update>,
    errors: Vec<Update>,
}

impl Activity {
    /// Record progress or an outcome.
    /// Takes one update and replaces the same device/action entry, keeping recent outcomes last.
    pub fn apply(&mut self, update: Update) {
        if update.result.is_err() && self.errors.last() != Some(&update) {
            self.errors.push(update.clone());
            if self.errors.len() > 16 {
                self.errors.remove(0);
            }
        }
        if let Some(index) = self
            .updates
            .iter()
            .position(|old| old.device == update.device && old.command.same_action(&update.command))
        {
            self.updates.remove(index);
        }
        self.updates.push(update);
    }

    /// Read visible activity.
    /// Takes this history and returns the most recent update for each device/action pair.
    pub fn updates(&self) -> &[Update] {
        &self.updates
    }

    /// Read retained failures.
    /// Takes this history and returns recent errors, preserving them across later running updates.
    pub fn errors(&self) -> &[Update] {
        &self.errors
    }

    /// Check whether an action can start.
    /// Takes a command and returns false while its worker or process-wide service is active.
    pub fn available(&self, command: &Command) -> bool {
        !self.updates.iter().any(|update| {
            update.command.same_action(command) && update.state != ActionState::Finished
        })
    }
}
