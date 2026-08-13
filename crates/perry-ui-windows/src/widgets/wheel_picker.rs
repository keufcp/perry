//! Windows WheelPicker (issue #5873).
//!
//! Win32 has no wheel control — `COMBOBOX` is already what `Picker` maps to.
//! So this is an owner-draw window class drawing the column with GDI in
//! `WM_PAINT`, the same shape as the Chart and ScrollView widgets.
//!
//! Motion model matches the macOS and GTK4 impls: `WM_MOUSEWHEEL` travel
//! accumulates, each whole `WHEEL_DELTA` becomes one row step, and the leftover
//! sub-notch travel stays in `accum`. The column is DRAWN from that continuous
//! position, so a precision touchpad turns it instead of stepping it, while the
//! selection only ever moves a whole row at a time.
//!
//! `onChange` reports the SETTLED selection, once — not every row the drum
//! passed on the way. Win32 has no scroll-end message, so a short settle timer
//! is what decides the wheel has gone quiet; it also zeroes `accum` so the
//! column never rests off-grid.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Once;

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::*;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::*;
#[cfg(target_os = "windows")]
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{alloc_control_id, register_widget, WidgetKind};

extern "C" {
    fn js_closure_call1(closure: *const u8, arg: f64) -> f64;
    fn js_nanbox_get_pointer(value: f64) -> i64;
}

const ROW_HEIGHT: i32 = 28;
const VISIBLE_ROWS: i32 = 5;

/// Timer that settles the wheel back onto the grid once the wheel goes quiet.
const SETTLE_TIMER_ID: usize = 0x5873;

struct WheelEntry {
    items: Vec<String>,
    selected: i64,
    /// Sub-notch wheel travel not yet converted into a step.
    accum: i32,
    /// Last index handed to the app. `onChange` reports a CHOICE, not the rows
    /// that flew past on the way to it, so it fires when the wheel settles —
    /// matching `Picker`, and matching what `UIPickerView` can actually
    /// deliver, which is what lets the contract be identical on every backend.
    reported: i64,
    on_change: f64,
    style: WheelStyle,
}

/// Row typography. `None` means "app never set this", so the widget keeps its
/// own default rather than being forced to a value the app did not choose.
#[derive(Clone, Copy)]
struct WheelStyle {
    font_size: Option<f64>,
    font_weight: Option<f64>,
    text_color: Option<(f64, f64, f64)>,
    selected_color: Option<(f64, f64, f64)>,
}

impl WheelStyle {
    const DEFAULT_SELECTED_SIZE: f64 = 15.0;
    const DEFAULT_ROW_SIZE: f64 = 13.0;

    fn new() -> Self {
        Self {
            font_size: None,
            font_weight: None,
            text_color: None,
            selected_color: None,
        }
    }

    /// Point size for a row `dist` rows from the selection band — fractional,
    /// because the wheel is drawn at continuous positions. Without an app font
    /// size the row inside the band is drawn slightly larger, which is the
    /// depth cue that makes an undecorated column read as a drum; once the app
    /// picks a size, every row uses it so the text does not jump as it scrolls.
    fn size_for(&self, dist: f64) -> f64 {
        match self.font_size {
            Some(size) => size,
            None if dist < 0.5 => Self::DEFAULT_SELECTED_SIZE,
            None => Self::DEFAULT_ROW_SIZE,
        }
    }

    /// GDI text has no alpha, so distance is expressed as a grey ramp when the
    /// app has not chosen colours; the macOS and GTK4 impls fade instead. An
    /// explicit colour is used as given, and its alpha is dropped rather than
    /// silently approximated.
    fn color_for(&self, dist: f64) -> (u8, u8, u8) {
        let selected = dist < 0.5;
        let base = if selected {
            self.selected_color.or(self.text_color)
        } else {
            self.text_color
        };
        // Taper past the band edge. With no alpha to fade, the ramp runs
        // toward the background instead — which is white, so lighten.
        let fade = (1.0 - (dist - 0.5).max(0.0) * 0.42).clamp(0.25, 1.0);
        let toward_bg = |c: f64| -> u8 {
            let v = c.clamp(0.0, 1.0) * fade + (1.0 - fade);
            (v * 255.0) as u8
        };
        match base {
            Some((r, g, b)) if selected && self.selected_color.is_some() => (
                (r.clamp(0.0, 1.0) * 255.0) as u8,
                (g.clamp(0.0, 1.0) * 255.0) as u8,
                (b.clamp(0.0, 1.0) * 255.0) as u8,
            ),
            Some((r, g, b)) => (toward_bg(r), toward_bg(g), toward_bg(b)),
            None => {
                let v = toward_bg(0.10);
                (v, v, v)
            }
        }
    }

