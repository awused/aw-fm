use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

use gtk::glib;
use serde_json::{json, Value};
use tokio::{pin, select};

use super::Manager;
use crate::closing;
use crate::com::GuiAction;


impl Manager {
    pub(super) fn execute(&self, cmd: String, gui_env: Vec<(String, OsString)>) {
        tokio::task::spawn_local(execute(cmd, gui_env, None));
    }

    pub(super) fn script(&self, cmd: String, gui_env: Vec<(String, OsString)>) {
        tokio::task::spawn_local(execute(cmd, gui_env, Some(self.gui_sender.clone())));
    }
}

#[cfg(target_family = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

async fn execute(
    cmdstr: String,
    env: Vec<(String, OsString)>,
    gui_chan: Option<glib::Sender<GuiAction>>,
) {
    let mut m = serde_json::Map::new();
    let mut cmd = tokio::process::Command::new(cmdstr.clone());

    #[cfg(target_family = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let fut = cmd.envs(env).kill_on_drop(true).output();

    pin!(fut);
    let output = select! {
        output = &mut fut => output,
        _ = closing::closed_fut() => {
            warn!("Waiting to exit for up to 60 seconds until external command completes: {cmdstr}");
            drop(tokio::time::timeout(Duration::from_secs(60), fut).await);
            warn!("Command blocking exit completed or killed: {cmdstr}");
            return;
        },
    };


    match output {
        Ok(output) => {
            if output.status.success() {
                let Some(gui_chan) = gui_chan else {
                    return;
                };

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
            m.insert(
                "error".into(),
                format!("Executable {cmdstr} exited with error code {:?}", output.status).into(),
            );
            m.insert("stdout".to_string(), String::from_utf8_lossy(&output.stdout).into());
            m.insert("stderr".to_string(), String::from_utf8_lossy(&output.stderr).into());
        }
        Err(e) => {
            m.insert(
                "error".into(),
                format!("Executable {cmdstr} failed to start with error {e:?}").into(),
            );
        }
    }

    let m = Value::Object(m);
    error!("{m:?}");
    // TODO -- convey error instead of building json object
    // if let Some(resp) = resp {
    //     drop(resp.send(m));
    // }
}
