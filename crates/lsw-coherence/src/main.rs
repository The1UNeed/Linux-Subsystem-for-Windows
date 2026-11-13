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

/// `lsw.<exe stem>`: one Linux app per Windows app (matches the generated
/// `.desktop` files).
fn app_id(exe: &str) -> String {
    let stem = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("windows")
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_ascii_lowercase();
    let clean: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!(
        "lsw.{}",
        if clean.is_empty() {
            "windows".into()
        } else {
            clean
        }
    )
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
            app_id(&info.exe),
            info.rect.width,
            info.rect.height
        );
        let surface = self.compositor.create_surface(&self.qh);
        let size = (info.rect.width.max(1), info.rect.height.max(1));
        let owner = info.owner.and_then(|o| self.windows.get(&o));
        let shell = match (info.kind, owner) {
            (WindowKind::Popup, Some(parent)) => {
                let Ok(positioner) = XdgPositioner::new(&self.xdg) else {
                    return;
                };
                let dx = info.rect.x - parent.info.rect.x;
                let dy = info.rect.y - parent.info.rect.y;
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
                w.set_app_id(app_id(&info.exe));
                w.set_min_size(Some((64, 32)));
                if let (WindowKind::Dialog, Some(parent)) = (info.kind, owner)
                    && let Shell::Toplevel(pw) = &parent.shell
                {
                    w.set_parent(Some(pw));
                }
                if info.state == WindowState::Maximized {
                    w.set_maximized();
                }
                w.commit();
                Shell::Toplevel(w)
            }
        };
        self.windows.insert(
            info.id,
            Mirrored {
                info,
                shell,
                frame: Vec::new(),
                size: (0, 0),
                configured: false,
                mapped: false,
                requested: None,
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
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        if w.size != (width, height) {
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
        Self::draw(&mut self.pool, w, Some(&tiles));
    }

    fn draw(pool: &mut SlotPool, w: &mut Mirrored, tiles: Option<&[lsw_proto::coherence::Tile]>) {
        let (width, height) = w.size;
        if width == 0 || w.frame.is_empty() {
            return;
        }
        let stride = width as i32 * 4;
        let Ok((buffer, canvas)) = pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            return;
        };
        canvas[..w.frame.len()].copy_from_slice(&w.frame);
        let surface = w.shell.surface();
        match tiles {
            Some(tiles) => {
                for t in tiles {
                    surface.damage_buffer(t.x as i32, t.y as i32, t.width as i32, t.height as i32);
                }
            }
            None => surface.damage_buffer(0, 0, width as i32, height as i32),
        }
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
            w.mapped = true;
        }
    }

    fn handle(&mut self, update: Update) {
        match update {
            Update::Guest(msg) => match msg {
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

    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;

    let (update_tx, update_rx) = cchannel::channel::<Update>();
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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
            Self::draw(&mut self.pool, w, None);
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
            Self::draw(&mut self.pool, w, None);
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
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
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
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(qnum) = keymap::evdev_to_qnum(event.raw_code) {
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
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
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
    fn app_ids_are_stable_and_safe() {
        assert_eq!(app_id("C:\\Windows\\System32\\notepad.exe"), "lsw.notepad");
        assert_eq!(app_id("C:\\Program Files\\Foo Bar\\FOO.EXE"), "lsw.foo");
        assert_eq!(app_id("C:\\x\\my app (x86).exe"), "lsw.my-app--x86-");
        assert_eq!(app_id(""), "lsw.windows");
    }

    #[test]
    fn buttons_map() {
        assert_eq!(qemu_button(BTN_LEFT), Some(keymap::button::LEFT));
        assert_eq!(qemu_button(BTN_RIGHT), Some(keymap::button::RIGHT));
        assert_eq!(qemu_button(0x200), None);
    }
}
