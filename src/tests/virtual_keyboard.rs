use std::fmt::Write as _;
use std::fs::{self, File};
use std::os::fd::AsFd as _;
use std::path::PathBuf;

use insta::assert_snapshot;
use niri_config::{Action, Config};
use smithay::backend::input::{InputEvent, InputTime, KeyState, Keycode};
use smithay::input::keyboard::xkb;
use smithay::reexports::wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2;
use smithay::reexports::wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use smithay::wayland::input_method::InputMethodSeat as _;
use wayland_client::protocol::wl_keyboard::KeymapFormat;
use wayland_client::protocol::wl_surface::WlSurface;

use crate::tests::client::{ClientId, KeyboardEvent};
use crate::tests::fixture::Fixture;
use crate::tests::test_input_backend::{TestInputBackend, TestKeyboardKeyEvent};

// Evdev keycodes.
const KEY_Q: u32 = 16;
const KEY_W: u32 = 17;
const KEY_Y: u32 = 21;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_NUMLOCK: u32 = 69;
const KEY_LEFTMETA: u32 = 125;

fn set_up(config: Option<&str>) -> (Fixture, ClientId, WlSurface) {
    let f = match config {
        Some(config) => Fixture::with_config(Config::parse_mem(config).unwrap()),
        None => Fixture::new(),
    };
    set_up_with(f)
}

fn set_up_with(mut f: Fixture) -> (Fixture, ClientId, WlSurface) {
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.roundtrip(id);

    let _ = f.client(id).state.recent_keyboard_events(&surface);

    (f, id, surface)
}

/// Creates a virtual keyboard for the client, with the given keymap, or none.
fn create_virtual_keyboard(
    f: &mut Fixture,
    id: ClientId,
    keymap: Option<&str>,
) -> ZwpVirtualKeyboardV1 {
    let client = f.client(id);
    let seat = client.state.seats.keys().next().unwrap().clone();
    let manager = client.state.virtual_keyboard_manager.as_ref().unwrap();
    let keyboard = manager.create_virtual_keyboard(&seat, &client.qh, ());

    match keymap {
        Some(keymap) => {
            let file = TempFile::new("virtual-keyboard", keymap);
            let fd = File::open(&file.path).unwrap();
            keyboard.keymap(
                KeymapFormat::XkbV1 as u32,
                fd.as_fd(),
                keymap.len() as u32 + 1,
            );
        }
        None => {
            let fd = File::open("/dev/null").unwrap();
            keyboard.keymap(KeymapFormat::NoKeymap as u32, fd.as_fd(), 0);
        }
    }

    f.roundtrip(id);
    keyboard
}

/// Makes the client an input method holding the keyboard grab, with a virtual keyboard to
/// forward keys with, uploading the seat's keymap back to it (like fcitx).
fn grab_keyboard(
    f: &mut Fixture,
    id: ClientId,
) -> (ZwpInputMethodKeyboardGrabV2, ZwpVirtualKeyboardV1) {
    let input_method = create_input_method(f, id);
    grab_keyboard_with(f, id, &input_method)
}

fn create_input_method(f: &mut Fixture, id: ClientId) -> ZwpInputMethodV2 {
    let client = f.client(id);
    let seat = client.state.seats.keys().next().unwrap().clone();
    let manager = client.state.input_method_manager.as_ref().unwrap();
    manager.get_input_method(&seat, &client.qh, ())
}

/// Like grab_keyboard(), for an input method that already exists (e.g. to grab again).
fn grab_keyboard_with(
    f: &mut Fixture,
    id: ClientId,
    input_method: &ZwpInputMethodV2,
) -> (ZwpInputMethodKeyboardGrabV2, ZwpVirtualKeyboardV1) {
    let client = f.client(id);
    let grab = input_method.grab_keyboard(&client.qh, ());
    f.roundtrip(id);

    let keymap = seat_keymap(f);
    let keyboard = create_virtual_keyboard(f, id, Some(&keymap));
    f.client(id).state.input_method_grab_events.clear();

    (grab, keyboard)
}

/// Presses or releases a key on the physical keyboard.
fn physical_key(f: &mut Fixture, key: u32, state: KeyState) {
    f.niri_state()
        .process_input_event(InputEvent::<TestInputBackend>::Keyboard {
            event: TestKeyboardKeyEvent {
                time: InputTime::from_micros(0),
                code: Keycode::new(key + 8),
                state,
                count: 1,
            },
        });
}

/// Returns everything the app's surface and the input method's grab received since last time.
fn drain(f: &mut Fixture, app: ClientId, surface: &WlSurface, ime: Option<ClientId>) -> String {
    let mut rv = String::new();

    // The input method goes first so that whatever it sent gets flushed and processed.
    if let Some(ime) = ime {
        f.roundtrip(ime);
    }

    f.roundtrip(app);
    for event in f.client(app).state.recent_keyboard_events(surface) {
        let _ = writeln!(&mut rv, "app: {event}");
    }

    if let Some(ime) = ime {
        f.roundtrip(ime);
        for event in f.client(ime).state.input_method_grab_events.drain(..) {
            let _ = writeln!(&mut rv, "ime: {event}");
        }
    }

    rv
}

fn seat_keymap(f: &mut Fixture) -> String {
    let state = f.niri_state();
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |context| {
        context.xkb().lock().unwrap().keymap_as_string()
    })
}

fn seat_num_lock(f: &mut Fixture) -> bool {
    f.niri_state()
        .niri
        .seat
        .get_keyboard()
        .unwrap()
        .modifier_state()
        .num_lock
}

fn seat_layout(f: &mut Fixture) -> u32 {
    let state = f.niri_state();
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |context| {
        context.xkb().lock().unwrap().active_layout().0
    })
}

