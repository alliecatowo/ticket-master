//! The macOS backend: `CGEvent` input injection, display capture, and an `AXUIElement` tree
//! walk, with precise TCC permission detection.
//!
//! macOS has no supported virtual display (`SPEC.md` §20.3): `CGEvent` posts and screen capture
//! both target the active login session, so every [`MacosBackend`] session is attended. This
//! module never offers a headless entry point — that only exists in [`crate::linux`].
//!
//! Everything below the safe wrapper crates (`core-graphics`, `core-foundation`,
//! `objc2`/`objc2-app-kit`) still has to reach a handful of C APIs those crates don't cover —
//! `AXUIElement*`, `AXIsProcessTrustedWithOptions`, `CGPreflightScreenCaptureAccess`,
//! `CGWindowListCopyWindowInfo`, and `ImageIO`'s image destination — so those are declared and
//! called directly against the system frameworks, confined to small, named helper modules.
//!
//! `unsafe` is otherwise denied crate-wide (`crate::lib`); it is allowed only in this module,
//! only for the raw C FFI above that no safe wrapper crate covers.

#![allow(unsafe_code)]

use async_trait::async_trait;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGSize};
use core_graphics::image::CGImage;
use foreign_types::ForeignType;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString, NSWorkspace};
use objc2_foundation::NSString;
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use tm_types::Result as TmResult;

use crate::backend::{
    Backend, BackendKind, Capabilities, DisplayInfo, ElementNode, Screenshot, WindowInfo,
};
use crate::input::{
    lerp_drag_path, InputAction, InputTarget, Modifier, MouseButton, Point, Rect, ScrollDelta,
};
use crate::ComputerError;

/// The two TCC grants computer use on macOS depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TccPermission {
    /// Lets an app synthesize input events and walk the accessibility tree.
    Accessibility,
    /// Lets an app capture the screen.
    ScreenRecording,
}

impl TccPermission {
    /// The exact System Settings path that grants this permission, e.g.
    /// `"System Settings > Privacy & Security > Accessibility"`.
    pub fn settings_path(&self) -> &'static str {
        match self {
            TccPermission::Accessibility => "System Settings > Privacy & Security > Accessibility",
            TccPermission::ScreenRecording => {
                "System Settings > Privacy & Security > Screen Recording"
            }
        }
    }

    /// The human name used in error messages.
    pub fn label(&self) -> &'static str {
        match self {
            TccPermission::Accessibility => "Accessibility",
            TccPermission::ScreenRecording => "Screen Recording",
        }
    }
}

/// The narrow slice of `ApplicationServices`/`CoreGraphics`/`ImageIO` C entry points this module
/// needs and that no dependency wraps safely: TCC queries, the `AXUIElement` tree, window
/// listing, and PNG encoding.
mod sys {
    use core_foundation::array::CFArrayRef;
    use core_foundation::base::{CFAllocatorRef, CFTypeRef};
    use core_foundation::dictionary::CFDictionaryRef;
    use core_foundation::string::CFStringRef;
    use std::ffi::c_void;
    use std::os::raw::c_int;

    /// Opaque `AXUIElementRef`. Never dereferenced directly — only ever passed back into the
    /// Accessibility API, and released with `CFRelease` once retained by us.
    #[repr(C)]
    pub struct OpaqueAxUiElement(c_void);
    pub type AXUIElementRef = *const OpaqueAxUiElement;

    pub const K_AX_VALUE_CG_POINT_TYPE: u32 = 1;
    pub const K_AX_VALUE_CG_SIZE_TYPE: u32 = 2;

    pub const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
    pub const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    pub const K_CG_NULL_WINDOW_ID: u32 = 0;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        pub fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        pub fn AXUIElementCreateApplication(pid: c_int) -> AXUIElementRef;
        pub fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        pub fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> i32;
        pub fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> bool;
        pub fn AXValueCreate(the_type: u32, value_ptr: *const c_void) -> CFTypeRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGPreflightScreenCaptureAccess() -> bool;
        pub fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFDataCreateMutable(allocator: CFAllocatorRef, capacity: isize) -> CFTypeRef;
    }

    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        pub fn CGImageDestinationCreateWithData(
            data: CFTypeRef,
            image_type: CFStringRef,
            count: usize,
            options: CFTypeRef,
        ) -> CFTypeRef;
        pub fn CGImageDestinationAddImage(
            destination: CFTypeRef,
            image: CFTypeRef,
            properties: CFTypeRef,
        );
        pub fn CGImageDestinationFinalize(destination: CFTypeRef) -> bool;
    }
}

