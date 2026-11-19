//! `lsw-app`: the LSW desktop app. M1 provides Window and Full Screen
//! views of Windows; Coherence (M2) reuses the backend.

mod backend;
mod framebuffer;

use adw::prelude::*;
use backend::{Cmd, Frame, UiMsg};
use gtk::{gdk, gio, glib};
use lsw_daemon::keymap;
use lsw_daemon::paths::VmPaths;
use lsw_proto::ClipFormat;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const APP_ID: &str = "dev.lsw.Lsw";

/// Command-line options (GTK's own arguments are not used).
#[derive(Debug, Clone, Default)]
struct Options {
    name: String,
    full_screen: bool,
    /// Initial window size (default 1280×846).
    size: Option<(i32, i32)>,
    /// Save a PNG of the window this long after the first frame, then quit
    /// (used for headless verification).
    screenshot: Option<(PathBuf, u64)>,
}

fn parse_args() -> Options {
    let mut o = Options {
        name: "windows".into(),
        ..Default::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => o.name = args.next().unwrap_or(o.name),
            "--full-screen" => o.full_screen = true,
            "--size" => {
                o.size = args.next().and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                });
            }
            "--screenshot" => {
                let path = args.next().map(PathBuf::from);
                let delay = args.next().and_then(|d| d.parse().ok()).unwrap_or(5);
                o.screenshot = path.map(|p| (p, delay));
            }
            "-h" | "--help" => {
                println!(
                    "usage: lsw-app [--name VM] [--full-screen] [--size WxH] [--screenshot PATH [SECONDS]]"
                );
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    o
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,zbus=warn".into()),
        )
        .init();
    let options = parse_args();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| build_window(app, options.clone()));
    app.run_with_args::<&str>(&[])
}

/// Clipboard representations, each in one wire format.
type ClipItems = Vec<(ClipFormat, Vec<u8>)>;

struct View {
    window: adw::ApplicationWindow,
    toolbar: adw::ToolbarView,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    status: adw::StatusPage,
    start_button: gtk::Button,
    /// The VM supervisor this window started, until Windows shows up.
    starting: std::cell::RefCell<Option<lsw_daemon::detached::Started>>,
    paths: VmPaths,
    /// The Windows clipboard, kept until the user presses a key or clicks
    /// in this window. A compositor takes the selection only with the
    /// serial of such an event in the focused window; a refused claim is not
    /// reported, and GTK would then ignore every later Linux copy as its own.
    pending_clipboard: std::cell::RefCell<Option<ClipItems>>,
    /// A key press or click happened since this window became active.
    input_since_active: Cell<bool>,
    /// The status page says why Start failed: the backend's retries must not
    /// replace it until the next Start or connection.
    start_failed: Cell<bool>,
    picture: gtk::Picture,
    frame: Arc<Frame>,
    cmds: UnboundedSender<Cmd>,
    full_screen: Cell<bool>,
    /// Keys pressed in Windows and not yet released.
    held: std::cell::RefCell<std::collections::BTreeSet<u32>>,
}

