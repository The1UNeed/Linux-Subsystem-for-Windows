//! `lsw-coherence`: Coherence view. Every Windows window becomes a native
//! Wayland window (its own title, taskbar/dock entry and Alt+Tab entry);
//! Windows keeps drawing its own title bar and controls.
//!
//! Threads: the Wayland event loop (calloop) runs here; a tokio backend
//! thread owns the Coherence channel and the D-Bus display (input). They
//! talk through channels.

mod backend;

use backend::{Request, Update};
use lsw_daemon::keymap;
use lsw_proto::coherence::{CoMessage, HitArea, WindowInfo, WindowKind, WindowState};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel as cchannel;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor, Gravity,
};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::RawModifiers;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

/// Most windows mirrored at once (the guest is untrusted).
const MAX_WINDOWS: usize = 128;
/// Most memory held for window contents, all windows together: each
/// window's frame plus its Wayland buffers.
const MAX_FRAME_BYTES: usize = 768 << 20;
/// Wayland buffers per window (double buffering). When both are still held
/// by the compositor, updates are coalesced until one is released.
const MAX_BUFFERS: usize = 2;
/// Copies of a window's contents charged against [`MAX_FRAME_BYTES`]
/// (frame and buffers) while its size is stable.
const HELD_COPIES: usize = 1 + MAX_BUFFERS;

/// What to draw into, given a window's buffers.
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    Reuse(usize),
    Create,
    /// Every buffer is still held by the compositor: wait for a release.
    Wait,
}

/// Choose a buffer of size `want` (height, stride). Free buffers of another
/// size are dropped (destroyed at once); busy ones stay tracked, and
/// charged, until the compositor releases them, so allocations never exceed
/// [`MAX_BUFFERS`] per window, even while the window keeps changing size.
fn pick_buffer<B>(
    buffers: &mut Vec<B>,
    want: (i32, i32),
    size: impl Fn(&B) -> (i32, i32),
    mut free: impl FnMut(&B) -> bool,
) -> Pick {
    buffers.retain(|b| size(b) == want || !free(b));
    if let Some(i) = buffers.iter().position(|b| size(b) == want && free(b)) {
        Pick::Reuse(i)
    } else if buffers.len() < MAX_BUFFERS {
        Pick::Create
    } else {
        Pick::Wait
    }
}

/// Upper bound of the memory a window can hold: its frame plus
/// [`MAX_BUFFERS`] buffers no larger than the largest one alive or needed.
fn held_bound(frame: usize, buffers: impl Iterator<Item = usize>) -> usize {
    frame + MAX_BUFFERS * buffers.fold(frame, usize::max)
}
/// Guest updates queued between the backend and the Wayland thread.
const UPDATE_QUEUE: usize = 16;

/// Size of the placeholder shown for a window that is minimized in Windows:
/// its last size if Windows reports one, else a modest default.
fn placeholder_size(info: &WindowInfo) -> (u32, u32) {
    let (w, h) = (info.rect.width, info.rect.height);
    if w >= 64 && h >= 32 {
        (w.min(1280), h.min(800))
    } else {
        (640, 400)
    }
}

/// Placeholder size charged against [`MAX_FRAME_BYTES`]: once the budget is
/// spent, a 64×32 one (8 KiB; at most `MAX_WINDOWS` of them beyond it).
fn placeholder_within_budget(info: &WindowInfo, held: usize) -> (u32, u32) {
    let size = placeholder_size(info);
    if held + size.0 as usize * size.1 as usize * 4 * HELD_COPIES > MAX_FRAME_BYTES {
        (64, 32)
    } else {
        size
    }
}

/// Linux input button codes (`linux/input-event-codes.h`).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

fn qemu_button(linux: u32) -> Option<u32> {
    use keymap::button::*;
    Some(match linux {
        BTN_LEFT => LEFT,
        BTN_RIGHT => RIGHT,
        BTN_MIDDLE => MIDDLE,
        BTN_SIDE => SIDE,
        BTN_EXTRA => EXTRA,
        _ => return None,
    })
}

enum Shell {
    Toplevel(Window),
    Popup(Popup),
}