/// An owned, refcounted `AXUIElementRef`: released on drop so a tree walk or window lookup never
/// leaks Accessibility objects.
struct AxElement(sys::AXUIElementRef);

impl Drop for AxElement {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { core_foundation::base::CFRelease(self.0 as CFTypeRef) };
        }
    }
}

/// Check whether this process currently holds the Accessibility TCC grant.
pub fn check_accessibility_permission() -> TmResult<bool> {
    // The "prompt" key is present and false so this stays a pure query, never a side-effecting
    // prompt dialog.
    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::false_value();
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
    let trusted =
        unsafe { sys::AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as *const _) };
    Ok(trusted)
}

/// Check whether this process currently holds the Screen Recording TCC grant.
pub fn check_screen_recording_permission() -> TmResult<bool> {
    let granted = unsafe { sys::CGPreflightScreenCaptureAccess() };
    Ok(granted)
}

/// Build a [`ComputerError::PermissionMissing`] for `permission`, with its exact fix path.
pub fn permission_missing_error(permission: TccPermission) -> ComputerError {
    ComputerError::PermissionMissing {
        permission: permission.label().to_string(),
        fix_path: permission.settings_path().to_string(),
    }
}

/// Copy an `AXUIElement` attribute by name; `None` when the element has no such attribute or the
/// copy fails, since a missing attribute is routine (not every role has `AXTitle`, etc.).
fn ax_copy_attribute(element: sys::AXUIElementRef, attribute: &str) -> Option<CFTypeRef> {
    let cf_attr = CFString::new(attribute);
    let mut value: CFTypeRef = std::ptr::null();
    let status = unsafe {
        sys::AXUIElementCopyAttributeValue(element, cf_attr.as_concrete_TypeRef(), &mut value)
    };
    if status == 0 && !value.is_null() {
        Some(value)
    } else {
        None
    }
}

/// Read an already-retained `CFTypeRef` known to hold a `CFString` back into an owned `String`.
fn cfstring_from_owned_ref(value: CFTypeRef) -> String {
    let cf_string: CFString = unsafe { CFString::wrap_under_create_rule(value as CFStringRef) };
    cf_string.to_string()
}

/// Read an element's `AXPosition` + `AXSize` into a [`Rect`], when both are present.
fn ax_bounds(element: sys::AXUIElementRef) -> Option<Rect> {
    let pos_ref = ax_copy_attribute(element, "AXPosition")?;
    let size_ref = ax_copy_attribute(element, "AXSize")?;

    let mut point = CGPoint::new(0.0, 0.0);
    let mut size = CGSize::new(0.0, 0.0);
    let point_ok = unsafe {
        sys::AXValueGetValue(
            pos_ref,
            sys::K_AX_VALUE_CG_POINT_TYPE,
            &mut point as *mut CGPoint as *mut std::ffi::c_void,
        )
    };
    let size_ok = unsafe {
        sys::AXValueGetValue(
            size_ref,
            sys::K_AX_VALUE_CG_SIZE_TYPE,
            &mut size as *mut CGSize as *mut std::ffi::c_void,
        )
    };
    unsafe {
        core_foundation::base::CFRelease(pos_ref);
        core_foundation::base::CFRelease(size_ref);
    }

    if point_ok && size_ok {
        Some(Rect::new(
            Point::new(point.x, point.y),
            size.width,
            size.height,
        ))
    } else {
        None
    }
}