impl View {
    /// A key press or click in the window: the Windows clipboard may now
    /// become the Linux one.
    fn user_input(&self) {
        self.input_since_active.set(true);
        if let Some(items) = self.pending_clipboard.take() {
            set_host_clipboard(
                &gtk::prelude::WidgetExt::display(&self.window).clipboard(),
                items,
            );
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }

    fn set_full_screen(&self, on: bool) {
        self.full_screen.set(on);
        self.toolbar.set_reveal_top_bars(!on);
        if on {
            self.window.fullscreen();
        } else {
            self.window.unfullscreen();
        }
        // In Full Screen every key goes to Windows (Super, Alt+Tab …).
        if let Some(toplevel) = self
            .window
            .surface()
            .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
        {
            if on {
                toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
            } else {
                toplevel.restore_system_shortcuts();
            }
        }
        self.picture.grab_focus();
    }

    fn redraw(&self) {
        self.frame.pending.store(false, Ordering::Release);
        let fb = self.frame.fb.lock().expect("fb");
        if fb.width == 0 {
            return;
        }
        let texture = gdk::MemoryTexture::new(
            fb.width as i32,
            fb.height as i32,
            gdk::MemoryFormat::B8g8r8x8,
            &glib::Bytes::from(&fb.data[..]),
            fb.stride(),
        );
        drop(fb);
        self.picture.set_paintable(Some(&texture));
    }

    fn to_frame(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let fb = self.frame.fb.lock().expect("fb");
        fb.view_to_frame(
            f64::from(self.picture.width()),
            f64::from(self.picture.height()),
            x,
            y,
        )
    }
}

fn build_window(app: &adw::Application, options: Options) {
    let paths = match VmPaths::resolve(&options.name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lsw-app: {e:#}");
            app.quit();
            return;
        }
    };
    let paths_for_size = paths.clone();
    let frame = Arc::new(Frame::default());
    let (ui_tx, ui_rx) = async_channel::bounded::<UiMsg>(64);
    let cmds = backend::spawn(paths, frame.clone(), ui_tx);

    // --- widgets
    let title = adw::WindowTitle::new("Windows 11", "Connecting…");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let full_screen_button = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    full_screen_button.set_tooltip_text(Some("Full Screen (Ctrl+Super+F)"));
    header.pack_end(&full_screen_button);

    let menu = gio::Menu::new();
    menu.append(Some("Coherence"), Some("win.coherence"));
    menu.append(Some("Send Ctrl+Alt+Delete"), Some("win.ctrl-alt-del"));
    menu.append(Some("Shut Down Windows"), Some("win.powerdown"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Windows")
        .build();
    header.pack_end(&menu_button);

    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.add_css_class("lsw-display");

    let start_button = gtk::Button::builder()
        .label("Start Windows")
        .halign(gtk::Align::Center)
        .build();
    start_button.add_css_class("pill");
    start_button.add_css_class("suggested-action");
    let status = adw::StatusPage::builder()
        .icon_name("computer-symbolic")
        .title("Windows is not running")
        .description("Start Windows to use your Windows apps.")
        .child(&start_button)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&status, Some("status"));
    stack.add_named(&picture, Some("display"));
    stack.set_visible_child_name("status");

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Windows 11 — LSW")
        .default_width(options.size.map_or(1280, |s| s.0))
        .default_height(options.size.map_or(846, |s| s.1))
        .content(&toolbar)
        .build();

    let css = gtk::CssProvider::new();
    css.load_from_string(".lsw-display { background: black; }");
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let view = Rc::new(View {
        window: window.clone(),
        toolbar,
        title,
        stack,
        status,
        start_button: start_button.clone(),
        starting: Default::default(),
        paths: paths_for_size.clone(),
        pending_clipboard: Default::default(),
        input_since_active: Cell::new(false),
        start_failed: Cell::new(false),
        picture: picture.clone(),
        frame,
        cmds,
        full_screen: Cell::new(false),
        held: Default::default(),
    });

    // --- actions
    let a = gio::SimpleAction::new("ctrl-alt-del", None);
    a.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| view.send(Cmd::CtrlAltDel)
    ));
    window.add_action(&a);
    let coherence_name = options.name.clone();
    let a = gio::SimpleAction::new("coherence", None);
    a.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| switch_to_coherence(&window, &coherence_name)
    ));
    window.add_action(&a);
    let a = gio::SimpleAction::new("powerdown", None);
    a.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| view.send(Cmd::Powerdown)
    ));
    window.add_action(&a);
    full_screen_button.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| {
            view.set_full_screen(!view.full_screen.get());
        }
    ));
    let vm_name = options.name.clone();
    start_button.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |b| match start_vm(&vm_name) {
            Ok(started) => {
                b.set_sensitive(false);
                view.start_failed.set(false);
                view.status.set_description(Some("Starting Windows…"));
                view.starting.replace(Some(started));
            }
            Err(e) => {
                view.start_failed.set(true);
                view.status.set_description(Some(&e));
            }
        }
    ));

    // --- input
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        view,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, keyval, keycode, state| {
            view.user_input();
            let lsw_mod =
                state.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SUPER_MASK);
            if lsw_mod && matches!(keyval, gdk::Key::f | gdk::Key::F) {
                view.set_full_screen(!view.full_screen.get());
                return glib::Propagation::Stop;
            }
            if lsw_mod && matches!(keyval, gdk::Key::c | gdk::Key::C) {
                let _ =
                    gtk::prelude::WidgetExt::activate_action(&view.window, "win.coherence", None);
                return glib::Propagation::Stop;
            }
            if view.stack.visible_child_name().as_deref() != Some("display") {
                return glib::Propagation::Proceed;
            }
            match keymap::evdev_to_qnum(keycode.saturating_sub(8)) {
                Some(qnum) => {
                    view.held.borrow_mut().insert(qnum);
                    view.send(Cmd::Key { qnum, down: true });
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        }
    ));
    keys.connect_key_released(glib::clone!(
        #[weak]
        view,
        move |_, _, keycode, _| {
            if let Some(qnum) = keymap::evdev_to_qnum(keycode.saturating_sub(8)) {
                view.held.borrow_mut().remove(&qnum);
                view.send(Cmd::Key { qnum, down: false });
            }
        }
    ));
    window.add_controller(keys);
    // Focus left the window: release everything still held in Windows.
    window.connect_is_active_notify(glib::clone!(
        #[weak]
        view,
        move |w| {
            if !w.is_active() {
                view.input_since_active.set(false);
                for qnum in std::mem::take(&mut *view.held.borrow_mut()) {
                    view.send(Cmd::Key { qnum, down: false });
                }
            }
        }
    ));

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(glib::clone!(
        #[weak]
        view,
        move |_, x, y| {
            if let Some((x, y)) = view.to_frame(x, y) {
                view.send(Cmd::MouseAbs { x, y });
            }
        }
    ));
    picture.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.connect_pressed(glib::clone!(
        #[weak]
        view,
        move |g, _, x, y| {
            view.user_input();
            view.picture.grab_focus();
            if let Some((x, y)) = view.to_frame(x, y) {
                view.send(Cmd::MouseAbs { x, y });
            }
            if let Some(button) = keymap::gdk_button_to_qemu(g.current_button()) {
                view.send(Cmd::Button { button, down: true });
            }
        }
    ));
    click.connect_released(glib::clone!(
        #[weak]
        view,
        move |g, _, _, _| {
            if let Some(button) = keymap::gdk_button_to_qemu(g.current_button()) {
                view.send(Cmd::Button {
                    button,
                    down: false,
                });
            }
        }
    ));
    picture.add_controller(click);

    let scroll = gtk::EventControllerScroll::new(
        gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
    );
    scroll.connect_scroll(glib::clone!(
        #[weak]
        view,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, dx, dy| {
            use keymap::button::*;
            let b = if dy < 0.0 {
                WHEEL_UP
            } else if dy > 0.0 {
                WHEEL_DOWN
            } else if dx < 0.0 {
                WHEEL_LEFT
            } else {
                WHEEL_RIGHT
            };
            view.send(Cmd::Button {
                button: b,
                down: true,
            });
            view.send(Cmd::Button {
                button: b,
                down: false,
            });
            glib::Propagation::Stop
        }
    ));
    picture.add_controller(scroll);

    // --- dynamic resolution: follow the view size (device pixels)
    let last = Cell::new((0, 0));
    glib::timeout_add_local(
        Duration::from_millis(400),
        glib::clone!(
            #[weak]
            view,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                // Fractional scale (125 %, 150 % …): the integer scale
                // factor rounds up and would give Windows too many pixels.
                let s = view
                    .picture
                    .native()
                    .and_then(|n| n.surface())
                    .map_or(f64::from(view.picture.scale_factor().max(1)), |s| s.scale());
                let px = |logical: i32| (f64::from(logical) * s).round() as i32;
                let size = (px(view.picture.width()), px(view.picture.height()));
                if size != last.get() && size.0 > 0 && size.1 > 0 {
                    last.set(size);
                    view.send(Cmd::Resize {
                        width: size.0 as u32,
                        height: size.1 as u32,
                    });
                }
                glib::ControlFlow::Continue
            }
        ),
    );

    // --- clipboard: Linux → Windows
    let clipboard = gtk::prelude::WidgetExt::display(&window).clipboard();
    clipboard.connect_changed(glib::clone!(
        #[weak]
        view,
        move |cb| {
            tracing::debug!("Linux clipboard changed (ours: {})", cb.is_local());
            if cb.is_local() {
                return; // our own copy of the Windows clipboard
            }
            // A newer Linux copy: a Windows clipboard still waiting for a
            // click must not replace it.
            view.pending_clipboard.take();
            let cb = cb.clone();
            glib::spawn_future_local(async move {
                if let Some(items) = read_host_clipboard(&cb).await {
                    view.send(Cmd::HostClipboard(items));
                }
            });
        }
    ));

    // --- backend messages
    let shot = options.screenshot.clone();
    let frames_seen = Rc::new(Cell::new(0u64));
    // This loop owns the view: it lives as long as the backend (the
    // window); every other closure holds the view weakly.
    glib::spawn_future_local(glib::clone!(
        #[strong]
        view,
        #[strong]
        clipboard,
        async move {
            while let Ok(msg) = ui_rx.recv().await {
                match msg {
                    UiMsg::Connected => {
                        view.starting.take();
                        view.start_failed.set(false);
                        view.start_button.set_sensitive(true);
                        view.title.set_subtitle("Running");
                        view.stack.set_visible_child_name("display");
                        view.picture.grab_focus();
                    }
                    UiMsg::Redraw => {
                        view.redraw();
                        frames_seen.set(frames_seen.get() + 1);
                        if frames_seen.get() == 1
                            && let Some((path, delay)) = shot.clone()
                        {
                            let w = view.window.clone();
                            glib::timeout_add_seconds_local_once(delay as u32, move || {
                                save_screenshot(&w, &path);
                                if let Some(app) = w.application() {
                                    app.quit();
                                }
                            });
                        }
                    }
                    UiMsg::AgentAttached(version) => {
                        tracing::info!("guest agent attached ({version})");
                        view.title
                            .set_subtitle(&format!("Running · LSW agent {version}"));
                    }
                    UiMsg::GuestClipboard(items) => {
                        if view.window.is_active() && view.input_since_active.get() {
                            view.pending_clipboard.take();
                            set_host_clipboard(&clipboard, items);
                        } else {
                            view.pending_clipboard.replace(Some(items));
                        }
                    }
                    UiMsg::Disconnected(reason) => {
                        tracing::info!("not connected: {reason}");
                        view.title.set_subtitle("Not running");
                        view.stack.set_visible_child_name("status");
                        // The backend retries every few seconds while
                        // Windows boots: keep saying so until it is up or
                        // the supervisor this window started has stopped.
                        let mut starting = view.starting.borrow_mut();
                        let text = match starting.as_mut() {
                            Some(s) => {
                                // Still starting, by this window or by
                                // another supervisor (`lsw@NAME`).
                                if s.running() || lsw_daemon::vm::supervised(&view.paths) {
                                    continue;
                                }
                                view.start_failed.set(true);
                                format!(
                                    "Windows stopped while starting. For the reason, {}.",
                                    s.log_hint()
                                )
                            }
                            // Keep saying why Start failed.
                            None if view.start_failed.get() => continue,
                            None => {
                                format!("Start Windows to use your Windows apps.\n({reason})")
                            }
                        };
                        starting.take();
                        view.status.set_description(Some(&text));
                        view.start_button.set_sensitive(true);
                    }
                }
            }
        }
    ));

    // Headless verification: screenshot whatever is shown (the status page
    // if Windows is not running) if no frame arrived in time.
    if let Some((path, delay)) = options.screenshot.clone() {
        let w = window.clone();
        glib::timeout_add_seconds_local_once((delay * 3) as u32, move || {
            save_screenshot(&w, &path);
            if let Some(app) = w.application() {
                app.quit();
            }
        });
    }
    window.present();
    remember_monitor_size(&window, &paths_for_size);
    if options.full_screen {
        view.set_full_screen(true);
    }
}

