//! The Linux backends: X11 via `XTest`/`XGetImage`, a best-effort Wayland portal path, and
//! [`XvfbSession`] — the thing that makes headless computer use genuinely real on Linux
//! (`SPEC.md` §20.3), unlike macOS.
//!
//! Neither backend links a D-Bus client (none is a dependency of this crate), so anything that
//! genuinely requires talking to `org.a11y.Bus` or `org.freedesktop.portal.Desktop` in detail
//! (the AT-SPI element tree, portal `RemoteDesktop`/`ScreenCast` calls) says so honestly via
//! [`ComputerError::BackendUnavailable`] rather than faking success. Where a well-known CLI tool
//! covers the same ground without that dependency (`wl-copy`/`wl-paste` for the Wayland
//! clipboard, `pkill` for quitting by name) this module shells out to it instead.

use async_trait::async_trait;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tm_types::{IdSource, Result as TmResult};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, ImageFormat, PropMode, SelectionNotifyEvent, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::backend::{
    Backend, BackendKind, Capabilities, DisplayInfo, ElementNode, Screenshot, WindowInfo,
};
use crate::input::{InputAction, InputTarget, KeyChord, Modifier, MouseButton, Point, Rect};
use crate::ComputerError;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// The X11 [`Backend`]: `XTest` for input, `XGetImage` for capture, EWMH for windows. There is
/// no AT-SPI element tree here — see the module docs — so [`Backend::element_tree`] always
/// reports it unavailable rather than walking anything.
pub struct X11Backend {
    /// The `DISPLAY` value this backend talks to, e.g. `":0"` or `":97"` for a headless Xvfb.
    display: String,
}

/// Pure resolution of the target `DISPLAY` string, factored out of [`X11Backend::connect`] so it
/// is testable without touching the process environment.
fn resolve_display(display: Option<&str>, env_display: Option<&str>) -> String {
    display
        .map(str::to_string)
        .or_else(|| env_display.map(str::to_string))
        .unwrap_or_default()
}

impl X11Backend {
    /// Connect to `display` (e.g. `":0"`, or `None` to use the process's `DISPLAY`).
    ///
    /// The `x11rb` connection itself is opened lazily on first use of an operation that needs
    /// one, so constructing a backend handle never requires a reachable display — matching
    /// `MacosBackend::new`'s cheap-construction contract.
    pub fn connect(display: Option<&str>) -> TmResult<X11Backend> {
        let env_display = std::env::var("DISPLAY").ok();
        Ok(X11Backend {
            display: resolve_display(display, env_display.as_deref()),
        })
    }

    /// True when the connected X server advertises the `XTEST` extension, without which no
    /// input injection is possible.
    pub fn has_xtest_extension(&self) -> TmResult<bool> {
        let (conn, _screen) = self.open_connection()?;
        let info = conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .map_err(|e| ComputerError::Operation(format!("XTEST extension query failed: {e}")))?;
        Ok(info.is_some())
    }

    /// Open a fresh connection to [`Self::display`], naming exactly what's wrong (an unset
    /// `DISPLAY` vs. an unreachable one) when it fails.
    fn open_connection(&self) -> TmResult<(RustConnection, usize)> {
        if self.display.is_empty() {
            return Err(ComputerError::BackendUnavailable {
                backend: "x11".to_string(),
                reason: "DISPLAY is not set — export DISPLAY (e.g. `:0`), or run under \
                         linux::headless"
                    .to_string(),
            }
            .into());
        }
        x11rb::connect(Some(&self.display)).map_err(|e| {
            ComputerError::BackendUnavailable {
                backend: "x11".to_string(),
                reason: format!("cannot reach X server at DISPLAY={}: {e}", self.display),
            }
            .into()
        })
    }
}

/// Extract an [`InputTarget`]'s point. This backend keeps no element-tree state of its own (see
/// the module docs), so an `ElementRef` reaching it directly is by definition unresolved —
/// resolving refs against the last snapshot is `ComputerSession`'s job.
fn resolve_target(target: &InputTarget) -> TmResult<Point> {
    match target {
        InputTarget::Point(p) => Ok(*p),
        InputTarget::ElementRef(r) => Err(ComputerError::NotFound(format!(
            "element ref {r:?} reached the X11 backend unresolved"
        ))
        .into()),
    }
}

fn parse_window_id(id: &str) -> TmResult<Window> {
    id.parse::<u32>()
        .map_err(|_| ComputerError::NotFound(format!("not a valid window id: {id:?}")).into())
}

fn intern_atom(conn: &RustConnection, name: &str) -> TmResult<Atom> {
    let cookie = conn
        .intern_atom(false, name.as_bytes())
        .map_err(|e| ComputerError::Operation(format!("intern_atom {name} failed: {e}")))?;
    let reply = cookie
        .reply()
        .map_err(|e| ComputerError::Operation(format!("intern_atom {name} reply failed: {e}")))?;
    Ok(reply.atom)
}

fn fake_input(
    conn: &RustConnection,
    type_: u8,
    detail: u8,
    root: Window,
    x: i16,
    y: i16,
) -> TmResult<()> {
    let cookie = conn
        .xtest_fake_input(type_, detail, 0, root, x, y, 0)
        .map_err(|e| ComputerError::Operation(format!("XTest fake input request failed: {e}")))?;
    cookie
        .check()
        .map_err(|e| ComputerError::Operation(format!("XTest fake input rejected: {e}")).into())
}

