//! Backend thread: QEMU's D-Bus display, input, and the session channel to
//! the guest agent. Runs on its own tokio runtime and talks to the GTK
//! thread through channels.

use crate::framebuffer::Framebuffer;
use lsw_daemon::display::{ConsoleProxy, KeyboardProxy, MouseProxy};
use lsw_daemon::paths::VmPaths;
use lsw_daemon::qmp::Qmp;
use lsw_daemon::session::{self, SessionEvent};
use lsw_proto::ClipFormat;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::zvariant::{Fd, OwnedFd};

/// Messages to the GTK thread.
#[derive(Debug)]
pub enum UiMsg {
    /// Connected to the VM display.
    Connected,
    /// The framebuffer changed; read it from the shared [`Frame`].
    Redraw,
    /// The guest agent attached to the session channel.
    AgentAttached(String),
    /// Windows copied something: put it on the Linux clipboard.
    GuestClipboard(Vec<(ClipFormat, Vec<u8>)>),
    /// The VM is not running or the connection broke.
    Disconnected(String),
}

/// Commands from the GTK thread.
#[derive(Debug)]
pub enum Cmd {
    Key {
        qnum: u32,
        down: bool,
    },
    MouseAbs {
        x: u32,
        y: u32,
    },
    Button {
        button: u32,
        down: bool,
    },
    /// The view is this many device pixels: resize the Windows display.
    Resize {
        width: u32,
        height: u32,
    },
    /// The Linux clipboard changed: offer it to Windows.
    HostClipboard(Vec<(ClipFormat, Vec<u8>)>),
    /// Send Ctrl+Alt+Delete (the host keeps that combination).
    CtrlAltDel,
    /// Ask Windows to shut down (ACPI power button).
    Powerdown,
}

/// Framebuffer shared with the GTK thread, plus a flag so at most one
/// redraw request is queued at a time.
#[derive(Default)]
pub struct Frame {
    pub fb: Mutex<Framebuffer>,
    pub pending: AtomicBool,
}

/// The object QEMU calls with frames (`org.qemu.Display1.Listener`).
struct Listener {
    frame: Arc<Frame>,
    ui: async_channel::Sender<UiMsg>,
}

impl Listener {
    fn changed(&self) {
        if !self.frame.pending.swap(true, Ordering::AcqRel) {
            let _ = self.ui.try_send(UiMsg::Redraw);
        }
    }
}

