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
    // Retries while Windows is off fail the same way every time: report a
    // reason once until a session starts again.
    let mut reported: Option<String> = None;
    loop {
        let reason = match connection(&paths, &updates, &mut requests, &mut reported).await {
            Ok(()) => return, // the Wayland side is gone
            Err(e) => format!("{e:#}"),
        };
        if reported.as_ref() != Some(&reason) {
            if updates.send(Update::Disconnected(reason.clone())).is_err() {
                return;
            }
            reported = Some(reason);
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

/// The same without waiting, while a window is visible: then Windows is not
/// idle-paused and input must not wait for a round trip to the supervisor.
fn wake_soon(paths: &VmPaths) {
    let api = paths.api_socket();
    tokio::spawn(async move {
        let _ = lsw_daemon::api::call(
            &api,
            &lsw_daemon::api::Request::Wake,
            Duration::from_secs(2),
        )
        .await;
    });
}

async fn connection(
    paths: &VmPaths,
    updates: &Sender<Update>,
    requests: &mut mpsc::UnboundedReceiver<Request>,
    reported: &mut Option<String>,
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
    // Visible windows as last reported (0: Windows may be idle-paused).
    let mut visible: u32 = 0;
    let mut last_motion: Option<(u32, u32)> = None;
    *reported = None;
    loop {
        tokio::select! {
            msg = reader.recv() => {
                // Bounded queue: when the Wayland side falls behind, this
                // blocks, the socket stops being read and the guest is
                // slowed down instead of filling host memory.
                let msg = msg?;
                tracing::trace!("guest message");
                if tokio::task::block_in_place(|| updates.send(Update::Guest(Box::new(msg)))).is_err() {
                    return Ok(());
                }
                tracing::trace!("guest message queued");
            }
            req = requests.recv() => {
                let Some(req) = req else {
                    let _ = writer.send(&CoMessage::Stop).await;
                    return Ok(());
                };
                tracing::trace!("request {req:?}");
                match req {
                    Request::Guest(msg) => {
                        // Windows may be idle-paused (all windows were
                        // minimized): user actions need it running. These
                        // go over the Coherence channel, which buffers.
                        if matches!(
                            msg,
                            CoMessage::Activate { .. } | CoMessage::Restore { .. } | CoMessage::HitTest { .. }
                        ) {
                            wake_soon(paths);
                        }
                        writer.send(&msg).await?
                    }
                    Request::Motion { x, y } => {
                        last_motion = Some((x, y));
                        mouse.set_abs_position(x, y).await?
                    }
                    Request::Button { button, down } => {
                        tracing::debug!("button {button} {}", if down { "down" } else { "up" });
                        if down {
                            // QEMU drops input while the VM is paused, which
                            // only happens when no window is visible: then
                            // wait for the resume and re-send the pointer
                            // position (moves made while paused were lost).
                            if visible == 0 {
                                wake(paths).await;
                                if let Some((x, y)) = last_motion {
                                    mouse.set_abs_position(x, y).await?;
                                }
                            } else {
                                wake_soon(paths);
                            }
                            mouse.press(button).await?
                        } else {
                            mouse.release(button).await?
                        }
                    }
                    Request::WindowCount(count) => {
                        visible = count;
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
                            if visible == 0 {
                                wake(paths).await;
                            } else {
                                wake_soon(paths);
                            }
                            keyboard.press(qnum).await?
                        } else {
                            keyboard.release(qnum).await?
                        }
                    }
                }
            }
            event = qmp.next_event() => {
                let event = event?;
                tracing::debug!("QMP event {} {}", event.name, event.data);
                if event.name != "VSERPORT_CHANGE"
                    || event.data.get("id").and_then(|v| v.as_str())
                        != Some(lsw_daemon::qemu::COHERENCE_PORT_ID)
                {
                    continue;
                }
                // The agent restarted or Windows rebooted. Keep this socket:
                // dropping it would look like a host departure to the guest.
                // The old windows are gone; a new engine waits for Start.
                reader.reset();
                let open = event.data.get("open").and_then(|v| v.as_bool()) == Some(true);
                let reason = if open { "the guest agent reopened its Coherence port" } else { "the guest agent closed its Coherence port" };
                if updates.send(Update::Disconnected(reason.into())).is_err() {
                    return Ok(());
                }
                if open {
                    writer.send(&CoMessage::Start { max_fps: MAX_FPS }).await?;
                    tracing::info!("Coherence started again");
                }
            }
        }
    }
}