/// Record the monitor size (device pixels) so the next boot gives Windows
/// that resolution natively (virtio-gpu preferred mode).
fn remember_monitor_size(window: &adw::ApplicationWindow, paths: &VmPaths) {
    let display = gtk::prelude::WidgetExt::display(window);
    let Some(monitor) = display
        .monitors()
        .item(0)
        .and_then(|m| m.downcast::<gdk::Monitor>().ok())
    else {
        return;
    };
    let g = monitor.geometry();
    // The monitor's native pixels, with fractional scaling too.
    let s = monitor.scale();
    let size = lsw_daemon::config::DisplaySize {
        width: (f64::from(g.width()) * s).round() as u32,
        height: (f64::from(g.height()) * s).round() as u32,
    };
    let file = paths.display_size_file();
    if lsw_daemon::config::DisplaySize::load(&file) != (size.width, size.height)
        && let Err(e) = size.save(&file)
    {
        tracing::warn!("could not store the display size: {e}");
    }
}

/// Leave the Window view for Coherence: start the Coherence client and
/// close this window (Windows keeps running).
fn switch_to_coherence(window: &adw::ApplicationWindow, name: &str) {
    match lsw_daemon::detached::start(
        "lsw-coherence",
        &["--name", name],
        &format!("lsw-coherence-{name}"),
    ) {
        Ok(_) => window.close(),
        Err(e) => tracing::warn!("cannot start Coherence: {e}"),
    }
}