fn seat_mod_mask(f: &mut Fixture, name: &str) -> u32 {
    let state = f.niri_state();
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        let keymap = unsafe { xkb.keymap() };
        let idx = keymap.mod_get_index(name);
        assert_ne!(idx, xkb::MOD_INVALID, "unknown modifier {name}");
        1 << idx
    })
}

fn compile_keymap(layout: &str) -> String {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        layout,
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
}

/// Reads the app's keyboard events the way a client would (keymaps by name, and
/// mapping the keysym based on the previous keymap and modifier state).
struct Interpreter {
    /// Known keymaps as (name, text).
    keymaps: Vec<(String, String)>,
    /// The keymap the app currently has.
    current: String,
    /// Modifiers and group as the app last received them.
    mods: (u32, u32, u32, u32),
}

impl Interpreter {
    /// Starts with the seat's current keymap (the configured one) under the given name.
    fn new(f: &mut Fixture, configured_name: &str, keymaps: &[(&str, &str)]) -> Self {
        let current = seat_keymap(f);
        let mut all = vec![(configured_name.to_owned(), current.clone())];
        all.extend(keymaps.iter().map(|(n, k)| (n.to_string(), k.to_string())));
        Self {
            keymaps: all,
            current,
            mods: (0, 0, 0, 0),
        }
    }

    fn name(&self, keymap: &str) -> &str {
        self.keymaps
            .iter()
            .find(|(_, k)| k == keymap)
            .map_or("unknown", |(n, _)| n)
    }

    fn keysym(&self, key: u32) -> String {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            self.current.clone(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut state = xkb::State::new(&keymap);
        let (depressed, latched, locked, group) = self.mods;
        state.update_mask(depressed, latched, locked, 0, 0, group);
        xkb::keysym_get_name(state.key_get_one_sym(Keycode::new(key + 8)))
    }

    /// Like drain(), with keymaps named and keys interpreted.
    fn drain(
        &mut self,
        f: &mut Fixture,
        app: ClientId,
        surface: &WlSurface,
        ime: Option<ClientId>,
    ) -> String {
        let mut rv = String::new();

        if let Some(ime) = ime {
            f.roundtrip(ime);
        }

        f.roundtrip(app);
        for event in f.client(app).state.recent_keyboard_events(surface) {
            match event {
                KeyboardEvent::Keymap { keymap } => {
                    self.current.clone_from(keymap);
                    let _ = writeln!(&mut rv, "app: keymap: {}", self.name(keymap));
                }
                KeyboardEvent::Key { key, .. } => {
                    let _ = writeln!(&mut rv, "app: {event} ({})", self.keysym(*key));
                }
                KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    self.mods = (*depressed, *latched, *locked, *group);
                    let _ = writeln!(&mut rv, "app: {event}");
                }
                _ => {
                    let _ = writeln!(&mut rv, "app: {event}");
                }
            }
        }

        if let Some(ime) = ime {
            f.roundtrip(ime);
            for event in f.client(ime).state.input_method_grab_events.drain(..) {
                let _ = writeln!(&mut rv, "ime: {event}");
            }
        }

        rv
    }
}

fn seat_pressed_keys(f: &mut Fixture) -> Vec<u32> {
    let keyboard = f.niri_state().niri.seat.get_keyboard().unwrap();
    let mut keys: Vec<u32> = keyboard
        .pressed_keys()
        .into_iter()
        .map(|k| k.raw() - 8)
        .collect();
    keys.sort_unstable();
    keys
}

fn seat_shift(f: &mut Fixture) -> bool {
    f.niri_state()
        .niri
        .seat
        .get_keyboard()
        .unwrap()
        .modifier_state()
        .shift
}