    /// `LOGFONT.lfWeight` uses the same 0..1000 scale as CSS; FW_NORMAL is 400
    /// and FW_BOLD is 700.
    fn gdi_weight(&self) -> i32 {
        match self.font_weight {
            Some(w) => w.clamp(1.0, 1000.0) as i32,
            None => 400,
        }
    }
}

thread_local! {
    static WHEELS: RefCell<HashMap<i64, WheelEntry>> = RefCell::new(HashMap::new());
}

static WHEEL_CLASS_REGISTERED: Once = Once::new();

fn str_from_header(ptr: *const u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe {
        let header = ptr as *const perry_runtime::string::StringHeader;
        let len = (*header).byte_len as usize;
        let data = ptr.add(std::mem::size_of::<perry_runtime::string::StringHeader>());
        String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned()
    }
}

/// Apply a row delta and repaint. Does NOT report — see `report_if_settled`.
fn step_selection(handle: i64, delta: i64) {
    if delta == 0 {
        return;
    }
    WHEELS.with(|w| {
        let mut wheels = w.borrow_mut();
        let Some(entry) = wheels.get_mut(&handle) else {
            return;
        };
        if entry.items.is_empty() {
            return;
        }
        let max = entry.items.len() as i64 - 1;
        entry.selected = (entry.selected + delta).clamp(0, max);
    });

    request_redraw(handle);
}

/// Hand the settled selection to the app, once.
///
/// The closure runs AFTER the `WHEELS` borrow is dropped: it re-enters the JS
/// runtime, which may call back into `wheelPickerSetSelected` for the same
/// widget, and a live `borrow_mut` would panic.
fn report_if_settled(handle: i64) {
    let fired = WHEELS.with(|w| {
        let mut wheels = w.borrow_mut();
        let entry = wheels.get_mut(&handle)?;
        if entry.items.is_empty() || entry.selected == entry.reported {
            return None;
        }
        entry.reported = entry.selected;
        if entry.on_change == 0.0 {
            return None;
        }
        Some((entry.on_change, entry.selected))
    });

    if let Some((closure_f64, index)) = fired {
        unsafe {
            let ptr = js_nanbox_get_pointer(closure_f64) as *const u8;
            js_closure_call1(ptr, index as f64);
        }
    }
}