impl Shell {
    fn surface(&self) -> &wl_surface::WlSurface {
        match self {
            Shell::Toplevel(w) => w.wl_surface(),
            Shell::Popup(p) => p.wl_surface(),
        }
    }
}

struct Mirrored {
    info: WindowInfo,
    shell: Shell,
    /// Tightly packed BGRA of the window's current size.
    frame: Vec<u8>,
    size: (u32, u32),
    configured: bool,
    /// A buffer has been shown. Until then compositor sizes are only
    /// suggestions and must not resize the real Windows window.
    mapped: bool,
    /// Size the compositor asked for that the guest has not matched yet.
    requested: Option<(u32, u32)>,
    /// Reused Wayland buffers, at most [`MAX_BUFFERS`].
    buffers: Vec<smithay_client_toolkit::shm::slot::Buffer>,
    /// An update could not be shown (every buffer busy): show it on the
    /// next frame callback or update, with full damage.
    dirty: bool,
}

impl Mirrored {
    fn buffer_bytes(&self) -> impl Iterator<Item = usize> + '_ {
        self.buffers
            .iter()
            .map(|b| b.height() as usize * b.stride() as usize)
    }

    /// Bytes charged against [`MAX_FRAME_BYTES`].
    fn held(&self) -> usize {
        held_bound(self.frame.len(), self.buffer_bytes())
    }
}

/// A press waiting for the guest's hit test.
struct PendingPress {
    id: u64,
    serial_wl: u32,
    hit_serial: u32,
    button: u32,
    released: bool,
}

struct App {
    registry: RegistryState,
    seat_state: SeatState,
    output: OutputState,
    compositor: CompositorState,
    shm: Shm,
    xdg: XdgShell,
    pool: SlotPool,
    qh: QueueHandle<App>,
    windows: HashMap<u64, Mirrored>,
    to_backend: UnboundedSender<Request>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_window: Option<u64>,
    focused: Option<u64>,
    /// Windows' foreground window, as the guest reports it.
    guest_foreground: Option<u64>,
    /// Window count last reported to the supervisor.
    reported: Option<u32>,
    /// Ctrl and Super are held (LSW's own shortcuts).
    lsw_modifiers: bool,
    /// Keys pressed in Windows and not yet released (released on focus
    /// loss so nothing stays held down).
    held: std::collections::BTreeSet<u32>,
    vm: String,
    pending: Option<PendingPress>,
    next_hit: u32,
    exit: bool,
}

impl App {
    fn id_of(&self, surface: &wl_surface::WlSurface) -> Option<u64> {
        self.windows
            .iter()
            .find(|(_, w)| w.shell.surface() == surface)
            .map(|(id, _)| *id)
    }

    fn send(&self, r: Request) {
        let _ = self.to_backend.send(r);
    }