fn binds_config(config: &str) -> Config {
    let mut config = Config::parse_mem(config).unwrap();
    for bind in &mut config.binds.0 {
        bind.action = Action::TestAction;
    }
    config
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str, contents: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("niri-test-{}-{n}-{name}.xkb", std::process::id()));
        // Include the NUL terminator for mapping the file as a keymap.
        fs::write(&path, format!("{contents}\0")).unwrap();
        Self { path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn input_method_forwarded_keys_bypass_grab() {
    let (mut f, app, surface) = set_up(None);
    let ime = f.add_client();
    let (_grab, keyboard) = grab_keyboard(&mut f, ime);

    let mut rv = String::new();

    // The physical key goes to the input method only.
    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    rv += "physical press\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    // The input method passes it through, which must go directly to the surface rather than to the
    // grab.
    keyboard.key(0, KEY_Q, 1);
    rv += "ime forwards press\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    keyboard.key(0, KEY_Q, 0);
    rv += "ime forwards release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    // Some other client's virtual keyboard isn't the input method's, so it goes to the grab.
    let other = f.add_client();
    let other_keyboard = create_virtual_keyboard(&mut f, other, None);
    other_keyboard.key(0, KEY_W, 1);
    f.roundtrip(other);
    rv += "other virtual keyboard press\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    other_keyboard.key(0, KEY_W, 0);
    f.roundtrip(other);
    rv += "other virtual keyboard release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    assert_snapshot!(rv);
}

#[test]
fn input_method_deactivation_keeps_physically_held_modifier() {
    let (mut f, app, surface) = set_up(None);
    let ime = f.add_client();
    let (grab, keyboard) = grab_keyboard(&mut f, ime);

    let mut rv = String::new();

    physical_key(&mut f, KEY_LEFTMETA, KeyState::Pressed);
    keyboard.key(0, KEY_LEFTMETA, 1);
    rv += "physical press, forwarded by ime\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert!(
        f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .logo
    );

    // Like fcitx when losing focus (releases grab, destroys virtual keyboard with the key still
    // down). Must not lose the pressed key.
    grab.release();
    keyboard.destroy();
    rv += "ime deactivates\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert!(
        f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .logo
    );

    physical_key(&mut f, KEY_LEFTMETA, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(
        !f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .logo
    );

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_removal_keeps_physically_held_modifier() {
    let (mut f, app, surface) = set_up(None);
    let shift = seat_mod_mask(&mut f, xkb::MOD_NAME_SHIFT);

    let mut rv = String::new();

    physical_key(&mut f, KEY_LEFTSHIFT, KeyState::Pressed);
    rv += "physical shift press\n";
    rv += &drain(&mut f, app, &surface, None);

    // A virtual keyboard sharing the keymap send the modifiers again, then is destroyed.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.modifiers(shift, 0, 0, 0);
    keyboard.destroy();
    f.roundtrip(id);
    rv += "virtual keyboard destroyed\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(
        f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .shift
    );

    physical_key(&mut f, KEY_LEFTSHIFT, KeyState::Released);
    rv += "physical shift release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(
        !f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .shift
    );

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_set_locks_reverted_on_removal() {
    let (mut f, app, surface) = set_up(None);
    let shift = seat_mod_mask(&mut f, xkb::MOD_NAME_SHIFT);
    let num_lock = seat_mod_mask(&mut f, xkb::MOD_NAME_NUM);

    // A virtual keyboard sharing the seat's keymap sets some modifiers of its own.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.modifiers(shift, 0, num_lock, 0);
    f.roundtrip(id);

    let mut rv = String::new();
    rv += "virtual keyboard modifiers\n";
    rv += &drain(&mut f, app, &surface, None);

    let mods = f
        .niri_state()
        .niri
        .seat
        .get_keyboard()
        .unwrap()
        .modifier_state();
    assert!(mods.shift);
    assert!(mods.num_lock);

    // When it is destroyed, both the depressed modifier and the lock it turned on go with it.
    keyboard.destroy();
    f.roundtrip(id);

    rv += "virtual keyboard destroyed\n";
    rv += &drain(&mut f, app, &surface, None);

    let mods = f
        .niri_state()
        .niri
        .seat
        .get_keyboard()
        .unwrap()
        .modifier_state();
    assert!(!mods.shift);
    assert!(!mods.num_lock);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_echoed_locks_kept_on_removal() {
    let (mut f, app, surface) = set_up(None);
    let num_lock = seat_mod_mask(&mut f, xkb::MOD_NAME_NUM);

    physical_key(&mut f, KEY_NUMLOCK, KeyState::Pressed);
    physical_key(&mut f, KEY_NUMLOCK, KeyState::Released);
    assert!(seat_num_lock(&mut f));

    let mut rv = String::new();
    rv += "physical num lock\n";
    rv += &drain(&mut f, app, &surface, None);

    // Like an input method, the virtual keyboard only echoes the seat's state back.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.modifiers(0, 0, num_lock, 0);
    keyboard.destroy();
    f.roundtrip(id);

    rv += "virtual keyboard echoed and destroyed\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(seat_num_lock(&mut f));

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_cleared_locks_restored_on_removal() {
    let (mut f, app, surface) = set_up(None);

    physical_key(&mut f, KEY_NUMLOCK, KeyState::Pressed);
    physical_key(&mut f, KEY_NUMLOCK, KeyState::Released);
    assert!(seat_num_lock(&mut f));

    let mut rv = String::new();
    rv += "physical num lock\n";
    rv += &drain(&mut f, app, &surface, None);

    // The virtual keyboard clears the user's lock, then is destroyed.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(id);

    rv += "virtual keyboard cleared modifiers\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(!seat_num_lock(&mut f));

    keyboard.destroy();
    f.roundtrip(id);

    rv += "virtual keyboard destroyed\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(seat_num_lock(&mut f));

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_set_layout_reverted_on_removal() {
    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "us,de"
                }
            }
        }
    "#;
    let (mut f, app, surface) = set_up(Some(config));
    assert_eq!(seat_layout(&mut f), 0);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.modifiers(0, 0, 0, 1);
    f.roundtrip(id);

    let mut rv = String::new();
    rv += "virtual keyboard layout\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_layout(&mut f), 1);

    keyboard.destroy();
    f.roundtrip(id);

    rv += "virtual keyboard destroyed\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_layout(&mut f), 0);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_shift_press_released_on_removal() {
    let (mut f, app, surface) = set_up(None);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.key(0, KEY_LEFTSHIFT, 1);
    f.roundtrip(id);

    let mut rv = String::new();
    rv += "virtual keyboard shift press\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(
        f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .shift
    );

    keyboard.destroy();
    f.roundtrip(id);

    rv += "virtual keyboard destroyed\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(
        !f.niri_state()
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .modifier_state()
            .shift
    );

    assert_snapshot!(rv);
}

#[test]
fn configured_keymap_restored_without_rereading_xkb_file() {
    let configured = compile_keymap("fr");
    let file = TempFile::new("configured", &configured);
    let config = format!(
        r#"
        input {{
            keyboard {{
                xkb {{
                    file "{}"
                }}
            }}
        }}
        "#,
        file.path.display()
    );
    let (mut f, _app, _surface) = set_up(Some(&config));
    assert_eq!(seat_keymap(&mut f), configured);

    // The file is gone by the time a virtual keyboard swaps its keymap in.
    drop(file);

    let other = compile_keymap("de");
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    // A physical key switches back to the configured keymap, from memory.
    physical_key(&mut f, KEY_W, KeyState::Pressed);
    physical_key(&mut f, KEY_W, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), configured);
}