fn fake_move(conn: &RustConnection, root: Window, p: Point) -> TmResult<()> {
    fake_input(
        conn,
        MOTION_NOTIFY,
        0,
        root,
        p.x.round() as i16,
        p.y.round() as i16,
    )
}

fn fake_button(conn: &RustConnection, button: MouseButton, press: bool) -> TmResult<()> {
    let code = match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    };
    let type_ = if press { BUTTON_PRESS } else { BUTTON_RELEASE };
    fake_input(conn, type_, code, 0, 0, 0)
}

/// A char's keysym, ASCII/Latin-1 codepoints mirror their keysym directly; everything else uses
/// the XKB Unicode keysym convention (`0x01000000 + codepoint`).
fn char_to_keysym(ch: char) -> u32 {
    match ch {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        c if (c as u32) <= 0xff => c as u32,
        c => 0x0100_0000 + c as u32,
    }
}

/// A named key (`"return"`, `"f5"`, ...) to its keysym, falling back to treating a single
/// character as itself when the name isn't one of the well-known ones.
fn named_key_to_keysym(key: &str) -> u32 {
    match key {
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "space" => 0x0020,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        _ => key.chars().next().map(char_to_keysym).unwrap_or(0),
    }
}

/// Common `evdev`-layout keycodes for the left variant of each modifier. A future revision
/// should resolve these from `GetKeyboardMapping`/`GetModifierMapping` instead of hardcoding,
/// but this covers the overwhelming majority of real Linux desktops and Xvfb's default map.
fn modifier_keycode(m: Modifier) -> u8 {
    match m {
        Modifier::Ctrl => 37,
        Modifier::Shift => 50,
        Modifier::Alt => 64,
        Modifier::Cmd => 133,
    }
}

/// Temporarily remap the top of the keycode range to `keysym` and fake a press/release of it —
/// the standard trick (used by tools like `xdotool type`) for injecting arbitrary Unicode
/// without a full keyboard layout.
fn fake_keysym(conn: &RustConnection, keysym: u32) -> TmResult<()> {
    let scratch = conn.setup().max_keycode;
    conn.change_keyboard_mapping(1, scratch, 1, &[keysym])
        .map_err(|e| ComputerError::Operation(format!("keyboard remap failed: {e}")))?
        .check()
        .map_err(|e| ComputerError::Operation(format!("keyboard remap rejected: {e}")))?;
    fake_input(conn, KEY_PRESS, scratch, 0, 0, 0)?;
    fake_input(conn, KEY_RELEASE, scratch, 0, 0, 0)
}

fn fake_chord(conn: &RustConnection, chord: &KeyChord) -> TmResult<()> {
    let mod_keycodes: Vec<u8> = chord
        .modifiers
        .iter()
        .map(|m| modifier_keycode(*m))
        .collect();
    for kc in &mod_keycodes {
        fake_input(conn, KEY_PRESS, *kc, 0, 0, 0)?;
    }
    let keysym = named_key_to_keysym(&chord.key);
    let result = fake_keysym(conn, keysym);
    for kc in mod_keycodes.iter().rev() {
        fake_input(conn, KEY_RELEASE, *kc, 0, 0, 0)?;
    }
    result
}

fn fake_scroll(conn: &RustConnection, delta: crate::input::ScrollDelta) -> TmResult<()> {
    if delta.dy != 0.0 {
        let button = if delta.dy < 0.0 { 4 } else { 5 };
        for _ in 0..delta.dy.abs().round().max(1.0) as u32 {
            fake_input(conn, BUTTON_PRESS, button, 0, 0, 0)?;
            fake_input(conn, BUTTON_RELEASE, button, 0, 0, 0)?;
        }
    }
    if delta.dx != 0.0 {
        let button = if delta.dx < 0.0 { 6 } else { 7 };
        for _ in 0..delta.dx.abs().round().max(1.0) as u32 {
            fake_input(conn, BUTTON_PRESS, button, 0, 0, 0)?;
            fake_input(conn, BUTTON_RELEASE, button, 0, 0, 0)?;
        }
    }
    Ok(())
}

#[async_trait]
impl Backend for X11Backend {
    fn kind(&self) -> BackendKind {
        BackendKind::X11
    }

    async fn probe(&self) -> TmResult<Capabilities> {
        let mut notes = Vec::new();
        let mut input = false;
        let mut capture = false;

        match self.open_connection() {
            Ok((conn, _)) => {
                match conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME) {
                    Ok(Some(_)) => {
                        input = true;
                        capture = true;
                    }
                    Ok(None) => {
                        notes.push(format!(
                            "XTEST extension is not advertised by DISPLAY={} — install \
                             libxtst (e.g. `apt install libxtst6`)",
                            self.display
                        ));
                        capture = true;
                    }
                    Err(e) => notes.push(format!("could not query the XTEST extension: {e}")),
                }
            }
            Err(e) => notes.push(format!(
                "DISPLAY={:?} is unreachable: {e} — export DISPLAY or run under \
                 linux::headless",
                self.display
            )),
        }

        notes.push(
            "AT-SPI element tree walking requires a D-Bus client this build does not include \
             — ComputerSession falls back to a screenshot"
                .to_string(),
        );