    fn open(&mut self, info: WindowInfo) {
        if info.state == WindowState::Minimized && info.rect.width < 2 {
            // Opened minimized: wait for a real size.
        }
        tracing::info!(
            "window {:#x} {:?} {:?} ({}) {}x{}",
            info.id,
            info.kind,
            info.title,
            lsw_proto::linux_app_id(&info.app),
            info.rect.width,
            info.rect.height
        );
        if self.windows.len() >= MAX_WINDOWS {
            tracing::warn!(
                "more than {MAX_WINDOWS} windows: not mirroring {:#x}",
                info.id
            );
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let size = (info.rect.width.max(1), info.rect.height.max(1));
        let owner = info.owner.and_then(|o| self.windows.get(&o));
        let shell = match (info.kind, owner) {
            // A popup of a window we do not mirror: never a stray toplevel.
            (WindowKind::Popup, None) => return,
            (WindowKind::Popup, Some(parent)) => {
                let Ok(positioner) = XdgPositioner::new(&self.xdg) else {
                    return;
                };
                // Coordinates are bounded by validation; saturate anyway.
                let dx = info.rect.x.saturating_sub(parent.info.rect.x);
                let dy = info.rect.y.saturating_sub(parent.info.rect.y);
                positioner.set_size(size.0 as i32, size.1 as i32);
                positioner.set_anchor_rect(dx, dy, 1, 1);
                positioner.set_anchor(Anchor::TopLeft);
                positioner.set_gravity(Gravity::BottomRight);
                let parent_surface = match &parent.shell {
                    Shell::Toplevel(w) => w.xdg_surface().clone(),
                    Shell::Popup(p) => p.xdg_surface().clone(),
                };
                match Popup::from_surface(
                    Some(&parent_surface),
                    &positioner,
                    &self.qh,
                    surface,
                    &self.xdg,
                ) {
                    Ok(p) => {
                        p.wl_surface().commit();
                        Shell::Popup(p)
                    }
                    Err(e) => {
                        tracing::warn!("popup: {e}");
                        return;
                    }
                }
            }
            _ => {
                let w = self
                    .xdg
                    .create_window(surface, WindowDecorations::RequestClient, &self.qh);
                w.set_title(&info.title);
                w.set_app_id(lsw_proto::linux_app_id(&info.app));
                w.set_min_size(Some((64, 32)));
                if let (WindowKind::Dialog, Some(parent)) = (info.kind, owner)
                    && let Shell::Toplevel(pw) = &parent.shell
                {
                    w.set_parent(Some(pw));
                }
                match info.state {
                    WindowState::Maximized => w.set_maximized(),
                    WindowState::Minimized => w.set_minimized(),
                    WindowState::Normal => {}
                }
                w.commit();
                Shell::Toplevel(w)
            }
        };
        // A window minimized in Windows sends no frames (nothing to
        // capture), but it must still exist on Linux (in the dock) so the
        // user can restore it: show a neutral placeholder until it does.
        let (frame, size) = if info.state == WindowState::Minimized {
            let held = self.windows.values().map(Mirrored::held).sum();
            let size = placeholder_within_budget(&info, held);
            (vec![0xf0; size.0 as usize * size.1 as usize * 4], size)
        } else {
            (Vec::new(), (0, 0))
        };
        self.windows.insert(
            info.id,
            Mirrored {
                info,
                shell,
                frame,
                size,
                configured: false,
                mapped: false,
                requested: None,
                buffers: Vec::new(),
                dirty: false,
            },
        );
    }

    fn changed(&mut self, info: WindowInfo) {
        let Some(w) = self.windows.get_mut(&info.id) else {
            return self.open(info);
        };
        if let Shell::Toplevel(t) = &w.shell {
            if w.info.title != info.title {
                t.set_title(&info.title);
            }
            if w.info.state != info.state {
                match info.state {
                    WindowState::Minimized => t.set_minimized(),
                    WindowState::Maximized => t.set_maximized(),
                    WindowState::Normal => t.unset_maximized(),
                }
            }
        }
        w.info = info;
    }

    fn frame(&mut self, id: u64, width: u32, height: u32, tiles: Vec<lsw_proto::coherence::Tile>) {
        let held: usize = self.windows.values().map(Mirrored::held).sum();
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        if w.size != (width, height) {
            // Old-size buffers stay charged until released.
            let needed = held_bound(width as usize * height as usize * 4, w.buffer_bytes());
            if held - w.held() + needed > MAX_FRAME_BYTES {
                tracing::warn!("window {id:#x}: frame budget exhausted; frame dropped");
                return;
            }
            w.size = (width, height);
            w.frame = vec![0u8; width as usize * height as usize * 4];
            if w.requested == Some((width, height)) {
                w.requested = None;
            }
        }
        for t in &tiles {
            if let Err(e) = lsw_proto::coherence::apply_tile(&mut w.frame, width, t) {
                tracing::warn!("window {id}: bad tile: {e}");
                return;
            }
        }
        if !w.configured {
            return; // drawn on the first configure
        }
        if tiles.is_empty() && !w.dirty {
            return; // nothing changed
        }
        Self::draw(&mut self.pool, &self.qh, w, Some(&tiles));
    }

    /// Show the window's frame in one of its reused buffers. With every
    /// buffer still held by the compositor, the update waits (coalesced)
    /// for the next frame callback, so buffers never pile up.
    fn draw(
        pool: &mut SlotPool,
        qh: &QueueHandle<Self>,
        w: &mut Mirrored,
        tiles: Option<&[lsw_proto::coherence::Tile]>,
    ) {
        let (width, height) = w.size;
        if width == 0 || w.frame.is_empty() {
            return;
        }
        let stride = width as i32 * 4;
        let pick = pick_buffer(
            &mut w.buffers,
            (height as i32, stride),
            |b| (b.height(), b.stride()),
            |b| b.canvas(pool).is_some(),
        );
        let index = match pick {
            Pick::Reuse(i) => i,
            Pick::Create => {
                let Ok((buffer, _)) = pool.create_buffer(
                    width as i32,
                    height as i32,
                    stride,
                    wl_shm::Format::Argb8888,
                ) else {
                    return;
                };
                w.buffers.push(buffer);
                w.buffers.len() - 1
            }
            Pick::Wait => {
                w.dirty = true;
                return;
            }
        };
        let buffer = &w.buffers[index];
        let Some(canvas) = buffer.canvas(pool) else {
            return;
        };
        canvas[..w.frame.len()].copy_from_slice(&w.frame);
        let surface = w.shell.surface();
        match tiles {
            // Updates skipped while coalescing changed more than `tiles`.
            Some(tiles) if !w.dirty => {
                for t in tiles {
                    surface.damage_buffer(t.x as i32, t.y as i32, t.width as i32, t.height as i32);
                }
            }
            _ => surface.damage_buffer(0, 0, width as i32, height as i32),
        }
        w.dirty = false;
        surface.frame(
            qh,
            smithay_client_toolkit::compositor::FrameCallbackData(surface.clone()),
        );
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
            w.mapped = true;
        }
    }