/// Walk one `AXUIElement` subtree into an [`ElementNode`], assigning each node a `ref_id` from
/// `counter` and recording geometry into `cache` for later `ElementRef` resolution.
fn ax_walk(
    element: sys::AXUIElementRef,
    depth: u32,
    max_depth: Option<u32>,
    counter: &mut u64,
    cache: &mut HashMap<String, Rect>,
) -> ElementNode {
    *counter += 1;
    let ref_id = format!("ax-{counter}");

    let role = ax_copy_attribute(element, "AXRole")
        .map(cfstring_from_owned_ref)
        .unwrap_or_else(|| "unknown".to_string());
    let name = ax_copy_attribute(element, "AXTitle")
        .map(cfstring_from_owned_ref)
        .or_else(|| ax_copy_attribute(element, "AXDescription").map(cfstring_from_owned_ref));

    let bounds = ax_bounds(element);
    if let Some(b) = bounds {
        cache.insert(ref_id.clone(), b);
    }

    let mut children = Vec::new();
    let at_limit = max_depth.map(|m| depth >= m).unwrap_or(false);
    if !at_limit {
        if let Some(children_ref) = ax_copy_attribute(element, "AXChildren") {
            let array: CFArray<CFType> =
                unsafe { CFArray::wrap_under_create_rule(children_ref as CFArrayRef) };
            for child in array.iter() {
                let child_element = child.as_CFTypeRef() as sys::AXUIElementRef;
                children.push(ax_walk(child_element, depth + 1, max_depth, counter, cache));
            }
        }
    }

    ElementNode {
        ref_id,
        role,
        name,
        bounds,
        children,
    }
}

fn mouse_button_to_cg(button: MouseButton) -> CGMouseButton {
    match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
    }
}

fn mouse_event_pair(button: MouseButton) -> (CGEventType, CGEventType) {
    match button {
        MouseButton::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp),
        MouseButton::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp),
        MouseButton::Middle => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp),
    }
}

fn drag_event_type(button: MouseButton) -> CGEventType {
    match button {
        MouseButton::Left => CGEventType::LeftMouseDragged,
        MouseButton::Right => CGEventType::RightMouseDragged,
        MouseButton::Middle => CGEventType::OtherMouseDragged,
    }
}

fn modifiers_to_flags(modifiers: &BTreeSet<Modifier>) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for m in modifiers {
        flags |= match m {
            Modifier::Ctrl => CGEventFlags::CGEventFlagControl,
            Modifier::Alt => CGEventFlags::CGEventFlagAlternate,
            Modifier::Shift => CGEventFlags::CGEventFlagShift,
            Modifier::Cmd => CGEventFlags::CGEventFlagCommand,
        };
    }
    flags
}

/// Map a [`crate::input::KeyChord::key`] name to its macOS virtual keycode. Covers letters,
/// digits, the arrow/function keys, and the common named keys; anything else is
/// `ComputerError::Operation`, since the caller passed a key this backend doesn't know.
fn key_to_keycode(key: &str) -> TmResult<CGKeyCode> {
    let code: CGKeyCode = match key {
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0B,
        "q" => 0x0C,
        "w" => 0x0D,
        "e" => 0x0E,
        "r" => 0x0F,
        "y" => 0x10,
        "t" => 0x11,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "6" => 0x16,
        "5" => 0x17,
        "9" => 0x19,
        "7" => 0x1A,
        "8" => 0x1C,
        "0" => 0x1D,
        "o" => 0x1F,
        "u" => 0x20,
        "i" => 0x22,
        "p" => 0x23,
        "l" => 0x25,
        "j" => 0x26,
        "k" => 0x28,
        "n" => 0x2D,
        "m" => 0x2E,
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "space" => 0x31,
        "delete" | "backspace" => 0x33,
        "escape" | "esc" => 0x35,
        "left" => 0x7B,
        "right" => 0x7C,
        "down" => 0x7D,
        "up" => 0x7E,
        "f1" => 0x7A,
        "f2" => 0x78,
        "f3" => 0x63,
        "f4" => 0x76,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f8" => 0x64,
        "f9" => 0x65,
        "f10" => 0x6D,
        "f11" => 0x67,
        "f12" => 0x6F,
        other => {
            return Err(ComputerError::Operation(format!(
                "no macOS keycode mapping for key {other:?}"
            ))
            .into())
        }
    };
    Ok(code)
}

fn new_event_source() -> TmResult<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| ComputerError::Operation("CGEventSourceCreate failed".to_string()).into())
}