/// Start the VM supervisor in the background: as a transient systemd user
/// unit when systemd is available (it then outlives the app and logs to
/// the journal), otherwise as a plain child process.
fn start_vm(name: &str) -> Result<lsw_daemon::detached::Started, String> {
    let flag = if name == "windows" {
        String::new()
    } else {
        format!(" --name {name}")
    };
    let cfg = VmPaths::resolve(name)
        .ok()
        .and_then(|p| lsw_daemon::config::VmConfig::load(&p.data).ok());
    match cfg {
        None => {
            return Err(format!(
                "There is no Windows VM yet. Create it in a terminal with `lsw{flag} create`, \
                 then `lsw{flag} install`."
            ));
        }
        Some(cfg) if cfg.install.is_some() => {
            return Err(format!(
                "Windows is not installed yet. Install it in a terminal with `lsw{flag} install`."
            ));
        }
        Some(_) => {}
    }
    lsw_daemon::detached::start("lsw", &["--name", name, "run"], &format!("lsw-vm-{name}"))
        .map_err(|e| format!("Could not start Windows: {e}"))
}

/// Largest clipboard item read from the Linux side.
const MAX_HOST_CLIP: usize = lsw_proto::MAX_CLIPBOARD_TOTAL as usize;

async fn read_mime(cb: &gdk::Clipboard, mime: &str) -> Option<Vec<u8>> {
    let (stream, _) = cb
        .read_future(&[mime], glib::Priority::DEFAULT)
        .await
        .ok()?;
    let mut out = Vec::new();
    let result = loop {
        let Ok(chunk) = stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await
        else {
            break None;
        };
        if chunk.is_empty() {
            break Some(out);
        }
        if out.len() + chunk.len() > MAX_HOST_CLIP {
            break None;
        }
        out.extend_from_slice(&chunk);
    };
    let _ = stream.close_future(glib::Priority::DEFAULT).await;
    result
}

