use std::collections::HashMap;
use std::pin::pin;
use std::sync::Mutex;

use ashpd::backend::file_chooser::FileChooserImpl;
use ashpd::backend::request::RequestImpl;
use ashpd::desktop::HandleToken;
use ashpd::desktop::file_chooser::{
    OpenFileOptions, SaveFileOptions, SaveFilesOptions, SelectedFiles,
};
use ashpd::{MaybeAppID, Uri, WindowIdentifierType, backend};
use async_trait::async_trait;
use color_eyre::Result;
use futures_util::future::{Either, select};
use tokio::process::Command;
use tokio::sync::oneshot;


const NAME: &str = "org.freedesktop.impl.portal.desktop.aw-fm";

struct Server {
    commands: Mutex<HashMap<HandleToken, oneshot::Sender<()>>>,
}

struct ClearOnDrop<'a>(&'a Server, HandleToken);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        let _unused = self.0.commands.lock().unwrap().remove(&self.1);
    }
}

#[async_trait]
impl RequestImpl for Server {
    async fn close(&self, token: HandleToken) {
        let mut commands = self.commands.lock().unwrap();
        if let Some(kill) = commands.remove(&token) {
            let _unused = kill.send(());
        }
        drop(commands);
        println!("IN Close(): {token}");
    }
}

#[async_trait]
impl FileChooserImpl for Server {
    async fn open_file(
        &self,
        token: HandleToken,
        app_id: Option<MaybeAppID>,
        window_identifier: Option<WindowIdentifierType>,
        title: &str,
        options: OpenFileOptions,
    ) -> backend::Result<SelectedFiles> {
        println!("open_file: {token}");
        println!("{window_identifier:?} {options:?}");

        let mut cmd = Command::new("aw-fm");
        if let Some(folder) = options.current_folder() {
            cmd.arg(folder.as_ref());
        }

        cmd.arg("open-file");

        if !title.is_empty() {
            cmd.arg("--title").arg(title);
        }

        if options.multiple() == Some(true) {
            cmd.arg("--multiple");
        }

        if options.directory() == Some(true) {
            cmd.arg("--directory");
        }

        if options.modal() == Some(true) {
            cmd.arg("--modal");
        }

        if let Some(label) = options.accept_label() {
            cmd.arg("--label").arg(label);
        }

        // TODO -- maybe useless
        match window_identifier {
            Some(WindowIdentifierType::X11(x)) => {
                cmd.arg("--parent-window").arg(format!("x11:{x}"));
            }
            Some(WindowIdentifierType::Wayland(w)) => {
                cmd.arg("--parent-window").arg(w);
            }
            None => {}
        }

        if let Some(app_id) = app_id {
            cmd.arg("--app-id").arg(app_id.to_string());
        }

        println!("OpenFile: {cmd:?}");
        cmd.kill_on_drop(true);
        let (sender, receiver) = oneshot::channel();

        self.commands.lock().unwrap().insert(token.clone(), sender);
        let _clean = ClearOnDrop(&self, token);

        let out = pin!(cmd.output());

        let out = match select(out, receiver).await {
            Either::Left((Ok(out), _)) => out,
            Either::Right((..)) => {
                return Err(ashpd::PortalError::Cancelled("Cancelled".to_owned()));
            }
            Either::Left((Err(e), _)) => {
                return Err(ashpd::PortalError::Failed(e.to_string()));
            }
        };

        // TODO -- ensure cancellation works
        if !out.status.success() {
            println!("stderr: {}", String::from_utf8_lossy(&out.stderr));
            return Err(ashpd::PortalError::Failed("todo".to_owned()));
        }

        // We expect this to be utf-8 encoded URIs
        let lines = match String::from_utf8(out.stdout) {
            Ok(good) => good,
            Err(e) => return Err(ashpd::PortalError::Failed(e.to_string())),
        };

        let uris: Vec<_> = lines.trim().lines().map(str::trim).filter(|s| !s.is_empty()).collect();
        if uris.is_empty() || (uris.len() == 1 && uris[0].eq_ignore_ascii_case("cancelled")) {
            println!("Cancelled");
            return Err(ashpd::PortalError::Cancelled("Cancelled".to_owned()));
        }

        let mut selected = SelectedFiles::default();
        for uri in uris {
            match Uri::parse(uri) {
                Ok(uri) => {
                    selected = selected.uri(uri);
                }
                Err(e) => return Err(ashpd::PortalError::Failed(e.to_string())),
            }
        }

        Ok(selected)
    }

    async fn save_file(
        &self,
        token: HandleToken,
        app_id: Option<MaybeAppID>,
        window_identifier: Option<WindowIdentifierType>,
        title: &str,
        options: SaveFileOptions,
    ) -> backend::Result<SelectedFiles> {
        println!("save_file: {token}");
        Err(ashpd::PortalError::NotFound("oh no".to_owned()))
    }

    async fn save_files(
        &self,
        token: HandleToken,
        app_id: Option<MaybeAppID>,
        window_identifier: Option<WindowIdentifierType>,
        title: &str,
        options: SaveFilesOptions,
    ) -> backend::Result<SelectedFiles> {
        println!("save_files: {token}");
        Err(ashpd::PortalError::NotFound("oh no".to_owned()))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    ashpd::backend::Builder::new(NAME)?
        .file_chooser(Server { commands: Mutex::default() })
        .build()
        .await?;

    // Never exit
    std::future::pending().await
}