#[test]
fn binds_work_across_physical_and_virtual_keyboards() {
    let config = "
    binds {
        Mod+Q { close-window; }
    }
    ";
    let mut config = Config::parse_mem(config).unwrap();
    for bind in &mut config.binds.0 {
        bind.action = Action::TestAction;
    }
    let (mut f, _app, _surface) = set_up_with(Fixture::with_config(config));

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    // Modifier on the physical keyboard, key on the virtual one.
    physical_key(&mut f, KEY_LEFTMETA, KeyState::Pressed);
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    physical_key(&mut f, KEY_LEFTMETA, KeyState::Released);
    assert_eq!(f.niri().test_action_count, 1);

    // And the other way around.
    keyboard.key(0, KEY_LEFTMETA, 1);
    f.roundtrip(id);
    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    keyboard.key(0, KEY_LEFTMETA, 0);
    f.roundtrip(id);
    assert_eq!(f.niri().test_action_count, 2);
}

#[test]
fn virtual_keyboard_keymap_does_not_inherit_physical_num_lock() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    physical_key(&mut f, KEY_NUMLOCK, KeyState::Pressed);
    physical_key(&mut f, KEY_NUMLOCK, KeyState::Released);
    assert!(seat_num_lock(&mut f));

    let mut rv = String::new();
    rv += "physical num lock\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The virtual keyboard's own keymap starts from a clean state: no Num Lock.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);
    assert!(!seat_num_lock(&mut f));

    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Back on the configured keymap, the user's Num Lock is still on.
    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(seat_num_lock(&mut f));

    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_keymap_num_lock_does_not_leak_into_configured() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);
    assert!(!seat_num_lock(&mut f));

    // Num Lock set through the mask on the virtual keyboard's own keymap.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    let num_lock = seat_mod_mask(&mut f, xkb::MOD_NAME_NUM);
    keyboard.modifiers(0, 0, num_lock, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);
    assert!(seat_num_lock(&mut f));

    let mut rv = String::new();
    rv += "virtual key and num lock\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // It stays with that keymap.
    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(!seat_num_lock(&mut f));

    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn config_reload_while_virtual_keyboard_keymap_active() {
    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "us"
                }
            }
        }
    "#;
    let (mut f, app, surface) = set_up(Some(config));
    let other = compile_keymap("de");
    let reloaded = compile_keymap("fr");
    let mut interp = Interpreter::new(
        &mut f,
        "configured",
        &[("virtual", &other), ("reloaded", &reloaded)],
    );

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    let mut rv = String::new();
    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Changing the layout in the config puts the new configured keymap on the seat right away.
    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "fr"
                }
            }
        }
    "#;
    let config = Config::parse_mem(config).unwrap();
    f.niri_state().reload_config(Ok(config));
    assert_eq!(seat_keymap(&mut f), reloaded);
    assert!(f.niri().virtual_keyboard_keymap.is_none());

    rv += "config reload\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The virtual keyboard's next key brings its keymap back.
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // And a physical one, the new configured keymap.
    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), reloaded);

    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[cfg(feature = "dbus")]