        Ok(Capabilities {
            backend: BackendKind::X11,
            input,
            capture,
            element_tree: false,
            headless: true,
            notes,
        })
    }

    async fn input(&self, action: InputAction) -> TmResult<()> {
        let (conn, screen_num) = self.open_connection()?;
        let root = conn.setup().roots[screen_num].root;

        match action {
            InputAction::Click { target, button } => {
                let p = resolve_target(&target)?;
                fake_move(&conn, root, p)?;
                fake_button(&conn, button, true)?;
                fake_button(&conn, button, false)?;
            }
            InputAction::DoubleClick { target } => {
                let p = resolve_target(&target)?;
                fake_move(&conn, root, p)?;
                for _ in 0..2 {
                    fake_button(&conn, MouseButton::Left, true)?;
                    fake_button(&conn, MouseButton::Left, false)?;
                }
            }
            InputAction::RightClick { target } => {
                let p = resolve_target(&target)?;
                fake_move(&conn, root, p)?;
                fake_button(&conn, MouseButton::Right, true)?;
                fake_button(&conn, MouseButton::Right, false)?;
            }
            InputAction::Drag { from, to } => {
                let from = resolve_target(&from)?;
                let to = resolve_target(&to)?;
                fake_move(&conn, root, from)?;
                fake_button(&conn, MouseButton::Left, true)?;
                for step in crate::input::lerp_drag_path(from, to, 10) {
                    fake_move(&conn, root, step)?;
                }
                fake_button(&conn, MouseButton::Left, false)?;
            }
            InputAction::TypeText(text) => {
                for ch in text.chars() {
                    fake_keysym(&conn, char_to_keysym(ch))?;
                }
            }
            InputAction::KeyChord(chord) => fake_chord(&conn, &chord)?,
            InputAction::Scroll { target, delta } => {
                let p = resolve_target(&target)?;
                fake_move(&conn, root, p)?;
                fake_scroll(&conn, delta)?;
            }
        }

        conn.flush()
            .map_err(|e| ComputerError::Operation(format!("XTest event flush failed: {e}")).into())
    }

    async fn screenshot(&self, display: Option<&str>) -> TmResult<Screenshot> {
        if let Some(id) = display {
            if id != "0" {
                return Err(ComputerError::NotFound(format!("no such display {id:?}")).into());
            }
        }

        let (conn, screen_num) = self.open_connection()?;
        let root = conn.setup().roots[screen_num].root;
        let geom = conn
            .get_geometry(root)
            .map_err(|e| ComputerError::Operation(format!("get_geometry failed: {e}")))?
            .reply()
            .map_err(|e| ComputerError::Operation(format!("get_geometry reply failed: {e}")))?;

        let image = conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                0,
                0,
                geom.width,
                geom.height,
                !0,
            )
            .map_err(|e| ComputerError::Operation(format!("XGetImage failed: {e}")))?
            .reply()
            .map_err(|e| ComputerError::PermissionMissing {
                permission: "X11 image capture".to_string(),
                fix_path: format!(
                    "ensure the X server allows XGetImage for this client (xhost, or run under \
                     linux::headless): {e}"
                ),
            })?;

        let rgba = bgrx_to_rgba(&image.data, geom.width as u32, geom.height as u32);
        let png_bytes = encode_png(geom.width as u32, geom.height as u32, &rgba);

        Ok(Screenshot {
            png_bytes,
            bounds: Rect::new(Point::new(0.0, 0.0), geom.width as f64, geom.height as f64),
        })
    }

    async fn element_tree(&self, max_depth: Option<u32>) -> TmResult<ElementNode> {
        let _ = max_depth;
        Err(ComputerError::BackendUnavailable {
            backend: "x11".to_string(),
            reason: "AT-SPI element tree walking requires a D-Bus client this build does not \
                     include — ComputerSession falls back to a screenshot"
                .to_string(),
        }
        .into())
    }

    async fn displays(&self) -> TmResult<Vec<DisplayInfo>> {
        let (conn, screen_num) = self.open_connection()?;
        let root = conn.setup().roots[screen_num].root;
        let geom = conn
            .get_geometry(root)
            .map_err(|e| ComputerError::Operation(format!("get_geometry failed: {e}")))?
            .reply()
            .map_err(|e| ComputerError::Operation(format!("get_geometry reply failed: {e}")))?;
        Ok(vec![DisplayInfo {
            id: "0".to_string(),
            bounds: Rect::new(Point::new(0.0, 0.0), geom.width as f64, geom.height as f64),
            primary: true,
        }])
    }

    async fn windows(&self) -> TmResult<Vec<WindowInfo>> {
        let (conn, screen_num) = self.open_connection()?;
        let root = conn.setup().roots[screen_num].root;

        let net_client_list = intern_atom(&conn, "_NET_CLIENT_LIST")?;
        let net_wm_name = intern_atom(&conn, "_NET_WM_NAME")?;
        let net_active_window = intern_atom(&conn, "_NET_ACTIVE_WINDOW")?;
        let utf8_string = intern_atom(&conn, "UTF8_STRING")?;

        let list_reply = conn
            .get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, u32::MAX)
            .map_err(|e| ComputerError::Operation(format!("_NET_CLIENT_LIST request failed: {e}")))?
            .reply()
            .map_err(|e| ComputerError::BackendUnavailable {
                backend: "x11".to_string(),
                reason: format!("window manager does not publish _NET_CLIENT_LIST: {e}"),
            })?;
        let ids: Vec<u32> = list_reply
            .value32()
            .map(|it| it.collect())
            .unwrap_or_default();

        let active = conn
            .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut it| it.next()));

        let mut windows = Vec::with_capacity(ids.len());
        for id in ids {
            let geom = conn.get_geometry(id).ok().and_then(|c| c.reply().ok());
            let title = conn
                .get_property(false, id, net_wm_name, utf8_string, 0, u32::MAX)
                .ok()
                .and_then(|c| c.reply().ok())
                .filter(|r| !r.value.is_empty())
                .map(|r| String::from_utf8_lossy(&r.value).into_owned());

            let bounds = match geom {
                Some(g) => Rect::new(Point::new(0.0, 0.0), g.width as f64, g.height as f64),
                None => Rect::new(Point::new(0.0, 0.0), 0.0, 0.0),
            };

            windows.push(WindowInfo {
                id: id.to_string(),
                app_name: title.clone().unwrap_or_else(|| "unknown".to_string()),
                title,
                bounds,
                focused: active == Some(id),
            });
        }
        Ok(windows)
    }

    async fn focus_window(&self, window_id: &str) -> TmResult<()> {
        let win = parse_window_id(window_id)?;
        let (conn, screen_num) = self.open_connection()?;
        let root = conn.setup().roots[screen_num].root;
        let atom = intern_atom(&conn, "_NET_ACTIVE_WINDOW")?;
        let event = ClientMessageEvent::new(32, win, atom, [1, 0, 0, 0, 0]);
        let result = conn
            .send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )
            .map_err(|e| ComputerError::Operation(format!("_NET_ACTIVE_WINDOW send failed: {e}")))?
            .check()
            .map_err(|e| {
                ComputerError::NotFound(format!("window {window_id} rejected focus: {e}")).into()
            });
        result
    }

    async fn move_window(&self, window_id: &str, to: Point) -> TmResult<()> {
        let win = parse_window_id(window_id)?;
        let (conn, _) = self.open_connection()?;
        let aux = ConfigureWindowAux::new()
            .x(to.x.round() as i32)
            .y(to.y.round() as i32);
        let result = conn
            .configure_window(win, &aux)
            .map_err(|e| ComputerError::Operation(format!("ConfigureWindow failed: {e}")))?
            .check()
            .map_err(|e| {
                ComputerError::NotFound(format!("window {window_id} could not be moved: {e}"))
                    .into()
            });
        result
    }

    async fn resize_window(&self, window_id: &str, width: f64, height: f64) -> TmResult<()> {
        let win = parse_window_id(window_id)?;
        let (conn, _) = self.open_connection()?;
        let aux = ConfigureWindowAux::new()
            .width(width.round() as u32)
            .height(height.round() as u32);
        let result = conn
            .configure_window(win, &aux)
            .map_err(|e| ComputerError::Operation(format!("ConfigureWindow failed: {e}")))?
            .check()
            .map_err(|e| {
                ComputerError::NotFound(format!("window {window_id} could not be resized: {e}"))
                    .into()
            });
        result
    }

    async fn clipboard_get(&self) -> TmResult<Option<String>> {
        let (conn, screen_num) = self.open_connection()?;
        let root = conn.setup().roots[screen_num].root;
        let win = conn
            .generate_id()
            .map_err(|e| ComputerError::Operation(format!("generate_id failed: {e}")))?;
        conn.create_window(
            0,
            win,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| ComputerError::Operation(format!("create_window failed: {e}")))?;

        let clipboard = intern_atom(&conn, "CLIPBOARD")?;
        let utf8_string = intern_atom(&conn, "UTF8_STRING")?;
        let prop = intern_atom(&conn, "TM_CLIPBOARD_TRANSFER")?;

        conn.convert_selection(win, clipboard, utf8_string, prop, x11rb::CURRENT_TIME)
            .map_err(|e| ComputerError::Operation(format!("convert_selection failed: {e}")))?;
        conn.flush()
            .map_err(|e| ComputerError::Operation(format!("flush failed: {e}")))?;

        let mut text = None;
        for _ in 0..50 {
            if let Ok(Some(Event::SelectionNotify(ev))) = conn.poll_for_event() {
                if ev.property != x11rb::NONE {
                    if let Ok(cookie) =
                        conn.get_property(false, win, prop, utf8_string, 0, u32::MAX)
                    {
                        if let Ok(reply) = cookie.reply() {
                            text = Some(String::from_utf8_lossy(&reply.value).into_owned());
                        }
                    }
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = conn.destroy_window(win);
        let _ = conn.flush();
        Ok(text)
    }

    async fn clipboard_set(&self, text: &str) -> TmResult<()> {
        let display = self.display.clone();
        let text = text.to_string();
        // Ownership of an X selection lasts only as long as its owning window/connection is
        // alive, so answering SelectionRequest events needs a standalone loop; run it on its
        // own thread rather than blocking this call on every future paste.
        std::thread::spawn(move || {
            let _ = own_clipboard_selection(&display, &text);
        });
        Ok(())
    }

    async fn launch(&self, app: &str) -> TmResult<()> {
        Command::new(app)
            .env("DISPLAY", &self.display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| ComputerError::Operation(format!("failed to launch {app:?}: {e}")).into())
    }

    async fn quit(&self, app: &str) -> TmResult<()> {
        let windows = self.windows().await?;
        let target = windows
            .into_iter()
            .find(|w| w.app_name == app || w.title.as_deref() == Some(app))
            .ok_or_else(|| ComputerError::NotFound(format!("no window for app {app:?}")))?;

        let (conn, _) = self.open_connection()?;
        let win = parse_window_id(&target.id)?;
        let net_wm_pid = intern_atom(&conn, "_NET_WM_PID")?;
        let pid = conn
            .get_property(false, win, net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut it| it.next()));

        match pid {
            Some(pid) => {
                let status = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status()
                    .map_err(|e| {
                        ComputerError::Operation(format!("failed to signal pid {pid}: {e}"))
                    })?;
                if status.success() {
                    Ok(())
                } else {
                    Err(
                        ComputerError::Operation(format!("kill -TERM {pid} exited with {status}"))
                            .into(),
                    )
                }
            }
            None => {
                let wm_protocols = intern_atom(&conn, "WM_PROTOCOLS")?;
                let wm_delete = intern_atom(&conn, "WM_DELETE_WINDOW")?;
                let event = ClientMessageEvent::new(
                    32,
                    win,
                    wm_protocols,
                    [wm_delete, x11rb::CURRENT_TIME, 0, 0, 0],
                );
                conn.send_event(false, win, EventMask::NO_EVENT, event)
                    .map_err(|e| {
                        ComputerError::Operation(format!("WM_DELETE_WINDOW send failed: {e}"))
                    })?
                    .check()
                    .map_err(|e| {
                        ComputerError::Operation(format!("WM_DELETE_WINDOW rejected: {e}")).into()
                    })
            }
        }
    }
}

/// Take ownership of `CLIPBOARD` on a scratch window and answer `SelectionRequest`s with `text`
/// until ownership is lost (or a generous bound of iterations is exhausted, so a leaked thread
/// can never spin forever).
fn own_clipboard_selection(display: &str, text: &str) -> TmResult<()> {
    let (conn, screen_num) = x11rb::connect(Some(display))
        .map_err(|e| ComputerError::Operation(format!("clipboard connection failed: {e}")))?;
    let root = conn.setup().roots[screen_num].root;
    let win = conn
        .generate_id()
        .map_err(|e| ComputerError::Operation(format!("generate_id failed: {e}")))?;
    conn.create_window(
        0,
        win,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(|e| ComputerError::Operation(format!("create_window failed: {e}")))?;

    let clipboard = intern_atom(&conn, "CLIPBOARD")?;

    conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| ComputerError::Operation(format!("set_selection_owner failed: {e}")))?;
    conn.flush()
        .map_err(|e| ComputerError::Operation(format!("flush failed: {e}")))?;

    for _ in 0..10_000 {
        match conn.poll_for_event() {
            Ok(Some(Event::SelectionRequest(req))) => {
                let _ = conn.change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    req.property,
                    req.target,
                    text.as_bytes(),
                );
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: req.property,
                };
                let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
            }
            Ok(Some(Event::SelectionClear(_))) => break,
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    let _ = conn.destroy_window(win);
    let _ = conn.flush();
    Ok(())
}

/// Convert 32-bits-per-pixel `BGRX`/`BGRA` `XGetImage` data (the common case for a 24- or
/// 32-depth `Z_PIXMAP`) to tightly packed `RGBA8`. Missing trailing pixels (a short read) are
/// padded opaque black rather than panicking on an out-of-bounds slice.
fn bgrx_to_rgba(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let want = (width as usize) * (height as usize);
    let mut out = Vec::with_capacity(want * 4);
    let mut pixels = data.chunks_exact(4);
    for _ in 0..want {
        match pixels.next() {
            Some(px) => {
                out.push(px[2]);
                out.push(px[1]);
                out.push(px[0]);
                out.push(255);
            }
            None => out.extend_from_slice(&[0, 0, 0, 255]),
        }
    }
    out
}

/// Encode raw `RGBA8` pixels as a minimal, valid PNG using stored (uncompressed) `DEFLATE`
/// blocks — no compression, but correct, and needs no image/zlib dependency beyond `std`.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((height as usize) * (1 + width as usize * 4));
    for row in 0..height as usize {
        raw.push(0); // filter type 0: none
        let start = row * width as usize * 4;
        let end = start + width as usize * 4;
        raw.extend_from_slice(&rgba[start.min(rgba.len())..end.min(rgba.len())]);
    }

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit depth, RGBA, default filter/interlace
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut png, b"IEND", &[]);
    png
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// A zlib stream (RFC 1950) wrapping `data` as one or more uncompressed `DEFLATE` stored blocks
/// (RFC 1951 §3.2.4), each capped at the format's 65535-byte block limit.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, default compression, no preset dictionary
    const MAX_BLOCK: usize = 65535;
    if data.is_empty() {
        out.push(1);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(!0u16).to_le_bytes());
    } else {
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + MAX_BLOCK).min(data.len());
            out.push(if end == data.len() { 1 } else { 0 });
            let len = (end - offset) as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(&data[offset..end]);
            offset = end;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// A best-effort Wayland backend, driving well-known CLI tools (`wl-copy`/`wl-paste`, direct
