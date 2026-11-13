//! `lsw-app`: the LSW desktop app. M1 provides Window and Full Screen
//! views of Windows; Coherence (M2) reuses the backend.

mod backend;
mod framebuffer;
mod keymap;

use adw::prelude::*;
use backend::{Cmd, Frame, UiMsg};
use gtk::{gdk, gio, glib};
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

struct View {
    window: adw::ApplicationWindow,
    toolbar: adw::ToolbarView,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    status: adw::StatusPage,
    picture: gtk::Picture,
    frame: Arc<Frame>,
    cmds: UnboundedSender<Cmd>,
    full_screen: Cell<bool>,
}

impl View {
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
        picture: picture.clone(),
        frame,
        cmds,
        full_screen: Cell::new(false),
    });

    // --- actions
    let a = gio::SimpleAction::new("ctrl-alt-del", None);
    a.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| view.send(Cmd::CtrlAltDel)
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
        move |b| {
            b.set_sensitive(false);
            view.status.set_description(Some("Starting Windows…"));
            if let Err(e) = start_vm(&vm_name) {
                view.status
                    .set_description(Some(&format!("Could not start Windows: {e}")));
                b.set_sensitive(true);
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
            let lsw_mod =
                state.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SUPER_MASK);
            if lsw_mod && matches!(keyval, gdk::Key::f | gdk::Key::F) {
                view.set_full_screen(!view.full_screen.get());
                return glib::Propagation::Stop;
            }
            if view.stack.visible_child_name().as_deref() != Some("display") {
                return glib::Propagation::Proceed;
            }
            match keymap::evdev_to_qnum(keycode.saturating_sub(8)) {
                Some(qnum) => {
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
                view.send(Cmd::Key { qnum, down: false });
            }
        }
    ));
    window.add_controller(keys);

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
                let s = view.picture.scale_factor().max(1);
                let size = (view.picture.width() * s, view.picture.height() * s);
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
            if cb.is_local() {
                return; // our own copy of the Windows clipboard
            }
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
                        view.title
                            .set_subtitle(&format!("Running · LSW agent {version}"));
                    }
                    UiMsg::GuestClipboard(items) => set_host_clipboard(&clipboard, items),
                    UiMsg::Disconnected(reason) => {
                        tracing::info!("not connected: {reason}");
                        view.title.set_subtitle("Not running");
                        view.status.set_description(Some(&format!(
                            "Start Windows to use your Windows apps.\n({reason})"
                        )));
                        view.stack.set_visible_child_name("status");
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
    if options.full_screen {
        view.set_full_screen(true);
    }
}

/// Start the VM supervisor in the background: as a transient systemd user
/// unit when systemd is available (it then outlives the app and logs to
/// the journal), otherwise as a plain child process.
fn start_vm(name: &str) -> std::io::Result<()> {
    let lsw = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("lsw")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("lsw"));
    let unit = format!("lsw-vm-{name}");
    let systemd = std::process::Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet", "--unit", &unit])
        .arg(&lsw)
        .args(["--name", name, "run"])
        .status();
    match systemd {
        Ok(s) if s.success() => Ok(()),
        _ => std::process::Command::new(&lsw)
            .args(["--name", name, "run"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ()),
    }
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

fn set_host_clipboard(cb: &gdk::Clipboard, items: Vec<(ClipFormat, Vec<u8>)>) {
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