#[test]
fn locale1_change_while_virtual_keyboard_keymap_active() {
    use crate::dbus::freedesktop_locale1::Locale1ToNiri;

    // No xkb settings in the config, so locale1's apply.
    let (mut f, app, surface) = set_up(None);
    let other = compile_keymap("de");
    let from_locale1 = compile_keymap("fr");
    let mut interp = Interpreter::new(
        &mut f,
        "configured",
        &[("virtual", &other), ("locale1", &from_locale1)],
    );

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    let mut rv = String::new();
    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    let xkb = niri_config::Xkb {
        layout: String::from("fr"),
        ..Default::default()
    };
    f.niri_state()
        .on_locale1_msg(Locale1ToNiri::XkbChanged(xkb));
    assert_eq!(seat_keymap(&mut f), from_locale1);
    assert!(f.niri().virtual_keyboard_keymap.is_none());

    rv += "locale1 change\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), from_locale1);

    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn binds_work_with_virtual_keyboard_modifiers_mask() {
    let config = binds_config(
        "
        binds {
            Mod+Q { close-window; }
        }
        ",
    );
    let (mut f, app, surface) = set_up_with(Fixture::with_config(config));
    let logo = seat_mod_mask(&mut f, xkb::MOD_NAME_LOGO);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    // Super through the mask only, no key event for it.
    keyboard.modifiers(logo, 0, 0, 0);
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(f.niri().test_action_count, 1);

    // The modifiers are shared with the physical keyboard.
    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    assert_eq!(f.niri().test_action_count, 2);

    let mut rv = String::new();
    rv += "binds\n";
    rv += &drain(&mut f, app, &surface, None);

    // Clearing the mask stops the bind from matching.
    keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(id);
    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    assert_eq!(f.niri().test_action_count, 2);

    rv += "mask cleared, physical key\n";
    rv += &drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn identical_keymap_ownership_transfers_between_virtual_keyboards() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    let first = f.add_client();
    let first_keyboard = create_virtual_keyboard(&mut f, first, Some(&other));
    let second = f.add_client();
    let second_keyboard = create_virtual_keyboard(&mut f, second, Some(&other));

    first_keyboard.key(0, KEY_Y, 1);
    first_keyboard.key(0, KEY_Y, 0);
    f.roundtrip(first);
    assert_eq!(seat_keymap(&mut f), other);

    let mut rv = String::new();
    rv += "first virtual keyboard key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Same keymap, so nothing gets swapped, but the second one now owns it.
    second_keyboard.key(0, KEY_Y, 1);
    second_keyboard.key(0, KEY_Y, 0);
    f.roundtrip(second);
    assert_eq!(seat_keymap(&mut f), other);

    rv += "second virtual keyboard key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Removing the first one must not take the keymap away from the second.
    first_keyboard.destroy();
    f.roundtrip(first);
    assert_eq!(seat_keymap(&mut f), other);
    assert!(f.niri().virtual_keyboard_keymap.is_some());

    rv += "first virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    second_keyboard.key(0, KEY_Y, 1);
    second_keyboard.key(0, KEY_Y, 0);
    f.roundtrip(second);
    assert_eq!(seat_keymap(&mut f), other);

    rv += "second virtual keyboard key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    second_keyboard.destroy();
    f.roundtrip(second);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(f.niri().virtual_keyboard_keymap.is_none());

    rv += "second virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn configured_keymap_restored_after_virtual_keyboard_replaces_keymap() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let first = compile_keymap("de");
    let second = compile_keymap("fr");
    let mut interp = Interpreter::new(
        &mut f,
        "configured",
        &[("first", &first), ("second", &second)],
    );

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&first));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), first);

    let mut rv = String::new();
    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // A new keymap doesn't get activated until the next key, so the seat still has the first.
    let file = TempFile::new("virtual-keyboard", &second);
    let fd = File::open(&file.path).unwrap();
    keyboard.keymap(
        KeymapFormat::XkbV1 as u32,
        fd.as_fd(),
        second.len() as u32 + 1,
    );
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), first);

    // Going away without sending a key must still give the configured keymap back, since the
    // device, not the keymap, is what's tracked.
    keyboard.destroy();
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(f.niri().virtual_keyboard_keymap.is_none());

    rv += "keymap replaced, virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // And when the new keymap does get used.
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&first));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    let fd = File::open(&file.path).unwrap();
    keyboard.keymap(
        KeymapFormat::XkbV1 as u32,
        fd.as_fd(),
        second.len() as u32 + 1,
    );
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), second);

    rv += "virtual key, keymap replaced, virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard.destroy();
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), configured);

    rv += "virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_held_keys_released_on_removal_with_own_keymap() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_LEFTSHIFT, 1);
    keyboard.key(0, KEY_Y, 1);
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Y, KEY_LEFTSHIFT]);
    assert!(seat_shift(&mut f));

    let mut rv = String::new();
    rv += "virtual shift and key held\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard.destroy();
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    assert!(!seat_shift(&mut f));
    assert_eq!(seat_keymap(&mut f), configured);

    rv += "virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_key_state_other_than_pressed_released() {
    let (mut f, app, surface) = set_up(None);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    // Anything non-zero counts as pressed, and repeating it doesn't press twice.
    keyboard.key(0, KEY_Q, 2);
    keyboard.key(0, KEY_Q, 2);
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Q]);

    let mut rv = String::new();
    rv += "virtual key state 2, twice\n";
    rv += &drain(&mut f, app, &surface, None);

    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    rv += "virtual key state 0\n";
    rv += &drain(&mut f, app, &surface, None);

    // Nothing stays stuck when the keyboard is destroyed with such a key down.
    keyboard.key(0, KEY_Q, 2);
    keyboard.destroy();
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    rv += "virtual key state 2, virtual keyboard destroyed\n";
    rv += &drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_key_before_keymap_reaches_nobody() {
    let (mut f, app, surface) = set_up(None);

    // The device doesn't exist until a keymap is sent, so this is a protocol error that kills
    // the client, and the key must not get anywhere.
    let id = f.add_client();
    let client = f.client(id);
    let seat = client.state.seats.keys().next().unwrap().clone();
    let manager = client.state.virtual_keyboard_manager.as_ref().unwrap();
    let keyboard = manager.create_virtual_keyboard(&seat, &client.qh, ());
    keyboard.key(0, KEY_Q, 1);
    client.tolerate_disconnect = true;
    client.connection.flush().unwrap();

    f.double_roundtrip(app);
    f.double_roundtrip(app);

    let error = f.client(id).connection.protocol_error().unwrap();
    assert_eq!(error.object_interface, "zwp_virtual_keyboard_v1");
    assert_eq!(error.code, 0, "no_keymap");

    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    let rv = drain(&mut f, app, &surface, None);
    assert_eq!(rv, "");
}

#[test]
fn input_method_key_repeat_through_virtual_keyboard() {
    let (mut f, app, surface) = set_up(None);
    let ime = f.add_client();
    let (_grab, keyboard) = grab_keyboard(&mut f, ime);

    let mut rv = String::new();

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    keyboard.key(0, KEY_Q, 1);
    rv += "physical press, forwarded by ime\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    // Like fcitx5 repeating a key it doesn't handle: release+press pairs while the physical key
    // stays down. Every one of them must reach the app.
    for _ in 0..2 {
        keyboard.key(0, KEY_Q, 0);
        keyboard.key(0, KEY_Q, 1);
    }
    rv += "ime repeats twice\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Q]);

    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    keyboard.key(0, KEY_Q, 0);
    rv += "ime forwards release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    assert_snapshot!(rv);
}

#[test]
fn input_method_synthesized_key_leaves_xkb_state_alone() {
    let (mut f, app, surface) = set_up(None);
    let ime = f.add_client();
    let (_grab, keyboard) = grab_keyboard(&mut f, ime);

    let mut rv = String::new();

    physical_key(&mut f, KEY_LEFTSHIFT, KeyState::Pressed);
    keyboard.key(0, KEY_LEFTSHIFT, 1);
    rv += "physical shift press, forwarded by ime\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    // A key nobody physically pressed, e.g. a commit via a key the app understands. It goes to
    // the app with the current modifiers, without touching the seat's keys.
    keyboard.key(0, KEY_W, 1);
    f.roundtrip(ime);
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_LEFTSHIFT]);
    keyboard.key(0, KEY_W, 0);
    rv += "ime synthesizes key\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_LEFTSHIFT]);
    assert!(seat_shift(&mut f));

    physical_key(&mut f, KEY_LEFTSHIFT, KeyState::Released);
    keyboard.key(0, KEY_LEFTSHIFT, 0);
    rv += "physical shift release, forwarded by ime\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    assert!(!seat_shift(&mut f));

    assert_snapshot!(rv);
}