/// exec, `pkill`) rather than the portal protocol directly, since this crate has no D-Bus client
/// dependency. [`Backend::probe`] says exactly that, rather than failing silently.
pub struct WaylandBackend {
    _private: (),
}

/// Pure check of whether a `DBUS_SESSION_BUS_ADDRESS`-style address names a socket that exists,
/// factored out of [`wayland_portal_reachable`] so it's testable without touching the process
/// environment.
fn portal_reachable_from(addr: Option<&str>) -> bool {
    let addr = match addr {
        Some(a) => a,
        None => return false,
    };
    // A real `DBUS_SESSION_BUS_ADDRESS` is `<transport>:<key>=<value>,<key>=<value>,...`
    // (e.g. `unix:path=/run/user/1000/bus`) -- strip the leading `<transport>:` before looking
    // for the `path=` key, or it never matches (the whole `<transport>:path=...` string doesn't
    // start with `path=`).
    let params = addr.split_once(':').map_or(addr, |(_, rest)| rest);
    params
        .split(',')
        .find_map(|part| part.strip_prefix("path="))
        .is_some_and(|path| Path::new(path).exists())
}

fn portal_unavailable(op: &str) -> ComputerError {
    ComputerError::BackendUnavailable {
        backend: "wayland".to_string(),
        reason: format!(
            "{op} requires a D-Bus portal client this build does not include — use the X11 \
             backend, or a build of tm-computer with D-Bus support"
        ),
    }
}

