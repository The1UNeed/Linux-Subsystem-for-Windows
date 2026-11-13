//! Backend thread: the Coherence channel to the guest agent, and QEMU's
//! D-Bus display for input (absolute pointer and keyboard at the guest
//! window's position).

use lsw_daemon::display::{KeyboardProxy, MouseProxy};
use lsw_daemon::paths::VmPaths;
use lsw_daemon::qmp::Qmp;
use lsw_proto::coherence::CoMessage;
use smithay_client_toolkit::reexports::calloop::channel::SyncSender as Sender;
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
    /// Number of mirrored windows changed (the supervisor pauses Windows
    /// when nothing is visible).
    WindowCount(u32),
}

/// To the Wayland side.
#[derive(Debug)]
pub enum Update {
    /// Boxed: queue entries stay small whatever the message size.
    Guest(Box<CoMessage>),
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

/// Ask the supervisor to resume Windows if it is idle-paused.
async fn wake(paths: &VmPaths) {
    let _ = lsw_daemon::api::call(
        &paths.api_socket(),
        &lsw_daemon::api::Request::Wake,
        Duration::from_secs(2),
    )
    .await;
}

async fn connection(
    paths: &VmPaths,
    updates: &Sender<Update>,
    requests: &mut mpsc::UnboundedReceiver<Request>,
) -> anyhow::Result<()> {
    let mut qmp =
        Qmp::connect_with_retry(&paths.coherence_qmp_socket(), Duration::from_millis(500))
            .await
            .map_err(|_| anyhow::anyhow!("Windows is not running"))?;
    let display = lsw_daemon::display::connect(&mut qmp).await?;
    let keyboard = KeyboardProxy::new(&display).await?;
    let mouse = MouseProxy::new(&display).await?;
    let (mut reader, mut writer) =
        lsw_daemon::coherence::connect(&paths.coherence_socket(), Duration::from_secs(5)).await?;
    wake(paths).await;
    writer.send(&CoMessage::Start { max_fps: MAX_FPS }).await?;
    tracing::info!("Coherence started");
    loop {
        tokio::select! {
            msg = reader.recv() => {
                // Bounded queue: when the Wayland side falls behind, this
                // blocks, the socket stops being read and the guest is
                // slowed down instead of filling host memory.
                let msg = msg?;
                if tokio::task::block_in_place(|| updates.send(Update::Guest(Box::new(msg)))).is_err() {
                    return Ok(());
                }
            }
            req = requests.recv() => {
                let Some(req) = req else {
                    let _ = writer.send(&CoMessage::Stop).await;
                    return Ok(());
                };
                match req {
                    Request::Guest(msg) => {
                        // Windows may be idle-paused (all windows were
                        // minimized): user actions need it running.
                        if matches!(
                            msg,
                            CoMessage::Activate { .. } | CoMessage::Restore { .. } | CoMessage::HitTest { .. }
                        ) {
                            wake(paths).await;
                        }
                        writer.send(&msg).await?
                    }
                    Request::Motion { x, y } => mouse.set_abs_position(x, y).await?,
                    Request::Button { button, down } => {
                        tracing::debug!("button {button} {}", if down { "down" } else { "up" });
                        if down {
                            wake(paths).await;
                            mouse.press(button).await?
                        } else {
                            mouse.release(button).await?
                        }
                    }
                    Request::WindowCount(count) => {
                        let api = paths.api_socket();
                        tokio::spawn(async move {
                            let _ = lsw_daemon::api::call(
                                &api,
                                &lsw_daemon::api::Request::Windows { count },
                                Duration::from_secs(2),
                            )
                            .await;
                        });
                    }
                    Request::Key { qnum, down } => {
                        if down {
                            wake(paths).await;
                            keyboard.press(qnum).await?
                        } else {
                            keyboard.release(qnum).await?
                        }
                    }
                }
            }
            event = qmp.next_event() => {
                // The agent restarted or Windows rebooted: the guest closed
                // or reopened its Coherence port. Its new engine waits for
                // Start, so drop this session (the Wayland side clears its
                // windows) and begin a new one.
                let event = event?;
                if event.name == "VSERPORT_CHANGE"
                    && event.data.get("id").and_then(|v| v.as_str())
                        == Some(lsw_daemon::qemu::COHERENCE_PORT_ID)
                {
                    anyhow::bail!("the guest agent restarted its Coherence port");
                }
            }
        }
    }
}