#[cfg(target_os = "windows")]
fn request_redraw(handle: i64) {
    if let Some(hwnd) = super::get_hwnd(handle) {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn request_redraw(_handle: i64) {}

#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn ensure_class_registered() {
    WHEEL_CLASS_REGISTERED.call_once(|| unsafe {
        let hinstance = GetModuleHandleW(None).unwrap();
        let class_name = to_wide("PerryWheelPicker");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wheel_wnd_proc),
            hInstance: hinstance.into(),
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn wheel_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let handle = super::find_handle_by_hwnd(hwnd);
            if handle > 0 {
                paint_wheel(handle, hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let handle = super::find_handle_by_hwnd(hwnd);
            if handle > 0 {
                // HIWORD(wParam) is a signed notch delta in WHEEL_DELTA units.
                let raw = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let steps = WHEELS.with(|w| {
                    let mut wheels = w.borrow_mut();
                    let Some(entry) = wheels.get_mut(&handle) else {
                        return 0;
                    };
                    entry.accum += raw;
                    let steps = entry.accum / WHEEL_DELTA as i32;
                    if steps != 0 {
                        entry.accum -= steps * WHEEL_DELTA as i32;
                    }
                    steps as i64
                });
                // Wheel-forward (positive) scrolls toward earlier rows.
                step_selection(handle, -steps);
                // Redraw for the sub-notch remainder — that is what makes a
                // precision touchpad turn the column instead of stepping it —
                // and arm the settle timer. Win32 has no scroll-end message,
                // so "the wheel stopped" has to be inferred from a quiet
                // interval, the same way smooth-scrolling list controls do it.
                let _ = InvalidateRect(Some(hwnd), None, true);
                SetTimer(Some(hwnd), SETTLE_TIMER_ID, 120, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == SETTLE_TIMER_ID => {
            let _ = KillTimer(Some(hwnd), SETTLE_TIMER_ID);
            let handle = super::find_handle_by_hwnd(hwnd);
            if handle > 0 {
                WHEELS.with(|w| {
                    if let Some(entry) = w.borrow_mut().get_mut(&handle) {
                        entry.accum = 0;
                    }
                });
                let _ = InvalidateRect(Some(hwnd), None, true);
                // The wheel has gone quiet: this is the choice.
                report_if_settled(handle);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let handle = super::find_handle_by_hwnd(hwnd);
            if handle > 0 {
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let centre = (client.bottom - client.top) / 2;
                let rows = ((y - centre) as f64 / ROW_HEIGHT as f64).round() as i64;
                let _ = SetFocus(Some(hwnd));
                step_selection(handle, rows);
                // A click lands settled — there is no gesture to wait out.
                report_if_settled(handle);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let handle = super::find_handle_by_hwnd(hwnd);
            if handle > 0 {
                // Compare the raw virtual-key codes rather than matching the
                // `VK_*` constants: they live in `UI::Input::KeyboardAndMouse`,
                // not the `WindowsAndMessaging` glob imported here, so as
                // patterns they would silently become catch-all bindings and
                // the first arm would swallow every key.
                const VK_UP_CODE: u16 = 0x26;
                const VK_DOWN_CODE: u16 = 0x28;
                match wparam.0 as u16 {
                    // A keystroke is a discrete, settled step.
                    VK_UP_CODE => {
                        step_selection(handle, -1);
                        report_if_settled(handle);
                    }
                    VK_DOWN_CODE => {
                        step_selection(handle, 1);
                        report_if_settled(handle);
                    }
                    _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
            }
            LRESULT(0)
        }
        // Without this the control never receives WM_KEYDOWN.
        WM_GETDLGCODE => LRESULT((DLGC_WANTARROWS | DLGC_WANTCHARS) as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(target_os = "windows")]
fn paint_wheel(handle: i64, hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let _ = FillRect(hdc, &client, HBRUSH(GetStockObject(WHITE_BRUSH).0));

        let snapshot = WHEELS.with(|w| {
            w.borrow()
                .get(&handle)
                .map(|e| (e.items.clone(), e.selected, e.accum, e.style))
        });

        if let Some((items, selected, accum, style)) = snapshot {
            let width = client.right - client.left;
            let centre = (client.bottom - client.top) / 2;

            SetBkMode(hdc, TRANSPARENT);
            // One font per distinct row size; the selected row may differ from
            // its neighbours. Both are released before the DC is returned.
            let face = to_wide("Segoe UI");
            let mut fonts: Vec<(f64, HFONT)> = Vec::new();
            let mut make_font = |points: f64| -> HFONT {
                if let Some((_, f)) = fonts.iter().find(|(p, _)| *p == points) {
                    return *f;
                }
                // Negative height asks GDI for a CHARACTER height in logical
                // units, which is what a point size means to a caller. Done in
                // Rust rather than via `MulDiv` so the arithmetic keeps its
                // fractional part until the final rounding.
                let dpi_y = GetDeviceCaps(Some(hdc), LOGPIXELSY) as f64;
                let height = -((points * dpi_y / 72.0).round() as i32);
                let f = CreateFontW(
                    height,
                    0,
                    0,
                    0,
                    style.gdi_weight(),
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                    windows::core::PCWSTR(face.as_ptr()),
                );
                fonts.push((points, f));
                f
            };

            // Rows are placed from the CONTINUOUS scroll position — the
            // snapped row plus the sub-row travel still held in `accum` — not
            // from the snapped index alone. Drawing `selected + offset`
            // renders only settled states, so the column teleports between
            // rows and nothing moves under a precision touchpad: a stepper,
            // not a drum. `accum` counts WHEEL_DELTA units, so convert to
            // pixels; it is ADDED because a positive wheel delta scrolls
            // toward earlier rows. One extra row each way covers the halves a
            // partial offset brings into view.
            let accum_px = (accum as f64 / WHEEL_DELTA as f64 * ROW_HEIGHT as f64) as i32;
            let half = (VISIBLE_ROWS / 2) as i64 + 1;
            for offset in -half..=half {
                let index = selected + offset;
                if index < 0 || index as usize >= items.len() {
                    continue;
                }
                let row_centre = centre + (offset as i32) * ROW_HEIGHT + accum_px;
                // Distance from the selection band in rows, fractional, so the
                // emphasis crossfades as the wheel turns instead of snapping.
                let dist = ((row_centre - centre) as f64).abs() / ROW_HEIGHT as f64;
                let (r, g, b) = style.color_for(dist);
                SetTextColor(
                    hdc,
                    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16)),
                );
                let font = make_font(style.size_for(dist));
                let old_font = SelectObject(hdc, font.into());
                let top = row_centre - ROW_HEIGHT / 2;
                let mut row = RECT {
                    left: client.left,
                    top,
                    right: client.left + width,
                    bottom: top + ROW_HEIGHT,
                };
                let mut text = to_wide(&items[index as usize]);
                // to_wide appends NUL; DrawTextW with -1 expects it, but the
                // slice length must not include it.
                let len = text.len() - 1;
                DrawTextW(
                    hdc,
                    &mut text[..len],
                    &mut row,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                SelectObject(hdc, old_font);
            }
            for (_, font) in fonts {
                let _ = DeleteObject(font.into());
            }

            // Selection band.
            let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00A59E9E));
            let old = SelectObject(hdc, pen.into());
            for edge in [centre - ROW_HEIGHT / 2, centre + ROW_HEIGHT / 2] {
                let _ = MoveToEx(hdc, client.left + 4, edge, None);
                let _ = LineTo(hdc, client.right - 4, edge);
            }
            SelectObject(hdc, old);
            let _ = DeleteObject(pen.into());
        }

        let _ = EndPaint(hwnd, &ps);
    }
}

// ===========================================================================
// Public API.
// ===========================================================================

pub fn create(on_change: f64) -> i64 {
    let control_id = alloc_control_id();

    #[cfg(target_os = "windows")]
    {
        ensure_class_registered();
        let class_name = to_wide("PerryWheelPicker");
        unsafe {
            let hinstance = GetModuleHandleW(None).unwrap();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::PCWSTR(class_name.as_ptr()),
                windows::core::PCWSTR(std::ptr::null()),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0),
                0,
                0,
                90,
                ROW_HEIGHT * VISIBLE_ROWS,
                Some(super::get_parking_hwnd()),
                Some(HMENU(control_id as *mut _)),
                Some(HINSTANCE::from(hinstance)),
                None,
            );
            let Ok(hwnd) = hwnd else {
                return register_widget(
                    HWND(std::ptr::null_mut()),
                    WidgetKind::WheelPicker,
                    control_id,
                );
            };
            let handle = register_widget(hwnd, WidgetKind::WheelPicker, control_id);
            WHEELS.with(|w| {
                w.borrow_mut().insert(
                    handle,
                    WheelEntry {
                        items: Vec::new(),
                        selected: -1,
                        accum: 0,
                        reported: -1,
                        on_change,
                        style: WheelStyle::new(),
                    },
                );
            });
            handle
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let handle = register_widget(0, WidgetKind::WheelPicker, control_id);
        WHEELS.with(|w| {
            w.borrow_mut().insert(
                handle,
                WheelEntry {
                    items: Vec::new(),
                    selected: -1,
                    accum: 0,
                    on_change,
                    style: WheelStyle::new(),
                },
            );
        });
        handle
    }
}

pub fn add_item(handle: i64, title_ptr: *const u8) {
    let title = str_from_header(title_ptr);
    WHEELS.with(|w| {
        if let Some(entry) = w.borrow_mut().get_mut(&handle) {
            entry.items.push(title);
            if entry.selected < 0 {
                entry.selected = 0;
                // Populating the wheel is never a user choice, so the
                // resulting selection must not read as one at the next settle.
                entry.reported = 0;
            }
        }
    });
    request_redraw(handle);
}

pub fn set_selected(handle: i64, index: i64) {
    // Programmatic: no onChange. Only `step_selection` fires the closure.
    let ok = WHEELS.with(|w| {
        let mut wheels = w.borrow_mut();
        let Some(entry) = wheels.get_mut(&handle) else {
            return false;
        };
        if index < 0 || index as usize >= entry.items.len() {
            return false;
        }
        entry.selected = index;
        entry.accum = 0;
        // Programmatic: not a choice, but it becomes the baseline the next
        // settle is compared against.
        entry.reported = index;
        true
    });
    if ok {
        request_redraw(handle);
    }
}

fn with_style(handle: i64, f: impl FnOnce(&mut WheelStyle)) {
    let found = WHEELS.with(|w| {
        let mut wheels = w.borrow_mut();
        match wheels.get_mut(&handle) {
            Some(entry) => {
                f(&mut entry.style);
                true
            }
            None => false,
        }
    });
    if found {
        request_redraw(handle);
    }
}

pub fn set_font_size(handle: i64, size: f64) {
    with_style(handle, |s| s.font_size = Some(size));
}

pub fn set_font_weight(handle: i64, weight: f64) {
    with_style(handle, |s| s.font_weight = Some(weight));
}

pub fn set_text_color(handle: i64, r: f64, g: f64, b: f64, _a: f64) {
    with_style(handle, |s| s.text_color = Some((r, g, b)));
}

pub fn set_selected_text_color(handle: i64, r: f64, g: f64, b: f64, _a: f64) {
    with_style(handle, |s| s.selected_color = Some((r, g, b)));
}

pub fn get_selected(handle: i64) -> i64 {
    WHEELS.with(|w| {
        w.borrow()
            .get(&handle)
            .map(|e| if e.items.is_empty() { -1 } else { e.selected })
            .unwrap_or(-1)
    })
}
