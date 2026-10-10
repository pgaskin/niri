use std::fmt::Write as _;
use std::fs::File;
use std::os::fd::AsFd as _;

use insta::assert_snapshot;
use niri_config::Config;
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use smithay::utils::{Logical, Point, Rectangle};
use wayland_client::backend::protocol::{Argument, Message};
use wayland_client::backend::smallvec::smallvec;
use wayland_client::protocol::wl_keyboard::KeymapFormat;
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy as _;

use super::client::{ClientId, PointerEvent};
use super::*;

/// Evdev BTN_LEFT.
const BTN_LEFT: u32 = 0x110;

#[test]
fn axis_discrete_overflow() {
    let mut f = Fixture::new();
    let id = f.add_client();

    let client = f.client(id);
    let manager = client.state.virtual_pointer_manager.as_ref().unwrap();
    let pointer = manager.create_virtual_pointer(None, &client.qh, ());
    pointer.axis_discrete(0, wl_pointer::Axis::VerticalScroll, 0., i32::MAX);
    f.roundtrip(id);
}

#[test]
fn virtual_pointer_held_button_released_on_removal() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // A window to receive the pointer.
    let app = f.add_client();
    let window = f.client(app).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(app);

    let window = f.client(app).window(&surface);
    window.set_size(800, 600);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(app);
    let _ = f.client(app).state.recent_pointer_events();

    let id = f.add_client();
    let client = f.client(id);
    let manager = client.state.virtual_pointer_manager.as_ref().unwrap();
    let pointer = manager.create_virtual_pointer(None, &client.qh, ());
    pointer.motion_absolute(0, 100, 100, 1920, 1080);
    pointer.frame();
    pointer.button(0, BTN_LEFT, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    f.roundtrip(id);

    let mut rv = String::new();
    rv += "virtual pointer button press\n";
    f.roundtrip(app);
    for event in f.client(app).state.recent_pointer_events() {
        let _ = writeln!(&mut rv, "app: {event}");
    }

    // Buttons are released when the pointer is destoyed.
    pointer.destroy();
    f.roundtrip(id);

    rv += "virtual pointer destroyed\n";
    f.roundtrip(app);
    for event in f.client(app).state.recent_pointer_events() {
        let _ = writeln!(&mut rv, "app: {event}");
    }

    assert_snapshot!(rv);
}

/// Evdev KEY_Q.
const KEY_Q: u32 = 16;

/// Opens an 800×600 window for a new client and drains its initial events.
fn open_window(f: &mut Fixture) -> (ClientId, WlSurface) {
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    let _ = f.client(id).state.recent_pointer_events();
    let _ = f.client(id).state.recent_keyboard_events(&surface);

    (id, surface)
}

/// Creates a virtual pointer for the client, for the output with the given name, or the whole
/// layout.
fn create_virtual_pointer(
    f: &mut Fixture,
    id: ClientId,
    output: Option<&str>,
) -> ZwlrVirtualPointerV1 {
    // The output names arrive in response to binding them, after the client's first sync.
    f.roundtrip(id);

    let client = f.client(id);
    let seat = client.state.seats.keys().next().unwrap().clone();
    let manager = client.state.virtual_pointer_manager.as_ref().unwrap();
    let pointer = match output {
        Some(name) => {
            let output = client
                .state
                .outputs
                .iter()
                .find(|(_, n)| *n == name)
                .map(|(o, _)| o.clone())
                .unwrap_or_else(|| panic!("no output named {name}"));
            manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &client.qh, ())
        }
        None => manager.create_virtual_pointer(Some(&seat), &client.qh, ()),
    };
    f.roundtrip(id);
    pointer
}

fn pointer_location(f: &mut Fixture) -> (f64, f64) {
    let location = f.niri().seat.get_pointer().unwrap().current_location();
    (location.x, location.y)
}

/// Returns everything the app's wl_pointer received since last time, with enter coordinates.
fn drain_pointer(f: &mut Fixture, app: ClientId) -> String {
    let mut rv = String::new();
    f.roundtrip(app);
    for event in f.client(app).state.recent_pointer_events() {
        match event {
            PointerEvent::Enter { x, y } => {
                let _ = writeln!(&mut rv, "app: enter: {x}, {y}");
            }
            event => {
                let _ = writeln!(&mut rv, "app: {event}");
            }
        }
    }
    rv
}

fn drain_keyboard(f: &mut Fixture, app: ClientId, surface: &WlSurface) -> String {
    let mut rv = String::new();
    f.roundtrip(app);
    for event in f.client(app).state.recent_keyboard_events(surface) {
        let _ = writeln!(&mut rv, "app: {event}");
    }
    rv
}