    /// Leave Coherence for the Window view: open the LSW app, then exit
    /// (Windows keeps running).
    fn switch_to_window_view(&mut self) {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("lsw-app")))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| "lsw-app".into());
        let started = std::process::Command::new("systemd-run")
            .args(["--user", "--collect", "--quiet", "--unit"])
            .arg(format!("lsw-app-{}", self.vm))
            .arg(&exe)
            .args(["--name", &self.vm])
            .status()
            .is_ok_and(|s| s.success())
            || std::process::Command::new(&exe)
                .args(["--name", &self.vm])
                .stdin(std::process::Stdio::null())
                .spawn()
                .is_ok();
        if started {
            self.exit = true;
        }
    }

    /// Release every key still held in Windows (focus left, view closed).
    fn release_held_keys(&mut self) {
        for qnum in std::mem::take(&mut self.held) {
            self.send(Request::Key { qnum, down: false });
        }
    }

    /// Tell the supervisor how many windows are visible, when it changes.
    fn report_count(&mut self) {
        let visible = self
            .windows
            .values()
            .filter(|w| w.info.state != WindowState::Minimized && w.info.kind != WindowKind::Popup)
            .count() as u32;
        if self.reported != Some(visible) {
            self.reported = Some(visible);
            self.send(Request::WindowCount(visible));
        }
    }

    fn handle(&mut self, update: Update) {
        self.handle_inner(update);
        self.report_count();
    }

    fn handle_inner(&mut self, update: Update) {
        match update {
            Update::Guest(msg) => match *msg {
                CoMessage::Opened(info) => self.open(info),
                CoMessage::Changed(info) => self.changed(info),
                CoMessage::Closed { id } => {
                    self.windows.remove(&id);
                }
                CoMessage::Frame {
                    id,
                    width,
                    height,
                    tiles,
                } => self.frame(id, width, height, tiles),
                CoMessage::HitTestResult { id, serial, area } => self.hit_result(id, serial, area),
                CoMessage::Foreground { id } => self.guest_foreground = id,
                CoMessage::Desktop { width, height } => {
                    tracing::info!("Windows desktop {width}x{height}");
                }
                _ => {}
            },
            Update::Disconnected(reason) => {
                tracing::info!("Coherence ended: {reason}");
                self.held.clear();
                self.windows.clear();
            }
        }
    }

    fn hit_result(&mut self, id: u64, serial: u32, area: HitArea) {
        tracing::debug!("hit test {serial} on {id:#x}: {area:?}");
        let Some(p) = self
            .pending
            .take_if(|p| p.hit_serial == serial && p.id == id)
        else {
            return;
        };
        let edge = match area {
            HitArea::Caption => None,
            HitArea::Top => Some(ResizeEdge::Top),
            HitArea::Bottom => Some(ResizeEdge::Bottom),
            HitArea::Left => Some(ResizeEdge::Left),
            HitArea::Right => Some(ResizeEdge::Right),
            HitArea::TopLeft => Some(ResizeEdge::TopLeft),
            HitArea::TopRight => Some(ResizeEdge::TopRight),
            HitArea::BottomLeft => Some(ResizeEdge::BottomLeft),
            HitArea::BottomRight => Some(ResizeEdge::BottomRight),
            HitArea::Client | HitArea::Button => {
                // An ordinary click: deliver it to Windows now.
                self.send(Request::Button {
                    button: p.button,
                    down: true,
                });
                if p.released {
                    self.send(Request::Button {
                        button: p.button,
                        down: false,
                    });
                }
                return;
            }
        };
        // Title bar or border: a native move/resize by the compositor. Too
        // late if the button is already up; then it was just a click.
        let (Some(seat), Some(w)) = (self.seat.clone(), self.windows.get(&id)) else {
            return;
        };
        if p.released {
            return;
        }
        if let Shell::Toplevel(t) = &w.shell {
            match edge {
                None => t.move_(&seat, p.serial_wl),
                Some(e) => t.resize(&seat, p.serial_wl, e),
            }
        }
    }
}