#[test]
fn input_method_regrab_between_press_and_release() {
    let (mut f, app, surface) = set_up(None);
    let ime = f.add_client();
    let input_method = create_input_method(&mut f, ime);
    let (grab, keyboard) = grab_keyboard_with(&mut f, ime, &input_method);

    let mut rv = String::new();

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    keyboard.key(0, KEY_Q, 1);
    rv += "physical press, forwarded by ime\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    // Like fcitx5 on a focus change: drop the grab and the virtual keyboard (with the key still
    // down from its point of view), then grab again.
    grab.release();
    keyboard.destroy();
    rv += "ime releases grab\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Q]);

    let (_grab, keyboard) = grab_keyboard_with(&mut f, ime, &input_method);
    rv += "ime grabs again\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    keyboard.key(0, KEY_Q, 0);
    rv += "ime forwards release\n";
    rv += &drain(&mut f, app, &surface, Some(ime));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_press_of_physically_held_key_is_absorbed() {
    let (mut f, app, surface) = set_up(None);
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    let mut rv = String::new();

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    rv += "physical press\n";
    rv += &drain(&mut f, app, &surface, None);

    keyboard.key(0, KEY_Q, 1);
    f.roundtrip(id);
    rv += "virtual press\n";
    rv += &drain(&mut f, app, &surface, None);

    // Still held by the physical keyboard.
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    rv += "virtual release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Q]);

    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    // The other way around.
    keyboard.key(0, KEY_Q, 1);
    f.roundtrip(id);
    rv += "virtual press\n";
    rv += &drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    rv += "physical press\n";
    rv += &drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Q]);

    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    rv += "virtual release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_keys_respect_shortcuts_inhibitor() {
    let config = binds_config(
        "
        binds {
            Q { close-window; }
        }
        ",
    );
    let (mut f, app, surface) = set_up_with(Fixture::with_config(config));
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    let mut rv = String::new();

    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(f.niri().test_action_count, 1);
    rv += "virtual key\n";
    rv += &drain(&mut f, app, &surface, None);

    let inhibitor = f.client(app).state.inhibit_shortcuts(&surface);
    f.roundtrip(app);

    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(f.niri().test_action_count, 1);
    rv += "inhibiting, virtual key\n";
    rv += &drain(&mut f, app, &surface, None);

    inhibitor.destroy();
    f.roundtrip(app);

    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(f.niri().test_action_count, 2);
    rv += "not inhibiting, virtual key\n";
    rv += &drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn input_method_client_death_with_held_key() {
    let config = binds_config(
        "
        binds {
            Mod+Q { close-window; }
        }
        ",
    );
    let (mut f, app, surface) = set_up_with(Fixture::with_config(config));
    let ime = f.add_client();
    let (_grab, keyboard) = grab_keyboard(&mut f, ime);

    let mut rv = String::new();

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    keyboard.key(0, KEY_Q, 1);
    rv += "physical press, forwarded by ime\n";
    rv += &drain(&mut f, app, &surface, Some(ime));

    // The input method dies with the grab active and the key down on its virtual keyboard.
    f.client(ime).disconnect();
    rv += "ime dies\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(f
        .niri_state()
        .niri
        .seat
        .input_method()
        .keyboard_grab_client()
        .is_none());

    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical release\n";
    rv += &drain(&mut f, app, &surface, None);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    assert_eq!(f.niri().test_action_count, 0);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_keymap_switches_with_physical_keyboard() {
    let (mut f, app, surface) = set_up(None);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));

    let mut rv = String::new();

    // The same keycode means z on the virtual keyboard and y on the physical one; the app has
    // to get the right keymap before each.
    for _ in 0..2 {
        keyboard.key(0, KEY_Y, 1);
        keyboard.key(0, KEY_Y, 0);
        f.roundtrip(id);
        rv += "virtual key\n";
        rv += &interp.drain(&mut f, app, &surface, None);

        physical_key(&mut f, KEY_Y, KeyState::Pressed);
        physical_key(&mut f, KEY_Y, KeyState::Released);
        rv += "physical key\n";
        rv += &interp.drain(&mut f, app, &surface, None);
    }

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_shift_held_across_keymap_switches() {
    let (mut f, app, surface) = set_up(None);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));

    let mut rv = String::new();

    keyboard.key(0, KEY_LEFTSHIFT, 1);
    f.roundtrip(id);
    rv += "virtual shift press\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(seat_shift(&mut f));

    // Modifiers are shared across keyboards, so the physical key is shifted too, and the
    // switch to the configured keymap keeps the held Shift rather than losing it.
    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(seat_shift(&mut f));

    // Back on its own keymap, the virtual keyboard's Shift is still in effect.
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(seat_shift(&mut f));

    keyboard.key(0, KEY_LEFTSHIFT, 0);
    f.roundtrip(id);
    rv += "virtual shift release\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(!seat_shift(&mut f));

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(!seat_shift(&mut f));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_modifiers_mask_applies_to_following_key() {
    let (mut f, app, surface) = set_up(None);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);
    let shift = seat_mod_mask(&mut f, xkb::MOD_NAME_SHIFT);

    let mut rv = String::new();

    // Shift through the mask only, on the configured keymap.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.modifiers(shift, 0, 0, 0);
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(id);
    rv += "shared keymap: shift mask, key, mask cleared\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(!seat_shift(&mut f));

    // And on the virtual keyboard's own keymap.
    let other_id = f.add_client();
    let other_keyboard = create_virtual_keyboard(&mut f, other_id, Some(&other));
    other_keyboard.modifiers(shift, 0, 0, 0);
    other_keyboard.key(0, KEY_Y, 1);
    other_keyboard.key(0, KEY_Y, 0);
    other_keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(other_id);
    rv += "own keymap: shift mask, key, mask cleared\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(!seat_shift(&mut f));

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_group_applies_to_own_keymap_only() {
    use niri_ipc::LayoutSwitchTarget;

    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "us,de"
                }
            }
        }
    "#;
    let (mut f, app, surface) = set_up(Some(config));
    let other = compile_keymap("fr,ru");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);
    assert_eq!(seat_layout(&mut f), 0);

    let mut rv = String::new();

    // The group goes with the virtual keyboard's keymap.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.modifiers(0, 0, 0, 1);
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);
    assert_eq!(seat_layout(&mut f), 1);
    rv += "virtual group 1, key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The user's layout is untouched by it.
    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_layout(&mut f), 0);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Nor is the virtual keyboard's by the user's.
    f.niri_state()
        .do_action(Action::SwitchLayout(LayoutSwitchTarget::Next), false);
    assert_eq!(seat_layout(&mut f), 1);
    rv += "user switches layout\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard.modifiers(0, 0, 0, 0);
    keyboard.key(0, KEY_Q, 1);
    keyboard.key(0, KEY_Q, 0);
    f.roundtrip(id);
    assert_eq!(seat_layout(&mut f), 0);
    rv += "virtual group 0, key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_layout(&mut f), 1);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