/// Visual rectangles of the windows on the active workspace of the first output, in global
/// coordinates, left to right.
fn window_rects(f: &mut Fixture) -> Vec<Rectangle<f64, Logical>> {
    let output = f.niri_output(1);
    let niri = f.niri();
    let output_geo = niri.global_space.output_geometry(&output).unwrap();
    let monitor = niri.layout.monitor_for_output(&output).unwrap();
    let mut rects: Vec<_> = monitor
        .active_workspace_ref()
        .tiles_with_render_positions()
        .map(|(tile, pos, _)| {
            let loc = pos + tile.window_loc() + output_geo.loc.to_f64();
            Rectangle::new(loc, tile.window_size())
        })
        .collect();
    rects.sort_by(|a, b| a.loc.x.total_cmp(&b.loc.x));
    rects
}

fn center(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    rect.loc + rect.size.downscale(2.).to_point()
}

#[test]
fn virtual_pointer_motion_absolute_maps_to_output_layout() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (app, _surface) = open_window(&mut f);

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, None);

    let mut rv = String::new();

    // Extents equal to the output: the position is used as is.
    pointer.motion_absolute(0, 480, 540, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (480., 540.));
    rv += "motion_absolute 480,540 of 1920x1080\n";
    rv += &drain_pointer(&mut f, app);

    // Arbitrary extents: scaled to the layout.
    pointer.motion_absolute(0, 30, 30, 100, 100);
    pointer.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (576., 324.));
    rv += "motion_absolute 30,30 of 100x100\n";
    rv += &drain_pointer(&mut f, app);

    assert_snapshot!(rv);
}

#[test]
fn virtual_pointer_with_output_maps_to_logical_size_at_scale_2() {
    let config = r#"
        output "headless-1" {
            scale 2
        }
    "#;
    let mut f = Fixture::with_config(Config::parse_mem(config).unwrap());
    f.add_output(1, (1920, 1080));
    let (app, _surface) = open_window(&mut f);

    let output = f.niri_output(1);
    let geo = f.niri().global_space.output_geometry(&output).unwrap();
    assert_eq!((geo.size.w, geo.size.h), (960, 540));

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, Some("headless-1"));

    let mut rv = String::new();

    // The extent covers the output's logical size, not its pixels.
    pointer.motion_absolute(0, 25, 50, 100, 100);
    pointer.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (240., 270.));
    rv += "motion_absolute 25,50 of 100x100\n";
    rv += &drain_pointer(&mut f, app);

    assert_snapshot!(rv);
}

#[test]
fn virtual_pointer_with_output_maps_to_that_output() {
    let config = r#"
        output "headless-1" {
            position x=0 y=0
        }
        output "headless-2" {
            position x=1920 y=0
        }
    "#;
    let mut f = Fixture::with_config(Config::parse_mem(config).unwrap());
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    // A window on the second output.
    f.niri_focus_output(2);
    let (app, _surface) = open_window(&mut f);

    let output = f.niri_output(2);
    let geo = f.niri().global_space.output_geometry(&output).unwrap();
    assert_eq!(
        (geo.loc.x, geo.loc.y, geo.size.w, geo.size.h),
        (1920, 0, 1280, 720)
    );

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, Some("headless-2"));

    let mut rv = String::new();

    pointer.motion_absolute(0, 25, 50, 100, 100);
    pointer.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (2240., 360.));
    rv += "motion_absolute 25,50 of 100x100 on headless-2\n";
    rv += &drain_pointer(&mut f, app);

    // Without an output, the same request lands in the whole layout instead.
    let whole = create_virtual_pointer(&mut f, id, None);
    whole.motion_absolute(0, 25, 50, 100, 100);
    whole.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (800., 540.));
    rv += "motion_absolute 25,50 of 100x100 on the layout\n";
    rv += &drain_pointer(&mut f, app);

    assert_snapshot!(rv);
}

#[test]
fn virtual_pointer_wheel_frame_delivered_as_one() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (app, _surface) = open_window(&mut f);

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, None);
    pointer.motion_absolute(0, 100, 100, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);

    let mut rv = String::new();
    rv += "motion\n";
    rv += &drain_pointer(&mut f, app);

    // Nothing reaches the app until the frame.
    pointer.axis_source(wl_pointer::AxisSource::Wheel);
    pointer.axis(0, wl_pointer::Axis::VerticalScroll, 15.);
    pointer.axis_discrete(0, wl_pointer::Axis::VerticalScroll, 15., 1);
    f.roundtrip(id);
    rv += "wheel notch without frame\n";
    rv += &drain_pointer(&mut f, app);

    pointer.frame();
    f.roundtrip(id);
    rv += "frame\n";
    rv += &drain_pointer(&mut f, app);

    pointer.axis_stop(0, wl_pointer::Axis::VerticalScroll);
    pointer.frame();
    f.roundtrip(id);
    rv += "axis_stop, frame\n";
    rv += &drain_pointer(&mut f, app);

    assert_snapshot!(rv);
}

