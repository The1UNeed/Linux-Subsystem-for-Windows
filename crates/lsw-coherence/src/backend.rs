//! Backend thread: the Coherence channel to the guest agent, and QEMU's
//! D-Bus display for input (absolute pointer and keyboard at the guest
//! window's position).

use lsw_daemon::display::{KeyboardProxy, MouseProxy};
use lsw_daemon::paths::VmPaths;
use lsw_daemon::qmp::Qmp;
use lsw_proto::coherence::CoMessage;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::time::Duration;
use tokio::sync::mpsc;

/// From the Wayland side.
#[derive(Debug)]
pub enum Request {
    /// A request for the guest agent.
    Guest(CoMessage),
    /// Pointer at guest desktop coordinates.
    Motion {
        x: u32,
        y: u32,
    },
    Button {
        button: u32,
        down: bool,
    },
    Key {
        qnum: u32,
        down: bool,
    },
}

/// To the Wayland side.
#[derive(Debug)]
pub enum Update {
    Guest(CoMessage),
    Disconnected(String),
}

/// Frames per second requested from the guest.
const MAX_FPS: u8 = 30;

pub fn spawn(paths: VmPaths, updates: Sender<Update>) -> mpsc::UnboundedSender<Request> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("lsw-coherence-backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run(paths, updates, rx));
        })
        .expect("backend thread");
    tx
}

async fn run(
    paths: VmPaths,
    updates: Sender<Update>,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    loop {
        let reason = match connection(&paths, &updates, &mut requests).await {
            Ok(()) => return, // the Wayland side is gone
            Err(e) => format!("{e:#}"),
        };
        if updates.send(Update::Disconnected(reason)).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn connection(
    paths: &VmPaths,
    updates: &Sender<Update>,
    requests: &mut mpsc::UnboundedReceiver<Request>,
) -> anyhow::Result<()> {
    let mut qmp = Qmp::connect_with_retry(&paths.ui_qmp_socket(), Duration::from_millis(500))
        .await
        .map_err(|_| anyhow::anyhow!("Windows is not running"))?;
    let display = lsw_daemon::display::connect(&mut qmp).await?;
    let keyboard = KeyboardProxy::new(&display).await?;
    let mouse = MouseProxy::new(&display).await?;
    let (mut reader, mut writer) =
        lsw_daemon::coherence::connect(&paths.coherence_socket(), Duration::from_secs(5)).await?;
    writer.send(&CoMessage::Start { max_fps: MAX_FPS }).await?;
    tracing::info!("Coherence started");
    loop {
        tokio::select! {
            msg = reader.recv() => {
                if updates.send(Update::Guest(msg?)).is_err() {
                    return Ok(());
                }
            }
            req = requests.recv() => {
                let Some(req) = req else {
                    let _ = writer.send(&CoMessage::Stop).await;
                    return Ok(());
                };
                match req {
                    Request::Guest(msg) => writer.send(&msg).await?,
                    Request::Motion { x, y } => mouse.set_abs_position(x, y).await?,
                    Request::Button { button, down } => {
                        tracing::debug!("button {button} {}", if down { "down" } else { "up" });
                        if down { mouse.press(button).await? } else { mouse.release(button).await? }
                    }
                    Request::Key { qnum, down } => {
                        if down { keyboard.press(qnum).await? } else { keyboard.release(qnum).await? }
                    }
                }
            }
            event = qmp.next_event() => { event?; }
        }
    }
}