/// Uploads a new keymap to an existing virtual keyboard.
fn set_keymap(f: &mut Fixture, id: ClientId, keyboard: &ZwpVirtualKeyboardV1, keymap: &str) {
    let file = TempFile::new("virtual-keyboard", keymap);
    let fd = File::open(&file.path).unwrap();
    keyboard.keymap(
        KeymapFormat::XkbV1 as u32,
        fd.as_fd(),
        keymap.len() as u32 + 1,
    );
    f.roundtrip(id);
}

#[test]
fn virtual_keyboard_repeated_modifier_press_counts_once() {
    let (mut f, app, surface) = set_up(None);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    // Like x11vnc forwarding autorepeat of a held Shift (press, press, then one
    // release, e.g., from novnc). The second press must not leave Shift needing
    // a second release.
    keyboard.key(0, KEY_LEFTSHIFT, 1);
    keyboard.key(0, KEY_LEFTSHIFT, 1);
    f.roundtrip(id);
    assert!(seat_shift(&mut f));
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_LEFTSHIFT]);

    let mut rv = String::new();
    rv += "virtual shift press, twice\n";
    rv += &drain(&mut f, app, &surface, None);

    keyboard.key(0, KEY_LEFTSHIFT, 0);
    f.roundtrip(id);
    assert!(!seat_shift(&mut f));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    rv += "virtual shift release\n";
    rv += &drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_unchanged_modifiers_mask_sends_nothing() {
    let (mut f, app, surface) = set_up(None);
    let shift = seat_mod_mask(&mut f, xkb::MOD_NAME_SHIFT);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    let mut rv = String::new();

    // Like xwlrvnc, which sends `modifiers` after every key with its own
    // computed state. The app must not see anything if the modifiers match.
    keyboard.key(0, KEY_Q, 1);
    keyboard.modifiers(0, 0, 0, 0);
    keyboard.key(0, KEY_Q, 0);
    keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(id);
    rv += "key with unchanged mask after press and release\n";
    rv += &drain(&mut f, app, &surface, None);

    keyboard.key(0, KEY_LEFTSHIFT, 1);
    keyboard.modifiers(shift, 0, 0, 0);
    keyboard.key(0, KEY_Q, 1);
    keyboard.modifiers(shift, 0, 0, 0);
    keyboard.key(0, KEY_Q, 0);
    keyboard.modifiers(shift, 0, 0, 0);
    keyboard.key(0, KEY_LEFTSHIFT, 0);
    keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(id);
    rv += "shift, key, with the mask echoed after each\n";
    rv += &drain(&mut f, app, &surface, None);
    assert!(!seat_shift(&mut f));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    assert_snapshot!(rv);
}

/// Note: This test isn't necessarily the desired behaviour, it's just the
/// result of how I currently implement it (a virtual keyboard explicitly
/// sending modifiers switches the active state until destroyed).
#[test]
fn shared_keymap_virtual_keyboard_group_overrides_user_layout_until_removed() {
    use niri_ipc::LayoutSwitchTarget;

    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "us,de"
                }
            }
        }
    "#;
    let (mut f, app, surface) = set_up(Some(config));
    let mut interp = Interpreter::new(&mut f, "configured", &[]);

    let mut rv = String::new();

    // The user is on the second layout.
    f.niri_state()
        .do_action(Action::SwitchLayout(LayoutSwitchTarget::Next), false);
    assert_eq!(seat_layout(&mut f), 1);
    rv += "user switches layout\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The virtual keyboard shares the keymap, and insists on group 0 after its key.
    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    keyboard.modifiers(0, 0, 0, 0);
    f.roundtrip(id);
    assert_eq!(seat_layout(&mut f), 0);
    rv += "virtual key, modifiers with group 0\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The physical keyboard is now typing in the virtual keyboard's layout.
    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_layout(&mut f), 0);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The user's layout comes back with the virtual keyboard's removal.
    keyboard.destroy();
    f.roundtrip(id);
    assert_eq!(seat_layout(&mut f), 1);
    rv += "virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_unmapped_keycodes_pass_through() {
    let (mut f, app, surface) = set_up(None);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, None);

    let mut rv = String::new();

    // Outside the keymap's keycode range entirely.
    keyboard.key(0, 600, 1);
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), vec![600]);
    keyboard.key(0, 600, 0);
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    rv += "virtual key 600\n";
    rv += &drain(&mut f, app, &surface, None);

    // In range, but with no symbols (KEY_UNKNOWN, <I248> in the evdev keycodes).
    keyboard.key(0, 240, 1);
    keyboard.key(0, 240, 0);
    f.roundtrip(id);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    rv += "virtual key 240\n";
    rv += &drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn physically_held_shift_survives_config_reload() {
    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "us"
                }
            }
        }
    "#;
    let (mut f, app, surface) = set_up(Some(config));
    let reloaded = compile_keymap("fr");
    let mut interp = Interpreter::new(&mut f, "configured", &[("reloaded", &reloaded)]);

    let mut rv = String::new();

    physical_key(&mut f, KEY_LEFTSHIFT, KeyState::Pressed);
    rv += "physical shift press\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(seat_shift(&mut f));

    // The new keymap must come with Shift still down.
    let config = r#"
        input {
            keyboard {
                xkb {
                    layout "fr"
                }
            }
        }
    "#;
    let config = Config::parse_mem(config).unwrap();
    f.niri_state().reload_config(Ok(config));
    assert_eq!(seat_keymap(&mut f), reloaded);
    assert!(seat_shift(&mut f));
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_LEFTSHIFT]);
    rv += "config reload\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    physical_key(&mut f, KEY_LEFTSHIFT, KeyState::Released);
    rv += "physical shift release\n";
    rv += &interp.drain(&mut f, app, &surface, None);
    assert!(!seat_shift(&mut f));
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());

    assert_snapshot!(rv);
}