fn post_mouse_event(point: Point, event_type: CGEventType, button: CGMouseButton) -> TmResult<()> {
    let source = new_event_source()?;
    let cg_point = CGPoint::new(point.x, point.y);
    let event = CGEvent::new_mouse_event(source, event_type, cg_point, button)
        .map_err(|_| ComputerError::Operation("CGEventCreateMouseEvent failed".to_string()))?;
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn post_key_event(keycode: CGKeyCode, key_down: bool, flags: CGEventFlags) -> TmResult<()> {
    let source = new_event_source()?;
    let event = CGEvent::new_keyboard_event(source, keycode, key_down)
        .map_err(|_| ComputerError::Operation("CGEventCreateKeyboardEvent failed".to_string()))?;
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn post_unicode_text(text: &str) -> TmResult<()> {
    let source = new_event_source()?;
    let event = CGEvent::new_keyboard_event(source, 0, true)
        .map_err(|_| ComputerError::Operation("CGEventCreateKeyboardEvent failed".to_string()))?;
    event.set_string(text);
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn post_scroll_event(delta: ScrollDelta) -> TmResult<()> {
    let source = new_event_source()?;
    let event = CGEvent::new_scroll_event(
        source,
        ScrollEventUnit::PIXEL,
        2,
        delta.dy as i32,
        delta.dx as i32,
        0,
    )
    .map_err(|_| ComputerError::Operation("CGEventCreateScrollWheelEvent failed".to_string()))?;
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn encode_png(image: &CGImage) -> TmResult<Vec<u8>> {
    let mutable_data = unsafe { sys::CFDataCreateMutable(std::ptr::null(), 0) };
    if mutable_data.is_null() {
        return Err(ComputerError::Operation("CFDataCreateMutable failed".to_string()).into());
    }

    let png_type = CFString::new("public.png");
    let destination = unsafe {
        sys::CGImageDestinationCreateWithData(
            mutable_data,
            png_type.as_concrete_TypeRef(),
            1,
            std::ptr::null(),
        )
    };
    if destination.is_null() {
        unsafe { core_foundation::base::CFRelease(mutable_data) };
        return Err(ComputerError::Operation(
            "CGImageDestinationCreateWithData failed".to_string(),
        )
        .into());
    }

    unsafe {
        sys::CGImageDestinationAddImage(destination, image.as_ptr() as CFTypeRef, std::ptr::null());
    }

    let finalized = unsafe { sys::CGImageDestinationFinalize(destination) };
    unsafe { core_foundation::base::CFRelease(destination) };

    if !finalized {
        unsafe { core_foundation::base::CFRelease(mutable_data) };
        return Err(
            ComputerError::Operation("CGImageDestinationFinalize failed".to_string()).into(),
        );
    }

    let data: core_foundation::data::CFData = unsafe {
        core_foundation::data::CFData::wrap_under_create_rule(
            mutable_data as core_foundation::data::CFDataRef,
        )
    };
    Ok(data.bytes().to_vec())
}

fn dict_get_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

fn dict_get_number(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<f64> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_f64())
}

fn dict_get_bounds(dict: &CFDictionary<CFString, CFType>) -> Option<Rect> {
    let key = CFString::new("kCGWindowBounds");
    let bounds_ref = dict.find(&key)?;
    // `CFDictionary<K, V>` only implements `ConcreteCFType` (and so `CFType::downcast`) for its
    // void-pointer instantiation, so a nested `CFString`/`CFType`-keyed dictionary value has to
    // be re-wrapped from its raw ref instead of downcast directly.
    let bounds_dict: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_get_rule(bounds_ref.as_CFTypeRef() as CFDictionaryRef) };
    let x = dict_get_number(&bounds_dict, "X")?;
    let y = dict_get_number(&bounds_dict, "Y")?;
    let w = dict_get_number(&bounds_dict, "Width")?;
    let h = dict_get_number(&bounds_dict, "Height")?;
    Some(Rect::new(Point::new(x, y), w, h))
}

fn pid_for_app_name(app: &str) -> TmResult<i32> {
    let workspace = NSWorkspace::sharedWorkspace();
    let running = workspace.runningApplications();
    for running_app in running.iter() {
        let local_name = running_app.localizedName();
        if local_name.map(|n| n.to_string()).as_deref() == Some(app) {
            return Ok(running_app.processIdentifier());
        }
    }
    Err(ComputerError::NotFound(format!("no running application named {app}")).into())
}

/// The macOS [`Backend`]: attended-only, per `SPEC.md` §20.3.
#[derive(Debug, Default)]
pub struct MacosBackend {
    _private: (),
    /// `ref_id` -> bounds from the last [`Backend::element_tree`] walk, so an
    /// `InputTarget::ElementRef` can resolve to a point without re-walking the tree.
    element_cache: Mutex<HashMap<String, Rect>>,
}

impl MacosBackend {
    /// Construct a backend handle. Cheap: no permission checks or OS handles are acquired until
    /// [`Backend::probe`] or an action is attempted.
    pub fn new() -> Self {
        MacosBackend {
            _private: (),
            element_cache: Mutex::new(HashMap::new()),
        }
    }

    fn resolve_target(&self, target: &InputTarget) -> TmResult<Point> {
        match target {
            InputTarget::Point(p) => Ok(*p),
            InputTarget::ElementRef(id) => {
                let cache = self.element_cache.lock().map_err(|_| {
                    ComputerError::Operation("element cache lock poisoned".to_string())
                })?;
                cache.get(id).copied().map(|r| r.origin).ok_or_else(|| {
                    ComputerError::NotFound(format!(
                        "element ref {id} not found in the last element_tree snapshot"
                    ))
                    .into()
                })
            }
        }
    }

    /// Resolve a [`WindowInfo::id`] (a `CGWindowID` string) to its live `AXUIElement`, by
    /// cross-referencing a fresh window list against the owning app's `AXWindows`.
    async fn resolve_ax_window(&self, window_id: &str) -> TmResult<AxElement> {
        let windows = self.windows().await?;
        let info = windows
            .iter()
            .find(|w| w.id == window_id)
            .ok_or_else(|| ComputerError::NotFound(format!("window {window_id} not found")))?;

        let pid = pid_for_app_name(&info.app_name)?;
        let app_ref = unsafe { sys::AXUIElementCreateApplication(pid) };
        if app_ref.is_null() {
            return Err(ComputerError::Operation(format!(
                "AXUIElementCreateApplication failed for pid {pid}"
            ))
            .into());
        }
        let app = AxElement(app_ref);

        let children_ref = ax_copy_attribute(app.0, "AXWindows").ok_or_else(|| {
            ComputerError::NotFound(format!("no AXWindows attribute for {}", info.app_name))
        })?;
        let array: CFArray<CFType> =
            unsafe { CFArray::wrap_under_create_rule(children_ref as CFArrayRef) };

        for child in array.iter() {
            let candidate = child.as_CFTypeRef() as sys::AXUIElementRef;
            if let Some(bounds) = ax_bounds(candidate) {
                let close_enough = (bounds.origin.x - info.bounds.origin.x).abs() < 1.0
                    && (bounds.origin.y - info.bounds.origin.y).abs() < 1.0;
                if close_enough {
                    unsafe { core_foundation::base::CFRetain(candidate as CFTypeRef) };
                    return Ok(AxElement(candidate));
                }
            }
        }

        Err(ComputerError::NotFound(format!(
            "could not match window {window_id} to an AXUIElement"
        ))
        .into())
    }
}

#[async_trait]
impl Backend for MacosBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Macos
    }

    async fn probe(&self) -> TmResult<Capabilities> {
        let accessibility = check_accessibility_permission()?;
        let screen_recording = check_screen_recording_permission()?;

        let mut notes = Vec::new();
        if !accessibility {
            notes.push(permission_missing_error(TccPermission::Accessibility).to_string());
        }
        if !screen_recording {
            notes.push(permission_missing_error(TccPermission::ScreenRecording).to_string());
        }
        notes.push(
            "headless is unavailable on macOS: CGEvent injection and screen capture both target \
             the active login session"
                .to_string(),
        );

        Ok(Capabilities {
            backend: BackendKind::Macos,
            input: accessibility,
            capture: screen_recording,
            element_tree: accessibility,
            headless: false,
            notes,
        })
    }

    async fn input(&self, action: InputAction) -> TmResult<()> {
        if !check_accessibility_permission()? {
            return Err(permission_missing_error(TccPermission::Accessibility).into());
        }

        match action {
            InputAction::Click { target, button } => {
                let p = self.resolve_target(&target)?;
                let cg_button = mouse_button_to_cg(button);
                let (down, up) = mouse_event_pair(button);
                post_mouse_event(p, down, cg_button)?;
                post_mouse_event(p, up, cg_button)?;
            }
            InputAction::DoubleClick { target } => {
                let p = self.resolve_target(&target)?;
                for _ in 0..2 {
                    post_mouse_event(p, CGEventType::LeftMouseDown, CGMouseButton::Left)?;
                    post_mouse_event(p, CGEventType::LeftMouseUp, CGMouseButton::Left)?;
                }
            }
            InputAction::RightClick { target } => {
                let p = self.resolve_target(&target)?;
                post_mouse_event(p, CGEventType::RightMouseDown, CGMouseButton::Right)?;
                post_mouse_event(p, CGEventType::RightMouseUp, CGMouseButton::Right)?;
            }
            InputAction::Drag { from, to } => {
                let from_p = self.resolve_target(&from)?;
                let to_p = self.resolve_target(&to)?;
                post_mouse_event(from_p, CGEventType::LeftMouseDown, CGMouseButton::Left)?;
                for p in lerp_drag_path(from_p, to_p, 10) {
                    post_mouse_event(p, drag_event_type(MouseButton::Left), CGMouseButton::Left)?;
                }
                post_mouse_event(to_p, CGEventType::LeftMouseUp, CGMouseButton::Left)?;
            }
            InputAction::TypeText(text) => {
                post_unicode_text(&text)?;
            }
            InputAction::KeyChord(chord) => {
                let keycode = key_to_keycode(&chord.key)?;
                let flags = modifiers_to_flags(&chord.modifiers);
                post_key_event(keycode, true, flags)?;
                post_key_event(keycode, false, flags)?;
            }
            InputAction::Scroll { target, delta } => {
                // The point is resolved (and validated) even though CGEvent scroll posts target
                // whatever has focus, so a stale ElementRef still surfaces as NotFound.
                let _ = self.resolve_target(&target)?;
                post_scroll_event(delta)?;
            }
        }

        Ok(())
    }

    async fn screenshot(&self, display: Option<&str>) -> TmResult<Screenshot> {
        if !check_screen_recording_permission()? {
            return Err(permission_missing_error(TccPermission::ScreenRecording).into());
        }

        let displays = self.displays().await?;
        let target = match display {
            Some(id) => displays
                .iter()
                .find(|d| d.id == id)
                .ok_or_else(|| ComputerError::NotFound(format!("display {id} not found")))?,
            None => displays
                .iter()
                .find(|d| d.primary)
                .ok_or_else(|| ComputerError::Operation("no primary display found".to_string()))?,
        };

        let display_id: u32 = target.id.parse().map_err(|_| {
            ComputerError::Operation(format!(
                "display id {} is not a valid CGDirectDisplayID",
                target.id
            ))
        })?;

        let cg_display = CGDisplay::new(display_id);
        let image = cg_display.image().ok_or_else(|| {
            ComputerError::Operation("CGDisplayCreateImage returned no image".to_string())
        })?;

        let png_bytes = encode_png(&image)?;

        Ok(Screenshot {
            png_bytes,
            bounds: target.bounds,
        })
    }

    async fn element_tree(&self, max_depth: Option<u32>) -> TmResult<ElementNode> {
        if !check_accessibility_permission()? {
            return Err(permission_missing_error(TccPermission::Accessibility).into());
        }

        let system_wide_ref = unsafe { sys::AXUIElementCreateSystemWide() };
        if system_wide_ref.is_null() {
            return Err(ComputerError::BackendUnavailable {
                backend: "macos".to_string(),
                reason: "AXUIElementCreateSystemWide returned null".to_string(),
            }
            .into());
        }
        let system_wide = AxElement(system_wide_ref);

        let root = match ax_copy_attribute(system_wide.0, "AXFocusedApplication") {
            Some(focused_ref) => AxElement(focused_ref as sys::AXUIElementRef),
            None => {
                unsafe { core_foundation::base::CFRetain(system_wide.0 as CFTypeRef) };
                AxElement(system_wide.0)
            }
        };

        let mut counter = 0u64;
        let mut cache = HashMap::new();
        let tree = ax_walk(root.0, 0, max_depth, &mut counter, &mut cache);

        if let Ok(mut guard) = self.element_cache.lock() {
            *guard = cache;
        }

        Ok(tree)
    }

    async fn displays(&self) -> TmResult<Vec<DisplayInfo>> {
        let active = CGDisplay::active_displays()
            .map_err(|_| ComputerError::Operation("CGGetActiveDisplayList failed".to_string()))?;
        let main_id = CGDisplay::main().id;

        let mut out = Vec::with_capacity(active.len());
        for id in active {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            out.push(DisplayInfo {
                id: id.to_string(),
                bounds: Rect::new(
                    Point::new(bounds.origin.x, bounds.origin.y),
                    bounds.size.width,
                    bounds.size.height,
                ),
                primary: id == main_id,
            });
        }
        Ok(out)
    }

    async fn windows(&self) -> TmResult<Vec<WindowInfo>> {
        let options = sys::K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY
            | sys::K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
        let array_ref =
            unsafe { sys::CGWindowListCopyWindowInfo(options, sys::K_CG_NULL_WINDOW_ID) };
        if array_ref.is_null() {
            return Err(ComputerError::Operation(
                "CGWindowListCopyWindowInfo returned null".to_string(),
            )
            .into());
        }
        let array: CFArray<CFDictionary<CFString, CFType>> =
            unsafe { CFArray::wrap_under_create_rule(array_ref) };

        let mut out = Vec::new();
        for dict in array.iter() {
            let app_name = dict_get_string(&dict, "kCGWindowOwnerName").unwrap_or_default();
            let title = dict_get_string(&dict, "kCGWindowName");
            let number = dict_get_number(&dict, "kCGWindowNumber").unwrap_or(0.0) as i64;
            let layer = dict_get_number(&dict, "kCGWindowLayer").unwrap_or(0.0);
            let bounds =
                dict_get_bounds(&dict).unwrap_or_else(|| Rect::new(Point::new(0.0, 0.0), 0.0, 0.0));

            out.push(WindowInfo {
                id: number.to_string(),
                app_name,
                title,
                bounds,
                // Layer 0 is the normal window layer; the frontmost normal-layer window is first
                // in CGWindowListCopyWindowInfo's front-to-back ordering.
                focused: layer == 0.0,
            });
        }
        Ok(out)
    }

    async fn focus_window(&self, window_id: &str) -> TmResult<()> {
        if !check_accessibility_permission()? {
            return Err(permission_missing_error(TccPermission::Accessibility).into());
        }
        let window = self.resolve_ax_window(window_id).await?;
        let attr = CFString::new("AXMain");
        let status = unsafe {
            sys::AXUIElementSetAttributeValue(
                window.0,
                attr.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(ComputerError::Operation(format!(
                "AXUIElementSetAttributeValue(AXMain) failed with status {status}"
            ))
            .into())
        }
    }

    async fn move_window(&self, window_id: &str, to: Point) -> TmResult<()> {
        if !check_accessibility_permission()? {
            return Err(permission_missing_error(TccPermission::Accessibility).into());
        }
        let window = self.resolve_ax_window(window_id).await?;
        let point = CGPoint::new(to.x, to.y);
        let value = unsafe {
            sys::AXValueCreate(
                sys::K_AX_VALUE_CG_POINT_TYPE,
                &point as *const CGPoint as *const std::ffi::c_void,
            )
        };
        if value.is_null() {
            return Err(
                ComputerError::Operation("AXValueCreate for position failed".to_string()).into(),
            );
        }
        let attr = CFString::new("AXPosition");
        let status = unsafe {
            sys::AXUIElementSetAttributeValue(window.0, attr.as_concrete_TypeRef(), value)
        };
        unsafe { core_foundation::base::CFRelease(value) };
        if status == 0 {
            Ok(())
        } else {
            Err(ComputerError::Operation(format!(
                "AXUIElementSetAttributeValue(AXPosition) failed with status {status}"
            ))
            .into())
        }
    }

    async fn resize_window(&self, window_id: &str, width: f64, height: f64) -> TmResult<()> {
        if !check_accessibility_permission()? {
            return Err(permission_missing_error(TccPermission::Accessibility).into());
        }
        let window = self.resolve_ax_window(window_id).await?;
        let size = CGSize::new(width, height);
        let value = unsafe {
            sys::AXValueCreate(
                sys::K_AX_VALUE_CG_SIZE_TYPE,
                &size as *const CGSize as *const std::ffi::c_void,
            )
        };
        if value.is_null() {
            return Err(
                ComputerError::Operation("AXValueCreate for size failed".to_string()).into(),
            );
        }
        let attr = CFString::new("AXSize");
        let status = unsafe {
            sys::AXUIElementSetAttributeValue(window.0, attr.as_concrete_TypeRef(), value)
        };
        unsafe { core_foundation::base::CFRelease(value) };
        if status == 0 {
            Ok(())
        } else {
            Err(ComputerError::Operation(format!(
                "AXUIElementSetAttributeValue(AXSize) failed with status {status}"
            ))
            .into())
        }
    }

    async fn clipboard_get(&self) -> TmResult<Option<String>> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let value = unsafe { pasteboard.stringForType(NSPasteboardTypeString) };
        Ok(value.map(|s| s.to_string()))
    }

    async fn clipboard_set(&self, text: &str) -> TmResult<()> {
        let pasteboard = NSPasteboard::generalPasteboard();
        unsafe {
            pasteboard.clearContents();
            let ns_text = NSString::from_str(text);
            pasteboard.setString_forType(&ns_text, NSPasteboardTypeString);
        }
        Ok(())
    }

    async fn launch(&self, app: &str) -> TmResult<()> {
        let workspace = NSWorkspace::sharedWorkspace();
        let name = NSString::from_str(app);
        // `launchApplication:` is deprecated in favor of the async, URL-based
        // `openApplicationAtURL:configuration:completionHandler:`; kept here for its simpler
        // synchronous, name-based contract, which matches this trait's signature.
        #[allow(deprecated)]
        let launched = workspace.launchApplication(&name);
        if launched {
            Ok(())
        } else {
            Err(ComputerError::NotFound(format!("could not launch application {app}")).into())
        }
    }

    async fn quit(&self, app: &str) -> TmResult<()> {
        let workspace = NSWorkspace::sharedWorkspace();
        let running = workspace.runningApplications();
        for running_app in running.iter() {
            let local_name = running_app.localizedName();
            if local_name.map(|n| n.to_string()).as_deref() == Some(app) {
                let terminated = running_app.terminate();
                return if terminated {
                    Ok(())
                } else {
                    Err(ComputerError::Operation(format!("failed to terminate {app}")).into())
                };
            }
        }
        Err(ComputerError::NotFound(format!("no running application named {app}")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessibility_settings_path_names_the_exact_pane() {
        assert_eq!(
            TccPermission::Accessibility.settings_path(),
            "System Settings > Privacy & Security > Accessibility"
        );
    }

    #[test]
    fn screen_recording_label_is_human_readable() {
        assert_eq!(TccPermission::ScreenRecording.label(), "Screen Recording");
    }

    #[test]
    fn permission_missing_error_carries_the_exact_fix_path() {
        let err = permission_missing_error(TccPermission::Accessibility);
        match err {
            ComputerError::PermissionMissing {
                permission,
                fix_path,
            } => {
                assert_eq!(permission, "Accessibility");
                assert!(fix_path.ends_with("Accessibility"));
            }
            other => panic!("expected PermissionMissing, got {other:?}"),
        }
    }

    #[test]
    fn key_to_keycode_resolves_letters_and_named_keys() {
        assert_eq!(key_to_keycode("a").unwrap(), 0x00);
        assert_eq!(key_to_keycode("return").unwrap(), 0x24);
        assert_eq!(key_to_keycode("enter").unwrap(), 0x24);
        assert_eq!(key_to_keycode("f5").unwrap(), 0x60);
    }

    #[test]
    fn key_to_keycode_rejects_an_unmapped_key() {
        assert!(key_to_keycode("not-a-real-key").is_err());
    }

    #[test]
    fn new_backend_starts_with_an_empty_element_cache() {
        let backend = MacosBackend::new();
        assert!(backend.element_cache.lock().unwrap().is_empty());
    }

    #[test]
    fn kind_reports_macos() {
        assert_eq!(MacosBackend::new().kind(), BackendKind::Macos);
    }

    #[test]
    fn mouse_event_pair_matches_the_requested_button() {
        let (down, up) = mouse_event_pair(MouseButton::Right);
        assert!(matches!(down, CGEventType::RightMouseDown));
        assert!(matches!(up, CGEventType::RightMouseUp));
    }
}
