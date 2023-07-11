use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process;

use serde_json::{json, Value};
use tokio::{pin, select};

use super::Manager;
use crate::closing;
use crate::com::CommandResponder;


impl Manager {
    pub(super) fn execute(
        &self,
        cmd: String,
        gui_env: Vec<(String, OsString)>,
        resp: Option<CommandResponder>,
    ) {
        // TODO -- get_env() in place of new() for actions
        tokio::task::spawn_local(execute(cmd, Vec::new(), resp));
    }
}

#[cfg(target_family = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

async fn execute(cmdstr: String, env: Vec<(String, OsString)>, resp: Option<CommandResponder>) {
    let mut m = serde_json::Map::new();
    let mut cmd = tokio::process::Command::new(cmdstr.clone());

    #[cfg(target_family = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let cmd = cmd.envs(env).spawn();

    // https://github.com/rust-lang/rust/issues/48594
    #[allow(clippy::never_loop)]
    'outer: loop {
        let cmd = match cmd {
            Ok(cmd) => cmd,
            Err(e) => {
                m.insert(
                    "error".into(),
                    format!("Executable {cmdstr} failed to start with error {e:?}").into(),
                );
                break 'outer;
            }
        };

        let fut = cmd.wait_with_output();
        pin!(fut);
        let output = select! {
            output = &mut fut => output,
            _ = closing::closed_fut() => {
                warn!("Waiting to exit until external command completes: {cmdstr}");
                drop(fut.await);
                warn!("Command blocking exit completed: {cmdstr}");
                return;
            },
        };


        match output {
            Ok(output) => {
                if output.status.success() {
                    return;
                }
                m.insert(
                    "error".into(),
                    format!("Executable {} exited with error code {:?}", cmdstr, output.status)
                        .into(),
                );
                m.insert("stdout".to_string(), String::from_utf8_lossy(&output.stdout).into());
                m.insert("stderr".to_string(), String::from_utf8_lossy(&output.stderr).into());
            }
            Err(e) => {
                m.insert(
                    "error".into(),
                    format!("Executable {} failed to start with error {:?}", cmdstr, e).into(),
                );
            }
        }

        break;
    }

    let m = Value::Object(m);
    error!("{:?}", m);
    if let Some(resp) = resp {
        drop(resp.send(m));
    }
}