#[test]
fn two_virtual_keyboards_with_different_keymaps_interleave() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let keymap_a = compile_keymap("de");
    let keymap_b = compile_keymap("fr");
    let mut interp = Interpreter::new(&mut f, "configured", &[("a", &keymap_a), ("b", &keymap_b)]);

    // The user's Num Lock, which neither virtual keyboard should see or disturb.
    physical_key(&mut f, KEY_NUMLOCK, KeyState::Pressed);
    physical_key(&mut f, KEY_NUMLOCK, KeyState::Released);
    assert!(seat_num_lock(&mut f));

    let mut rv = String::new();
    rv += "physical num lock\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    let a = f.add_client();
    let keyboard_a = create_virtual_keyboard(&mut f, a, Some(&keymap_a));
    let b = f.add_client();
    let keyboard_b = create_virtual_keyboard(&mut f, b, Some(&keymap_b));

    keyboard_a.key(0, KEY_Y, 1);
    keyboard_a.key(0, KEY_Y, 0);
    f.roundtrip(a);
    assert_eq!(seat_keymap(&mut f), keymap_a);
    assert!(!seat_num_lock(&mut f));
    rv += "a key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard_b.key(0, KEY_Q, 1);
    keyboard_b.key(0, KEY_Q, 0);
    f.roundtrip(b);
    assert_eq!(seat_keymap(&mut f), keymap_b);
    assert!(!seat_num_lock(&mut f));
    rv += "b key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard_a.key(0, KEY_Y, 1);
    keyboard_a.key(0, KEY_Y, 0);
    f.roundtrip(a);
    assert_eq!(seat_keymap(&mut f), keymap_a);
    rv += "a key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The state saved before the first swap comes back once, with the configured keymap.
    physical_key(&mut f, KEY_Y, KeyState::Pressed);
    physical_key(&mut f, KEY_Y, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(seat_num_lock(&mut f));
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Removing A while B's keymap is active must not touch B's keymap.
    keyboard_b.key(0, KEY_Q, 1);
    keyboard_b.key(0, KEY_Q, 0);
    f.roundtrip(b);
    assert_eq!(seat_keymap(&mut f), keymap_b);
    rv += "b key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard_a.destroy();
    f.roundtrip(a);
    assert_eq!(seat_keymap(&mut f), keymap_b);
    assert!(f.niri().virtual_keyboard_keymap.is_some());
    rv += "a destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard_b.destroy();
    f.roundtrip(b);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(f.niri().virtual_keyboard_keymap.is_none());
    assert!(seat_num_lock(&mut f));
    rv += "b destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_keymap_replaced_with_configured_one_restores_it() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    let mut rv = String::new();
    rv += "virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // Now it mirrors the configured keymap back (like xwlrvnc after a keymap event), so the
    // next key must put the configured keymap back, rather than keep the stale swap.
    set_keymap(&mut f, id, &keyboard, &configured);
    keyboard.key(0, KEY_Y, 1);
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), configured);
    assert!(f.niri().virtual_keyboard_keymap.is_none());
    rv += "keymap replaced with configured, virtual key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    keyboard.destroy();
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), configured);
    rv += "virtual keyboard destroyed\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}

#[test]
fn virtual_keyboard_held_key_released_under_own_keymap_after_physical_key() {
    let (mut f, app, surface) = set_up(None);
    let configured = seat_keymap(&mut f);
    let other = compile_keymap("de");
    let mut interp = Interpreter::new(&mut f, "configured", &[("virtual", &other)]);

    let id = f.add_client();
    let keyboard = create_virtual_keyboard(&mut f, id, Some(&other));
    keyboard.key(0, KEY_Y, 1);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);

    let mut rv = String::new();
    rv += "virtual key held\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // The keymap switches under the held key.
    physical_key(&mut f, KEY_Q, KeyState::Pressed);
    physical_key(&mut f, KEY_Q, KeyState::Released);
    assert_eq!(seat_keymap(&mut f), configured);
    assert_eq!(seat_pressed_keys(&mut f), vec![KEY_Y]);
    rv += "physical key\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    // And back for the release.
    keyboard.key(0, KEY_Y, 0);
    f.roundtrip(id);
    assert_eq!(seat_keymap(&mut f), other);
    assert_eq!(seat_pressed_keys(&mut f), Vec::<u32>::new());
    rv += "virtual key release\n";
    rv += &interp.drain(&mut f, app, &surface, None);

    assert_snapshot!(rv);
}
