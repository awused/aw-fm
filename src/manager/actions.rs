use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use gtk::glib;
use tokio::{pin, select};

use super::Manager;
use crate::closing;
use crate::com::GuiAction;


impl Manager {
    pub(super) fn execute(&self, cmd: String, gui_env: Vec<(String, OsString)>) {
        tokio::task::spawn_local(execute(cmd, gui_env, self.gui_sender.clone(), false));
    }

    pub(super) fn script(&self, cmd: String, gui_env: Vec<(String, OsString)>) {
        tokio::task::spawn_local(execute(cmd, gui_env, self.gui_sender.clone(), true));
    }
}

#[cfg(target_family = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

async fn execute(
    cmdstr: String,
    env: Vec<(String, OsString)>,
    gui_chan: glib::Sender<GuiAction>,
    run_output: bool,
) {
    let p: &Path = Path::new(cmdstr.as_str());
    let mut comp = p.components();
    if comp.next().is_some() && comp.next().is_some() {
        if let Ok(canon) = p.canonicalize() {
            if !canon.is_absolute() {
                let msg = format!("Relative paths are not allowed, got: {cmdstr}");
                error!("{msg}");
                drop(gui_chan.send(GuiAction::ConveyError(msg)));
                return;
            }
        } else {
            let msg = format!("Could not get canonical path for {cmdstr}");
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));
            return;
        }
    }

    let mut cmd = tokio::process::Command::new(cmdstr.clone());

    #[cfg(target_family = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let fut = cmd.envs(env).kill_on_drop(run_output).output();

    pin!(fut);
    let output = select! {
        output = &mut fut => output,
        _ = closing::closed_fut() => {
            warn!("Waiting to exit for up to 60 seconds until external command completes: {cmdstr}");
            if run_output {
                drop(tokio::time::timeout(Duration::from_secs(60), fut).await);
            }
            warn!("Command blocking exit completed or killed: {cmdstr}");
            return;
        },
    };


    match output {
        Ok(output) => {
            if output.status.success() {
                if !run_output {
                    return;
                }

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

            let msg = format!("Executable {cmdstr} exited with error code {:?}", output.status);
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));

            info!("stdout: {:?}", String::from_utf8_lossy(&output.stdout));
            warn!("stderr: {:?}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => {
            let msg = format!("Executable {cmdstr} failed to start with error {e:?}");
            error!("{msg}");
            drop(gui_chan.send(GuiAction::ConveyError(msg)));
        }
    }
}