// ------------------------------------------------------------------ main

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,zbus=warn".into()),
        )
        .init();
    let name = std::env::args()
        .skip_while(|a| a != "--name")
        .nth(1)
        .unwrap_or_else(|| "windows".into());
    let paths = lsw_daemon::paths::VmPaths::resolve(&name)?;
    paths.create()?;
    // One Coherence client per VM: launchers check this lock.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.coherence_lock())?;
    if lock.try_lock().is_err() {
        tracing::info!("Coherence is already running for {name}");
        return Ok(());
    }

    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;

    let (update_tx, update_rx) = cchannel::sync_channel::<Update>(UPDATE_QUEUE);
    let to_backend = backend::spawn(paths, update_tx);
    event_loop
        .handle()
        .insert_source(update_rx, |event, _, app: &mut App| {
            if let cchannel::Event::Msg(update) = event {
                app.handle(update);
            }
        })
        .map_err(|e| anyhow::anyhow!("event source: {e}"))?;

    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new(64 << 20, &shm)?;
    let mut app = App {
        registry: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh)?,
        xdg: XdgShell::bind(&globals, &qh)?,
        shm,
        pool,
        qh,
        windows: HashMap::new(),
        to_backend,
        seat: None,
        pointer: None,
        keyboard: None,
        pointer_window: None,
        focused: None,
        guest_foreground: None,
        reported: None,
        lsw_modifiers: false,
        held: Default::default(),
        vm: name.clone(),
        pending: None,
        next_hit: 1,
        exit: false,
    };
    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }
    Ok(())
}