async fn read_host_clipboard(cb: &gdk::Clipboard) -> Option<Vec<(ClipFormat, Vec<u8>)>> {
    let formats = cb.formats();
    let mut items = Vec::new();
    if (formats.contain_mime_type("text/plain;charset=utf-8")
        || formats.contains_type(glib::Type::STRING))
        && let Ok(Some(text)) = cb.read_text_future().await
    {
        items.push((ClipFormat::Text, text.as_bytes().to_vec()));
    }
    if formats.contain_mime_type("text/html")
        && let Some(html) = read_mime(cb, "text/html").await
    {
        items.push((ClipFormat::Html, html));
    }
    if formats.contain_mime_type("image/png")
        && let Some(png) = read_mime(cb, "image/png").await
    {
        items.push((ClipFormat::Png, png));
    }
    (!items.is_empty()).then_some(items)
}

fn set_host_clipboard(cb: &gdk::Clipboard, items: ClipItems) {
    let providers: Vec<gdk::ContentProvider> = items
        .into_iter()
        .map(|(format, data)| match format {
            ClipFormat::Text => {
                gdk::ContentProvider::for_value(&String::from_utf8_lossy(&data).to_value())
            }
            ClipFormat::Html => {
                gdk::ContentProvider::for_bytes("text/html", &glib::Bytes::from_owned(data))
            }
            ClipFormat::Png => {
                gdk::ContentProvider::for_bytes("image/png", &glib::Bytes::from_owned(data))
            }
        })
        .collect();
    tracing::debug!("Windows clipboard to Linux: {} format(s)", providers.len());
    let union = gdk::ContentProvider::new_union(&providers);
    if let Err(e) = cb.set_content(Some(&union)) {
        tracing::warn!("clipboard: {e}");
    }
}

/// Render the whole window (including its header bar) to a PNG.
fn save_screenshot(window: &adw::ApplicationWindow, path: &std::path::Path) {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let (w, h) = (f64::from(window.width()), f64::from(window.height()));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w, h);
    let Some(node) = snapshot.to_node() else {
        eprintln!("screenshot: nothing to render");
        return;
    };
    let Some(renderer) = window.renderer() else {
        eprintln!("screenshot: no renderer");
        return;
    };
    let texture = renderer.render_texture(node, None);
    match texture.save_to_png(path) {
        Ok(()) => println!("screenshot saved to {}", path.display()),
        Err(e) => eprintln!("screenshot: {e}"),
    }
}
