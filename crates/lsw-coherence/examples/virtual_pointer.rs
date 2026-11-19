//! Test harness: a persistent virtual pointer for wlroots compositors
//! (`zwlr_virtual_pointer_v1`), driven from stdin, for headless Coherence
//! tests. Commands, one per line, in output pixels:
//!
//! ```text
//! move X Y
//! click        (left button)
//! rclick       (right button)
//! down / up    (left button held / released: drags)
//! ```

use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use std::io::BufRead;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_pointer::ButtonState, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn main() -> anyhow::Result<()> {
    let width: u32 = std::env::var("OUTPUT_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1600);
    let height: u32 = std::env::var("OUTPUT_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ())?;
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut State)?;
    let mut t = 0u32;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        t += 10;
        match parts.as_slice() {
            ["move", x, y] => {
                pointer.motion_absolute(t, x.parse()?, y.parse()?, width, height);
                pointer.frame();
            }
            ["click"] | ["rclick"] => {
                let button = if parts[0] == "click" { 0x110 } else { 0x111 };
                pointer.button(t, button, ButtonState::Pressed);
                pointer.frame();
                queue.roundtrip(&mut State)?;
                std::thread::sleep(std::time::Duration::from_millis(80));
                pointer.button(t + 5, button, ButtonState::Released);
                pointer.frame();
            }
            ["down"] | ["up"] => {
                let state = if parts[0] == "down" {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                pointer.button(t, 0x110, state);
                pointer.frame();
            }
            _ => eprintln!("unknown command: {line}"),
        }
        queue.roundtrip(&mut State)?;
    }
    Ok(())
}