#[zbus::interface(name = "org.qemu.Display1.Listener")]
impl Listener {
    async fn scanout(
        &self,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
        data: Vec<u8>,
    ) -> zbus::fdo::Result<()> {
        let r =
            self.frame
                .fb
                .lock()
                .expect("fb")
                .scanout(width, height, stride, pixman_format, &data);
        match r {
            Ok(()) => {
                self.changed();
                Ok(())
            }
            Err(e) => Err(zbus::fdo::Error::InvalidArgs(e.to_string())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        pixman_format: u32,
        data: Vec<u8>,
    ) -> zbus::fdo::Result<()> {
        let r = self.frame.fb.lock().expect("fb").update(
            x,
            y,
            width,
            height,
            stride,
            pixman_format,
            &data,
        );
        match r {
            Ok(()) => {
                self.changed();
                Ok(())
            }
            Err(e) => Err(zbus::fdo::Error::InvalidArgs(e.to_string())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "ScanoutDMABUF")]
    async fn scanout_dmabuf(
        &self,
        _dmabuf: OwnedFd,
        _width: u32,
        _height: u32,
        _stride: u32,
        _fourcc: u32,
        _modifier: u64,
        _y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        // The display runs with gl=off; DMA-BUF scanout arrives with tier A.
        Err(zbus::fdo::Error::NotSupported("DMA-BUF scanout".into()))
    }

    #[zbus(name = "UpdateDMABUF")]
    async fn update_dmabuf(&self, _x: i32, _y: i32, _width: i32, _height: i32) {}

    async fn disable(&self) {}

    async fn mouse_set(&self, _x: i32, _y: i32, _on: i32) {}

    async fn cursor_define(
        &self,
        _width: i32,
        _height: i32,
        _hot_x: i32,
        _hot_y: i32,
        _data: Vec<u8>,
    ) {
    }

    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Start the backend thread.
pub fn spawn(
    paths: VmPaths,
    frame: Arc<Frame>,
    ui: async_channel::Sender<UiMsg>,
) -> mpsc::UnboundedSender<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("lsw-backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run(paths, frame, ui, rx));
        })
        .expect("backend thread");
    tx
}

async fn run(
    paths: VmPaths,
    frame: Arc<Frame>,
    ui: async_channel::Sender<UiMsg>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) {
    loop {
        let r = session_loop(&paths, &frame, &ui, &mut cmds).await;
        let reason = match r {
            Ok(()) => "the VM stopped".to_owned(),
            Err(e) => format!("{e:#}"),
        };
        if ui.send(UiMsg::Disconnected(reason)).await.is_err() {
            return; // UI closed
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// One connection lifetime: display + session until something breaks.
async fn session_loop(
    paths: &VmPaths,
    frame: &Arc<Frame>,
    ui: &async_channel::Sender<UiMsg>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
) -> anyhow::Result<()> {
    let mut qmp = Qmp::connect_with_retry(&paths.ui_qmp_socket(), Duration::from_millis(500))
        .await
        .map_err(|_| anyhow::anyhow!("Windows is not running"))?;
    tracing::debug!("QMP connected; opening the D-Bus display");
    let display = lsw_daemon::display::connect(&mut qmp).await?;
    tracing::debug!("display connection up");
    let console = ConsoleProxy::new(&display).await?;
    let keyboard = KeyboardProxy::new(&display).await?;
    let mouse = MouseProxy::new(&display).await?;

    // QEMU answers RegisterListener first, then opens the listener
    // connection as the D-Bus *server* (QEMU 8.2 dbus-listener.c); we are
    // the client that serves the Listener object on it.
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    ours.set_nonblocking(true)?;
    console.register_listener(Fd::from(&theirs)).await?;
    drop(theirs);
    tracing::debug!("listener registered; connecting");
    let listener = Listener {
        frame: frame.clone(),
        ui: ui.clone(),
    };
    let _listener_conn =
        zbus::connection::Builder::unix_stream(tokio::net::UnixStream::from_std(ours)?)
            .p2p()
            .serve_at("/org/qemu/Display1/Listener", listener)?
            .build()
            .await?;
    tracing::debug!("listener connection up");
    ui.send(UiMsg::Connected).await.ok();

    // The session channel is optional: without the agent, display and input
    // still work.
    let mut session = session::connect(&paths.session_socket()).await.ok();
    let mut guest_clip: Vec<(ClipFormat, Vec<u8>)> = Vec::new();
    let mut guest_pending: Vec<ClipFormat> = Vec::new();
    let mut last_size = (0u32, 0u32);

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                match cmd {
                    Cmd::Key { qnum, down } => {
                        if down { keyboard.press(qnum).await? } else { keyboard.release(qnum).await? }
                    }
                    Cmd::MouseAbs { x, y } => mouse.set_abs_position(x, y).await?,
                    Cmd::Button { button, down } => {
                        if down { mouse.press(button).await? } else { mouse.release(button).await? }
                    }
                    Cmd::Resize { width, height } => {
                        if (width, height) != last_size && width >= 640 && height >= 480 {
                            last_size = (width, height);
                            // Physical size at 96 dpi; Windows derives nothing else from it.
                            let mm = |px: u32| ((f64::from(px) * 25.4 / 96.0) as u32).min(u32::from(u16::MAX)) as u16;
                            // virtio-gpu learns the size (EDID + display event)…
                            console.set_ui_info(mm(width), mm(height), 0, 0, width, height).await?;
                            // …and the agent switches Windows to it.
                            if let Some(s) = session.as_mut() {
                                s.display_hint(width, height, 100).await?;
                            }
                        }
                    }
                    Cmd::HostClipboard(items) => {
                        if let Some(s) = session.as_mut() {
                            s.offer(items).await?;
                        }
                    }
                    Cmd::CtrlAltDel => {
                        for k in [0x1d, 0x38, 0xd3] { keyboard.press(k).await?; }
                        for k in [0xd3, 0x38, 0x1d] { keyboard.release(k).await?; }
                    }
                    Cmd::Powerdown => {
                        qmp.execute("system_powerdown", None).await?;
                    }
                }
            }
            // QMP closes when QEMU exits: that ends this connection.
            event = qmp.next_event() => {
                event?;
            }
            event = async {
                match session.as_mut() {
                    Some(s) => Some(s.next_event().await),
                    None => std::future::pending().await,
                }
            } => {
                let Some(event) = event else { continue };
                match event {
                    Ok(SessionEvent::Attached { agent_version }) => {
                        ui.send(UiMsg::AgentAttached(agent_version)).await.ok();
                        // A resize may have happened before the agent attached.
                        if last_size != (0, 0)
                            && let Some(s) = session.as_mut()
                        {
                            s.display_hint(last_size.0, last_size.1, 100).await?;
                        }
                    }
                    Ok(SessionEvent::GuestClipboard { serial, formats }) => {
                        guest_clip.clear();
                        guest_pending = formats.clone();
                        for f in formats {
                            if let Some(s) = session.as_mut() { s.fetch(serial, f).await?; }
                        }
                    }
                    Ok(SessionEvent::GuestClipboardData { format, data, .. }) => {
                        guest_pending.retain(|f| *f != format);
                        if !data.is_empty() { guest_clip.push((format, data)); }
                        if guest_pending.is_empty() && !guest_clip.is_empty() {
                            ui.send(UiMsg::GuestClipboard(std::mem::take(&mut guest_clip))).await.ok();
                        }
                    }
                    Ok(SessionEvent::AgentError { detail }) => tracing::info!("agent: {detail}"),
                    Err(e) => {
                        tracing::warn!("session channel: {e}");
                        session = None;
                    }
                }
            }
        }
    }
}