// -------------------------------------------------------------- handlers

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        // The compositor showed the last buffer: an update that waited for
        // a free buffer can go now.
        if let Some(w) = self
            .windows
            .values_mut()
            .find(|w| w.dirty && w.shell.surface() == surface)
        {
            Self::draw(&mut self.pool, &self.qh, w, None);
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(id) = self.id_of(window.wl_surface()) {
            self.send(Request::Guest(CoMessage::Close { id }));
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let Some(id) = self.id_of(window.wl_surface()) else {
            return;
        };
        let mut resize = None;
        if let Some(w) = self.windows.get_mut(&id) {
            w.configured = true;
            if let (true, (Some(cw), Some(ch))) = (w.mapped, configure.new_size) {
                let want = (cw.get(), ch.get());
                if want != w.size && w.requested != Some(want) {
                    w.requested = Some(want);
                    resize = Some(want);
                }
            }
            Self::draw(&mut self.pool, &self.qh, w, None);
        }
        if let Some((width, height)) = resize {
            self.send(Request::Guest(CoMessage::Resize { id, width, height }));
        }
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        _: PopupConfigure,
    ) {
        if let Some(id) = self.id_of(popup.wl_surface())
            && let Some(w) = self.windows.get_mut(&id)
        {
            w.configured = true;
            Self::draw(&mut self.pool, &self.qh, w, None);
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        // The compositor dismissed the popup: Windows closes the menu when
        // it loses focus anyway.
        if let Some(id) = self.id_of(popup.wl_surface()) {
            self.windows.remove(&id);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seat.get_or_insert(seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        self.seat.get_or_insert_with(|| seat.clone());
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.release();
        }
        if cap == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(id) = self.id_of(&event.surface) else {
                continue;
            };
            let Some(w) = self.windows.get(&id) else {
                continue;
            };
            let (lx, ly) = (event.position.0.max(0.0), event.position.1.max(0.0));
            let gx = (w.info.rect.x + lx as i32).max(0) as u32;
            let gy = (w.info.rect.y + ly as i32).max(0) as u32;
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.pointer_window = Some(id);
                    self.send(Request::Motion { x: gx, y: gy });
                }
                PointerEventKind::Leave { .. } => {
                    self.pointer_window = None;
                }
                PointerEventKind::Motion { .. } => self.send(Request::Motion { x: gx, y: gy }),
                PointerEventKind::Press { button, serial, .. } => {
                    let Some(b) = qemu_button(button) else {
                        continue;
                    };
                    // Minimized in Windows: the placeholder has no position
                    // in the guest; a click restores the window instead.
                    if w.info.state == WindowState::Minimized {
                        self.send(Request::Guest(CoMessage::Restore { id }));
                        self.guest_foreground = Some(id);
                        continue;
                    }
                    self.send(Request::Motion { x: gx, y: gy });
                    // A real click activates the window in Windows by itself
                    // once it is on top; only raise windows that are not.
                    if self.guest_foreground != Some(id) && w.info.kind != WindowKind::Popup {
                        self.send(Request::Guest(CoMessage::Activate { id }));
                        self.guest_foreground = Some(id);
                    }
                    tracing::debug!(
                        "press {b} on {id:#x} at ({lx:.0},{ly:.0}) -> guest ({gx},{gy})"
                    );
                    if b == keymap::button::LEFT && matches!(w.shell, Shell::Toplevel(_)) {
                        // Title bar or border? Ask Windows before delivering.
                        let hit_serial = self.next_hit;
                        self.next_hit = self.next_hit.wrapping_add(1);
                        self.pending = Some(PendingPress {
                            id,
                            serial_wl: serial,
                            hit_serial,
                            button: b,
                            released: false,
                        });
                        self.send(Request::Guest(CoMessage::HitTest {
                            id,
                            serial: hit_serial,
                            x: lx as i32,
                            y: ly as i32,
                        }));
                    } else {
                        self.send(Request::Button {
                            button: b,
                            down: true,
                        });
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    let Some(b) = qemu_button(button) else {
                        continue;
                    };
                    if let Some(p) = self.pending.as_mut().filter(|p| p.button == b) {
                        p.released = true; // delivered with the hit-test answer
                    } else {
                        self.send(Request::Button {
                            button: b,
                            down: false,
                        });
                    }
                }
                PointerEventKind::Axis {
                    vertical,
                    horizontal,
                    ..
                } => {
                    use keymap::button::*;
                    let b = if vertical.discrete < 0 || vertical.absolute < 0.0 {
                        Some(WHEEL_UP)
                    } else if vertical.discrete > 0 || vertical.absolute > 0.0 {
                        Some(WHEEL_DOWN)
                    } else if horizontal.absolute < 0.0 {
                        Some(WHEEL_LEFT)
                    } else if horizontal.absolute > 0.0 {
                        Some(WHEEL_RIGHT)
                    } else {
                        None
                    };
                    if let Some(b) = b {
                        self.send(Request::Button {
                            button: b,
                            down: true,
                        });
                        self.send(Request::Button {
                            button: b,
                            down: false,
                        });
                    }
                }
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(id) = self.id_of(surface) {
            self.focused = Some(id);
            if self
                .windows
                .get(&id)
                .is_some_and(|w| w.info.state == WindowState::Minimized)
            {
                // Restored on Linux (dock, Alt+Tab): restore it in Windows.
                self.send(Request::Guest(CoMessage::Restore { id }));
                self.guest_foreground = Some(id);
                return;
            }
            // Keyboard focus moved on Linux (click, Alt+Tab): keys must go
            // to the same window in Windows.
            if self.guest_foreground != Some(id) {
                self.send(Request::Guest(CoMessage::Activate { id }));
                self.guest_foreground = Some(id);
            }
        }
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.focused = None;
        self.release_held_keys();
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // Ctrl+Super+C: back to the Window view (KEY_C = 46).
        if self.lsw_modifiers && event.raw_code == 46 {
            self.switch_to_window_view();
            return;
        }
        if let Some(qnum) = keymap::evdev_to_qnum(event.raw_code) {
            self.held.insert(qnum);
            self.send(Request::Key { qnum, down: true });
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(qnum) = keymap::evdev_to_qnum(event.raw_code) {
            self.held.remove(&qnum);
            self.send(Request::Key { qnum, down: false });
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
        // Windows generates its own key repeat while a key is held.
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.lsw_modifiers = modifiers.ctrl && modifiers.logo;
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_dispatch2!(App);
delegate_registry!(App);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_have_a_usable_size() {
        let info = |w, h| WindowInfo {
            id: 1,
            owner: None,
            kind: WindowKind::Normal,
            title: String::new(),
            exe: String::new(),
            app: String::new(),
            rect: lsw_proto::coherence::Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            state: WindowState::Minimized,
        };
        assert_eq!(placeholder_size(&info(0, 0)), (640, 400));
        assert_eq!(placeholder_size(&info(900, 700)), (900, 700));
        assert_eq!(placeholder_size(&info(5000, 3000)), (1280, 800));
        // Charged against the frame budget.
        // Charged with its buffers: HELD_COPIES copies of the contents.
        let cost = 900 * 700 * 4 * HELD_COPIES;
        assert_eq!(placeholder_within_budget(&info(900, 700), 0), (900, 700));
        assert_eq!(
            placeholder_within_budget(&info(900, 700), MAX_FRAME_BYTES - cost),
            (900, 700)
        );
        assert_eq!(
            placeholder_within_budget(&info(900, 700), MAX_FRAME_BYTES - cost + 1),
            (64, 32)
        );
    }

    /// A buffer the fake compositor may hold.
    struct FakeBuf {
        size: (i32, i32),
        busy: bool,
    }

    #[test]
    fn resizing_while_the_compositor_holds_buffers_never_exceeds_two() {
        let mut bufs: Vec<FakeBuf> = Vec::new();
        let size = |b: &FakeBuf| b.size;
        let mut allocated = 0;
        // The guest alternates sizes on every frame; the compositor never
        // releases anything.
        for i in 0..100 {
            let want = if i % 2 == 0 { (600, 3200) } else { (700, 4000) };
            match pick_buffer(&mut bufs, want, size, |b| !b.busy) {
                Pick::Create => {
                    bufs.push(FakeBuf {
                        size: want,
                        busy: true,
                    });
                    allocated += 1;
                }
                Pick::Reuse(_) => panic!("nothing is free"),
                Pick::Wait => {}
            }
            assert!(bufs.len() <= MAX_BUFFERS);
        }
        assert_eq!(allocated, MAX_BUFFERS, "two allocations, then waiting");
        // The charge covers what is alive: frame + 2 x largest buffer.
        let bytes = |b: &FakeBuf| b.size.0 as usize * b.size.1 as usize;
        let alive: usize = bufs.iter().map(bytes).sum();
        let frame = 600 * 3200;
        assert!(held_bound(frame, bufs.iter().map(bytes)) >= frame + alive);
        // Releases come: free buffers of the old size go, one of the new
        // size is made; a free one of the right size is reused.
        for b in &mut bufs {
            b.busy = false;
        }
        assert_eq!(
            pick_buffer(&mut bufs, (800, 4800), size, |b| !b.busy),
            Pick::Create
        );
        assert!(bufs.is_empty(), "free old-size buffers are dropped");
        bufs.push(FakeBuf {
            size: (800, 4800),
            busy: false,
        });
        assert_eq!(
            pick_buffer(&mut bufs, (800, 4800), size, |b| !b.busy),
            Pick::Reuse(0)
        );
    }

    #[test]
    fn buttons_map() {
        assert_eq!(qemu_button(BTN_LEFT), Some(keymap::button::LEFT));
        assert_eq!(qemu_button(BTN_RIGHT), Some(keymap::button::RIGHT));
        assert_eq!(qemu_button(0x200), None);
    }
}
