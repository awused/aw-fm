use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::{pin, select};

use super::Manager;
use crate::closing;
use crate::com::GuiAction;


impl Manager {
    pub(super) fn execute(&self, cmd: Arc<Path>, gui_env: Vec<(String, OsString)>) {
        let cmd = match prep_command(cmd, gui_env, false) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("{e}");
                return drop(self.gui_sender.send(GuiAction::ConveyError(e)));
            }
        };

        tokio::task::spawn_local(run(cmd, self.gui_sender.clone(), true));
    }

    pub(super) fn script(&self, cmd: Arc<Path>, gui_env: Vec<(String, OsString)>) {
        let cmd = match prep_command(cmd, gui_env, true) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("{e}");
                return drop(self.gui_sender.send(GuiAction::ConveyError(e)));
            }
        };

        tokio::task::spawn_local(run_with_output(cmd, self.gui_sender.clone()));
    }

    pub(super) fn launch(&self, cmd: Arc<Path>, gui_env: Vec<(String, OsString)>) {
        let cmd = match prep_command(cmd, gui_env, false) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("{e}");
                return drop(self.gui_sender.send(GuiAction::ConveyError(e)));
            }
        };

        tokio::task::spawn_local(run(cmd, self.gui_sender.clone(), false));
    }
}

fn prep_command(
    cmd: Arc<Path>,
    env: Vec<(String, OsString)>,
    kill_on_drop: bool,
) -> Result<Command, String> {
    if let Ok(canon) = cmd.canonicalize() {
        if canon.is_relative() {
            return Err(format!("Relative paths are not allowed, got: {cmd:?}"));
        }

        let mut cmd = tokio::process::Command::new(&*canon);
        cmd.envs(env).kill_on_drop(kill_on_drop);
        Ok(cmd)
    } else {
        Err(format!("Could not get canonical path for {cmd:?}"))
    }
}

async fn run_with_output(mut cmd: Command, gui_chan: UnboundedSender<GuiAction>) {
    let fut = cmd.output();

    pin!(fut);
    let output = select! {
        output = &mut fut => output,
        _ = closing::closed_fut() => {
            warn!("Waiting to exit for up to 60 seconds until external command completes: {cmd:?}");
            drop(tokio::time::timeout(Duration::from_secs(60), fut).await);
            return warn!("Command blocking exit completed or killed: {cmd:?}");
        },
    };

    match output {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);

                for line in stdout.trim().lines() {
                    info!("Running command from script: {line}");
                    // It's possible to get the responses and include them in the JSON output,
                    // but probably unnecessary. This also doesn't wait for any slow/interactive
                    // commands to finish.
                    drop(gui_chan.send(GuiAction::Action(line.to_string())));
                }

                return;
            }

            let msg = format!("Executable {cmd:?} exited with error code {:?}", output.status);
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));

            info!("stdout: {:?}", String::from_utf8_lossy(&output.stdout));
            warn!("stderr: {:?}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => {
            let msg = format!("Executable {cmd:?} failed to start with error {e}");
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));
        }
    }
}

async fn run(mut cmd: Command, gui_chan: UnboundedSender<GuiAction>, convey_errors: bool) {
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        // Always convey this error
        Err(e) => {
            let msg = format!("Failed to launch {:?}: {e}", cmd.as_std().get_program());
            error!("{msg}");
            return drop(gui_chan.send(GuiAction::ConveyError(msg)));
        }
    };

    if !convey_errors {
        return;
    }

    let fut = child.wait();
    pin!(fut);
    let status = select! {
        status = &mut fut => status,
        _ = closing::closed_fut() => {
            warn!("Waiting to exit for up to 60 seconds until external command completes: {cmd:?}");
            drop(tokio::time::timeout(Duration::from_secs(60), fut).await);
            return warn!("Command blocking exit completed or killed: {cmd:?}");
        },
    };

    match status {
        Ok(status) => {
            if status.success() {
                return;
            }

            let msg = format!("Executable {cmd:?} exited with error code {:?}", status.code());
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));
        }
        Err(e) => {
            let msg = format!("Executable {cmd:?} failed to start with error {e}");
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));
        }
    }
}