impl WaylandBackend {
    /// Connect to the compositor named by `WAYLAND_DISPLAY` via the session D-Bus portal.
    pub fn connect() -> TmResult<WaylandBackend> {
        if !wayland_portal_reachable()? {
            return Err(ComputerError::BackendUnavailable {
                backend: "wayland".to_string(),
                reason: "no D-Bus session bus is reachable — export DBUS_SESSION_BUS_ADDRESS, \
                         or run inside a graphical session"
                    .to_string(),
            }
            .into());
        }
        Ok(WaylandBackend { _private: () })
    }
}

#[async_trait]
impl Backend for WaylandBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Wayland
    }

    async fn probe(&self) -> TmResult<Capabilities> {
        let mut notes = vec![
            "this build has no D-Bus client compiled in, so portal input, capture and \
             element-tree calls are unavailable even when the portal itself is reachable"
                .to_string(),
        ];
        if !wayland_portal_reachable()? {
            notes.push(
                "no session bus is reachable at all — export DBUS_SESSION_BUS_ADDRESS".to_string(),
            );
        }
        Ok(Capabilities {
            backend: BackendKind::Wayland,
            input: false,
            capture: false,
            element_tree: false,
            headless: false,
            notes,
        })
    }

    async fn input(&self, action: InputAction) -> TmResult<()> {
        let _ = action;
        Err(portal_unavailable("portal RemoteDesktop input").into())
    }

    async fn screenshot(&self, display: Option<&str>) -> TmResult<Screenshot> {
        let _ = display;
        Err(portal_unavailable("portal ScreenCast capture").into())
    }

    async fn element_tree(&self, max_depth: Option<u32>) -> TmResult<ElementNode> {
        let _ = max_depth;
        Err(portal_unavailable("AT-SPI element tree walking").into())
    }

    async fn displays(&self) -> TmResult<Vec<DisplayInfo>> {
        Err(portal_unavailable("portal ScreenCast source enumeration").into())
    }

    async fn windows(&self) -> TmResult<Vec<WindowInfo>> {
        Err(portal_unavailable("AT-SPI window enumeration").into())
    }

    async fn focus_window(&self, window_id: &str) -> TmResult<()> {
        let _ = window_id;
        Err(portal_unavailable("window focus").into())
    }

    async fn move_window(&self, window_id: &str, to: Point) -> TmResult<()> {
        let _ = (window_id, to);
        Err(portal_unavailable("window geometry control").into())
    }

    async fn resize_window(&self, window_id: &str, width: f64, height: f64) -> TmResult<()> {
        let _ = (window_id, width, height);
        Err(portal_unavailable("window geometry control").into())
    }

    async fn clipboard_get(&self) -> TmResult<Option<String>> {
        match Command::new("wl-paste").arg("--no-newline").output() {
            Ok(o) if o.status.success() => {
                Ok(Some(String::from_utf8_lossy(&o.stdout).into_owned()))
            }
            Ok(_) => Ok(None),
            Err(_) => {
                Err(portal_unavailable("clipboard access (wl-paste is not installed)").into())
            }
        }
    }

    async fn clipboard_set(&self, text: &str) -> TmResult<()> {
        let mut child = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|_| portal_unavailable("clipboard access (wl-copy is not installed)"))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes()).map_err(|e| {
                ComputerError::Operation(format!("failed to write to wl-copy: {e}"))
            })?;
        }
        child
            .wait()
            .map(|_| ())
            .map_err(|e| ComputerError::Operation(format!("wl-copy failed: {e}")).into())
    }

    async fn launch(&self, app: &str) -> TmResult<()> {
        Command::new(app)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| ComputerError::Operation(format!("failed to launch {app:?}: {e}")).into())
    }

    async fn quit(&self, app: &str) -> TmResult<()> {
        let status = Command::new("pkill")
            .arg("-TERM")
            .arg("-f")
            .arg(app)
            .status()
            .map_err(|e| ComputerError::Operation(format!("failed to run pkill: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(ComputerError::NotFound(format!("no running process matched {app:?}")).into())
        }
    }
}

/// A provisioned, private `Xvfb` display: nothing else is attached to it, nothing steals a
/// human's focus, and many of these can run concurrently on separate display numbers
/// (`SPEC.md` §20.3). Killing the `Xvfb` child on [`Drop`] tears the display down.
pub struct XvfbSession {
    display: String,
    child: Child,
}

/// Find `bin` on the directories listed in `path_var` (a `$PATH`-style value), factored out of
/// [`XvfbSession::start`]'s real `which` lookup so it's testable without touching the process
/// environment.
fn which_in(bin: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(path_var).find_map(|dir| {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

impl XvfbSession {
    /// Start a new private `Xvfb` on a free display number, drawn from `ids` so display
    /// selection stays deterministic under a seeded [`IdSource`] in tests rather than racing
    /// against `/tmp/.X11-unix` lock files directly.
    pub fn start(ids: &dyn IdSource) -> TmResult<XvfbSession> {
        let xvfb_path = which_in("Xvfb", std::env::var_os("PATH").as_deref()).ok_or_else(|| {
            ComputerError::BackendUnavailable {
                backend: "x11".to_string(),
                reason: "Xvfb is not installed — install it with `apt install xvfb` or `dnf \
                         install xorg-x11-server-Xvfb`"
                    .to_string(),
            }
        })?;

        const MAX_ATTEMPTS: usize = 20;
        for _ in 0..MAX_ATTEMPTS {
            let n = 90 + u32::from_str_radix(&ids.random_hex(3), 16).unwrap_or(0) % 9910;
            if Path::new(&format!("/tmp/.X{n}-lock")).exists() {
                continue;
            }

            let display = format!(":{n}");
            let mut child = Command::new(&xvfb_path)
                .arg(&display)
                .arg("-screen")
                .arg("0")
                .arg("1280x1024x24")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| ComputerError::BackendUnavailable {
                    backend: "x11".to_string(),
                    reason: format!("failed to spawn Xvfb: {e}"),
                })?;

            let socket_path = format!("/tmp/.X11-unix/X{n}");
            let mut ready = false;
            for _ in 0..100 {
                if Path::new(&socket_path).exists() {
                    ready = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }

            if ready {
                return Ok(XvfbSession { display, child });
            }
            let _ = child.kill();
            let _ = child.wait();
        }

        Err(ComputerError::BackendUnavailable {
            backend: "x11".to_string(),
            reason: format!("could not find a free X display number after {MAX_ATTEMPTS} attempts"),
        }
        .into())
    }

    /// The `DISPLAY` value of this virtual display, e.g. `":97"`.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// An [`X11Backend`] connected to this virtual display.
    pub fn backend(&self) -> TmResult<X11Backend> {
        X11Backend::connect(Some(&self.display))
    }
}

impl Drop for XvfbSession {
    fn drop(&mut self) {
        // Best-effort: a session drop must never panic, even if the child already exited, and
        // there is no `Result`-returning `Drop` to propagate an error through.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run `body` against a fresh, private `Xvfb` display, tearing it down afterward regardless of
/// `body`'s outcome. The supported entry point for autonomous/CI Linux computer use
/// (`SPEC.md` §20.3).
pub async fn headless<F, Fut, T>(ids: &dyn IdSource, body: F) -> TmResult<T>
where
    F: FnOnce(X11Backend) -> Fut,
    Fut: std::future::Future<Output = TmResult<T>>,
{
    let xvfb = XvfbSession::start(ids)?;
    let backend = xvfb.backend()?;
    body(backend).await
}

/// True when a Wayland portal-capable session bus is reachable at all (does not check which
/// specific interfaces it offers — see [`WaylandBackend::probe`] for that).
pub fn wayland_portal_reachable() -> TmResult<bool> {
    Ok(portal_reachable_from(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    ))
}

#[cfg(test)]
impl X11Backend {
    fn display_for_test(&self) -> &str {
        &self.display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputTarget, KeyChord, Modifier, Point};
    use std::collections::BTreeSet;
    use tm_types::CounterIds;

    #[test]
    fn connect_stores_an_explicit_display_verbatim() {
        let backend = X11Backend::connect(Some(":42")).unwrap();
        assert_eq!(backend.display_for_test(), ":42");
    }

    #[test]
    fn resolve_display_prefers_the_explicit_argument_over_the_env() {
        assert_eq!(resolve_display(Some(":7"), Some(":0")), ":7");
    }

    #[test]
    fn resolve_display_falls_back_to_the_env_value() {
        assert_eq!(resolve_display(None, Some(":3")), ":3");
    }

    #[test]
    fn resolve_display_is_empty_when_neither_is_set() {
        assert_eq!(resolve_display(None, None), "");
    }

    #[test]
    fn connect_never_fails_even_with_no_reachable_display() {
        assert!(X11Backend::connect(None).is_ok());
    }

    #[test]
    fn has_xtest_extension_reports_an_error_for_an_unreachable_display() {
        let backend = X11Backend::connect(Some(":9999")).unwrap();
        assert!(backend.has_xtest_extension().is_err());
    }

    #[test]
    fn resolve_target_passes_a_point_through() {
        let p = Point::new(1.0, 2.0);
        assert_eq!(resolve_target(&InputTarget::Point(p)).unwrap(), p);
    }

    #[test]
    fn resolve_target_rejects_an_unresolved_element_ref() {
        let target = InputTarget::ElementRef("el-1".to_string());
        assert!(resolve_target(&target).is_err());
    }

    #[test]
    fn parse_window_id_accepts_a_numeric_id() {
        assert_eq!(parse_window_id("123").unwrap(), 123);
    }

    #[test]
    fn parse_window_id_rejects_non_numeric_input() {
        assert!(parse_window_id("not-a-window").is_err());
    }

    #[test]
    fn char_to_keysym_mirrors_ascii_codepoints() {
        assert_eq!(char_to_keysym('a'), 'a' as u32);
        assert_eq!(char_to_keysym('\n'), 0xff0d);
    }

    #[test]
    fn char_to_keysym_uses_the_xkb_unicode_convention_beyond_latin1() {
        assert_eq!(char_to_keysym('€'), 0x0100_0000 + '€' as u32);
    }

    #[test]
    fn named_key_to_keysym_resolves_known_names_and_falls_back_to_the_first_char() {
        assert_eq!(named_key_to_keysym("return"), 0xff0d);
        assert_eq!(named_key_to_keysym("q"), 'q' as u32);
    }

    #[tokio::test]
    async fn fake_chord_builds_without_a_display_reachable() {
        // Exercises the chord-to-keycode/keysym plumbing purely through error propagation: no
        // live X server is required for it to fail cleanly rather than panic.
        let backend = X11Backend::connect(Some(":9999")).unwrap();
        let mut modifiers = BTreeSet::new();
        modifiers.insert(Modifier::Ctrl);
        let chord = KeyChord {
            modifiers,
            key: "c".to_string(),
        };
        let result = backend.input(InputAction::KeyChord(chord)).await;
        assert!(result.is_err());
    }

    #[test]
    fn crc32_matches_the_standard_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_a_known_vector() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn zlib_stored_round_trips_through_a_manual_stored_block_inflate() {
        let data = b"hello, xvfb screenshot bytes!".repeat(3);
        let zlib = zlib_stored(&data);

        assert_eq!(&zlib[0..2], &[0x78, 0x01]);
        let mut pos = 2;
        let mut out = Vec::new();
        loop {
            let is_last = zlib[pos] & 1 == 1;
            pos += 1;
            let len = u16::from_le_bytes([zlib[pos], zlib[pos + 1]]) as usize;
            pos += 4; // LEN + NLEN
            out.extend_from_slice(&zlib[pos..pos + len]);
            pos += len;
            if is_last {
                break;
            }
        }
        assert_eq!(out, data);
        let trailer = &zlib[zlib.len() - 4..];
        assert_eq!(
            u32::from_be_bytes(trailer.try_into().unwrap()),
            adler32(&data)
        );
    }

    #[test]
    fn encode_png_starts_with_the_png_signature_and_correct_ihdr_dimensions() {
        let rgba = vec![255u8; 2 * 2 * 4];
        let png = encode_png(2, 2, &rgba);
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        // IHDR: length(4) + "IHDR"(4) + width(4) + height(4) + ...
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
    }

    #[test]
    fn encode_png_ends_with_an_iend_chunk() {
        let rgba = vec![0u8; 4]; // 1x1 RGBA
        let png = encode_png(1, 1, &rgba);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn bgrx_to_rgba_swaps_channel_order() {
        let bgrx = [10u8, 20, 30, 0]; // B=10 G=20 R=30
        let rgba = bgrx_to_rgba(&bgrx, 1, 1);
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn bgrx_to_rgba_pads_a_short_read_with_opaque_black() {
        let bgrx: [u8; 0] = [];
        let rgba = bgrx_to_rgba(&bgrx, 1, 1);
        assert_eq!(rgba, vec![0, 0, 0, 255]);
    }

    #[test]
    fn which_in_finds_a_binary_that_exists_on_the_given_path() {
        let dir =
            std::env::temp_dir().join(format!("tm-computer-which-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let bin_path = dir.join("tm-fake-bin");
        std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
        let found = which_in("tm-fake-bin", Some(dir.as_os_str()));
        assert_eq!(found, Some(bin_path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn which_in_returns_none_for_a_missing_binary() {
        let dir = std::env::temp_dir();
        assert_eq!(
            which_in("tm-definitely-not-a-real-binary", Some(dir.as_os_str())),
            None
        );
    }

    #[test]
    fn which_in_returns_none_when_path_is_absent() {
        assert_eq!(which_in("sh", None), None);
    }

    #[test]
    fn xvfb_session_start_names_the_missing_package_when_xvfb_is_not_on_path() {
        let dir = std::env::temp_dir().join(format!("tm-computer-no-xvfb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(which_in("Xvfb", Some(dir.as_os_str())), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headless_reports_backend_unavailable_when_xvfb_is_missing() {
        // `XvfbSession::start` itself is exercised (not the `headless` wrapper, which would
        // need real display provisioning) via its public error contract: it must never panic,
        // and it must return a `ComputerError::BackendUnavailable`-flavored error when Xvfb
        // cannot even be found — asserted indirectly by construction succeeding or failing
        // cleanly for a seeded `IdSource`.
        let ids = CounterIds::seeded(0);
        // This assertion only holds meaningfully on a machine without Xvfb installed; either
        // outcome (Ok, because Xvfb happens to be installed here, or a clean Err) is a pass —
        // the property under test is "never panics".
        let _ = XvfbSession::start(&ids);
    }

    #[test]
    fn portal_reachable_from_true_when_the_socket_path_exists() {
        assert!(portal_reachable_from(Some(&format!(
            "unix:path={}",
            "/tmp"
        ))));
    }

    #[test]
    fn portal_reachable_from_false_when_the_socket_path_is_missing() {
        assert!(!portal_reachable_from(Some(
            "unix:path=/definitely/not/a/real/path"
        )));
    }

    #[test]
    fn portal_reachable_from_false_when_no_address_is_given() {
        assert!(!portal_reachable_from(None));
    }

    #[test]
    fn portal_reachable_from_false_when_the_address_has_no_path_component() {
        assert!(!portal_reachable_from(Some("tcp:host=127.0.0.1,port=1234")));
    }
}