#[test]
fn virtual_pointer_invalid_axis_is_protocol_error() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (app, _surface) = open_window(&mut f);

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, None);
    pointer.motion_absolute(0, 100, 100, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);
    let _ = drain_pointer(&mut f, app);

    // The typed API can't send an axis outside the enum, so send the request by hand:
    // axis(time, axis, value) is opcode 3.
    let client = f.client(id);
    let message = Message {
        sender_id: pointer.id(),
        opcode: 3,
        args: smallvec![
            Argument::Uint(0),
            Argument::Uint(42),
            Argument::Fixed(15 * 256)
        ],
    };
    client
        .connection
        .backend()
        .send_request(message, None, None)
        .unwrap();
    pointer.frame();
    client.tolerate_disconnect = true;
    client.connection.flush().unwrap();

    f.double_roundtrip(app);
    f.double_roundtrip(app);

    let error = f.client(id).connection.protocol_error().unwrap();
    assert_eq!(error.object_interface, "zwlr_virtual_pointer_v1");
    assert_eq!(error.code, 0, "invalid_axis");

    // Nothing got through, and the dead client's pointer is gone without a fuss.
    let rv = drain_pointer(&mut f, app);
    assert_eq!(rv, "");
    assert_eq!(pointer_location(&mut f), (100., 100.));
}

#[test]
fn virtual_pointer_motion_absolute_with_zero_extent_is_ignored() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (app, _surface) = open_window(&mut f);

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, None);
    pointer.motion_absolute(0, 100, 100, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);
    let _ = drain_pointer(&mut f, app);

    pointer.motion_absolute(0, 0, 0, 0, 0);
    pointer.frame();
    pointer.motion_absolute(0, 10, 10, 1920, 0);
    pointer.frame();
    pointer.motion_absolute(0, 10, 10, 0, 1080);
    pointer.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (100., 100.));

    let rv = drain_pointer(&mut f, app);
    assert_eq!(rv, "");
}

#[test]
fn virtual_pointer_motion_absolute_beyond_extent_is_clamped() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (app, _surface) = open_window(&mut f);

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, None);
    pointer.motion_absolute(0, 100, 100, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);

    let mut rv = String::new();
    rv += "motion inside\n";
    rv += &drain_pointer(&mut f, app);

    // Past the extent on both axes: lands on the far edge rather than off the layout.
    pointer.motion_absolute(0, 3000, 2000, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);
    let location = pointer_location(&mut f);
    rv += &format!(
        "motion beyond extent: pointer at {}, {}\n",
        location.0, location.1
    );
    rv += &drain_pointer(&mut f, app);
    assert!(location.0 <= 1920. && location.1 <= 1080., "{location:?}");

    pointer.motion_absolute(0, 100, 100, 1920, 1080);
    pointer.frame();
    f.roundtrip(id);
    assert_eq!(pointer_location(&mut f), (100., 100.));
    rv += "motion inside\n";
    rv += &drain_pointer(&mut f, app);

    assert_snapshot!(rv);
}

#[test]
fn virtual_pointer_click_focuses_window_for_virtual_keyboard() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Two windows; the second one has the keyboard focus.
    let (first, first_surface) = open_window(&mut f);
    let (second, second_surface) = open_window(&mut f);
    f.double_roundtrip(first);
    let _ = f.client(first).state.recent_keyboard_events(&first_surface);
    let _ = f
        .client(second)
        .state
        .recent_keyboard_events(&second_surface);

    let rects = window_rects(&mut f);
    assert_eq!(rects.len(), 2);
    let target = center(rects[0]);

    let id = f.add_client();
    let pointer = create_virtual_pointer(&mut f, id, None);
    let client = f.client(id);
    let seat = client.state.seats.keys().next().unwrap().clone();
    let manager = client.state.virtual_keyboard_manager.as_ref().unwrap();
    let keyboard = manager.create_virtual_keyboard(&seat, &client.qh, ());
    let fd = File::open("/dev/null").unwrap();
    keyboard.keymap(KeymapFormat::NoKeymap as u32, fd.as_fd(), 0);
    f.roundtrip(id);

    let mut rv = String::new();

    // Click on the first window.
    pointer.motion_absolute(0, target.x as u32, target.y as u32, 1920, 1080);
    pointer.frame();
    pointer.button(0, BTN_LEFT, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    pointer.button(0, BTN_LEFT, wl_pointer::ButtonState::Released);
    pointer.frame();
    f.roundtrip(id);
    f.double_roundtrip(first);

    rv += "virtual pointer click on first window\n";
    rv += &drain_pointer(&mut f, first).replace("app:", "first:");
    rv += &drain_pointer(&mut f, second).replace("app:", "second:");
    rv += &drain_keyboard(&mut f, first, &first_surface).replace("app:", "first:");
    rv += &drain_keyboard(&mut f, second, &second_surface).replace("app:", "second:");

    // The key goes where the click put the focus.
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);

    rv += "virtual key\n";
    rv += &drain_keyboard(&mut f, first, &first_surface).replace("app:", "first:");
    rv += &drain_keyboard(&mut f, second, &second_surface).replace("app:", "second:");

    assert_snapshot!(rv);
}
