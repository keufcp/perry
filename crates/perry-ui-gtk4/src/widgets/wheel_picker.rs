//! GTK4 WheelPicker (issue #5873).
//!
//! GTK has no wheel control — `GtkDropDown` is already what `Picker` maps to,
//! and `GtkScrolledWindow` has no snap-to-item. So this is a `GtkDrawingArea`
//! rendering the column with Cairo, the same shape as the Chart widget.
//!
//! Motion model matches the macOS impl: scroll travel accumulates, each whole
//! `ROW_HEIGHT` becomes one row step, and the leftover sub-row travel stays in
//! `accum`. The column is DRAWN from that continuous position, so it turns
//! under a trackpad instead of jumping between settled rows, while the
//! selection only ever moves a whole row at a time.
//!
//! `onChange` reports the SETTLED selection, once per gesture — not every row
//! the drum passed on the way. `accum` is zeroed at the same moment so the
//! wheel never comes to rest between two rows.

use gtk4::cairo::Context;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;

extern "C" {
    fn js_closure_call1(closure: *const u8, arg: f64) -> f64;
    fn js_nanbox_get_pointer(value: f64) -> i64;
}

const ROW_HEIGHT: f64 = 28.0;
const VISIBLE_ROWS: i64 = 5;

struct WheelEntry {
    items: Vec<String>,
    selected: i64,
    /// Sub-row scroll travel not yet converted into a step.
    accum: f64,
    /// Last index handed to the app. `onChange` reports a CHOICE, not the rows
    /// that flew past on the way to it, so it fires when the gesture settles —
    /// matching `Picker`, and matching what `UIPickerView` can actually
    /// deliver, which is what lets the contract be identical on every backend.
    reported: i64,
    /// Bumped on every scroll event so a queued settle can tell whether it is
    /// still the newest one. `scroll-end` is only emitted by devices that also
    /// emit `scroll-begin` — a notched mouse does not — so waiting for it
    /// alone would mean the callback never fires for mouse users.
    settle_gen: u64,
    on_change: f64,
    style: WheelStyle,
}

/// Row typography. `None` means "app never set this", so the widget keeps its
/// own default rather than being forced to a value the app did not choose.
#[derive(Clone, Copy)]
struct WheelStyle {
    font_size: Option<f64>,
    font_weight: Option<f64>,
    text_color: Option<(f64, f64, f64, f64)>,
    selected_color: Option<(f64, f64, f64, f64)>,
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

    /// Colour for a row `dist` rows from the selection band. Without an app
    /// colour the neighbours fade with distance, continuously.
    fn color_for(&self, dist: f64) -> (f64, f64, f64, f64) {
        let selected = dist < 0.5;
        let base = if selected {
            self.selected_color.or(self.text_color)
        } else {
            self.text_color
        };
        // Taper past the band edge; full opacity inside it.
        let fade = (1.0 - (dist - 0.5).max(0.0) * 0.42).clamp(0.25, 1.0);
        match base {
            Some((r, g, b, a)) if selected && self.selected_color.is_some() => (r, g, b, a),
            Some((r, g, b, a)) => (r, g, b, a * fade),
            None => (0.10, 0.10, 0.10, fade),
        }
    }

    fn cairo_weight(&self) -> gtk4::cairo::FontWeight {
        match self.font_weight {
            Some(w) if w >= 600.0 => gtk4::cairo::FontWeight::Bold,
            _ => gtk4::cairo::FontWeight::Normal,
        }
    }
}

thread_local! {
    static WHEELS: RefCell<HashMap<i64, WheelEntry>> = RefCell::new(HashMap::new());
}

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

/// Apply a row delta and redraw. Does NOT report — see `report_if_settled`.
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

    if let Some(widget) = super::get_widget(handle) {
        widget.queue_draw();
    }
}

/// Come to rest: drop the sub-row offset, redraw on the grid, and report.
fn settle_now(handle: i64) {
    WHEELS.with(|w| {
        if let Some(entry) = w.borrow_mut().get_mut(&handle) {
            entry.accum = 0.0;
        }
    });
    if let Some(widget) = super::get_widget(handle) {
        widget.queue_draw();
    }
    report_if_settled(handle);
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

pub fn create(on_change: f64) -> i64 {
    crate::app::ensure_gtk_init();

    let area = gtk4::DrawingArea::new();
    area.set_content_width(90);
    area.set_content_height((ROW_HEIGHT * VISIBLE_ROWS as f64) as i32);
    area.set_focusable(true);

    let handle = super::register_widget(area.clone().upcast());
    WHEELS.with(|w| {
        w.borrow_mut().insert(
            handle,
            WheelEntry {
                items: Vec::new(),
                selected: -1,
                accum: 0.0,
                reported: -1,
                settle_gen: 0,
                on_change,
                style: WheelStyle::new(),
            },
        );
    });

    area.set_draw_func(move |_area, cr, w, h| {
        let snapshot = WHEELS.with(|wheels| {
            wheels
                .borrow()
                .get(&handle)
                .map(|e| (e.items.clone(), e.selected, e.accum, e.style))
        });
        let Some((items, selected, accum, style)) = snapshot else {
            return;
        };
        draw_wheel(cr, &items, selected, accum, &style, w as f64, h as f64);
    });

    // Scroll → detent steps. BOTH_AXES so a touchpad's horizontal component
    // does not swallow the event before the vertical one is delivered.
    let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
    scroll.connect_scroll(move |_, _dx, dy| {
        let steps = WHEELS.with(|w| {
            let mut wheels = w.borrow_mut();
            let Some(entry) = wheels.get_mut(&handle) else {
                return 0;
            };
            // GTK reports one notch as dy == 1.0; scale it to a row so a
            // notch is a row and smooth-scroll pixels still accumulate.
            entry.accum += dy * ROW_HEIGHT;
            let steps = (entry.accum / ROW_HEIGHT) as i64;
            if steps != 0 {
                entry.accum -= steps as f64 * ROW_HEIGHT;
            }
            steps
        });
        step_selection(handle, steps);
        // Redraw even when no whole row was crossed: the sub-row remainder is
        // what makes the column move under a trackpad instead of stepping.
        if let Some(widget) = super::get_widget(handle) {
            widget.queue_draw();
        }
        // Debounced settle. `scroll-end` below handles touchpads promptly;
        // this is what makes a notched mouse — which emits no scroll-end —
        // still come to rest and report.
        let generation = WHEELS.with(|w| {
            let mut wheels = w.borrow_mut();
            match wheels.get_mut(&handle) {
                Some(entry) => {
                    entry.settle_gen += 1;
                    entry.settle_gen
                }
                None => 0,
            }
        });
        gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
            let current =
                WHEELS.with(|w| w.borrow().get(&handle).map(|e| e.settle_gen).unwrap_or(0));
            if current == generation {
                settle_now(handle);
            }
        });
        gtk4::glib::Propagation::Stop
    });
    // Touchpads announce the end of the gesture, so settle promptly rather
    // than waiting out the debounce above.
    scroll.connect_scroll_end(move |_| settle_now(handle));
    area.add_controller(scroll);

    // Click a visible row to select it — the desktop equivalent of flicking
    // the drum to a neighbour.
    let click = gtk4::GestureClick::new();
    click.connect_pressed(move |gesture, _n, _x, y| {
        let height = gesture
            .widget()
            .map(|w| w.height() as f64)
            .unwrap_or(ROW_HEIGHT * VISIBLE_ROWS as f64);
        let rows = ((y - height / 2.0) / ROW_HEIGHT).round() as i64;
        step_selection(handle, rows);
        // A click lands settled — there is no gesture to wait out.
        report_if_settled(handle);
    });
    area.add_controller(click);

    handle
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
    if let Some(widget) = super::get_widget(handle) {
        widget.queue_draw();
    }
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
        entry.accum = 0.0;
        // Programmatic: not a choice, but it becomes the baseline the
        // next settle is compared against.
        entry.reported = index;
        true
    });
    if ok {
        if let Some(widget) = super::get_widget(handle) {
            widget.queue_draw();
        }
    }
}

fn with_style(handle: i64, f: impl FnOnce(&mut WheelStyle)) {
    let changed = WHEELS.with(|w| {
        let mut wheels = w.borrow_mut();
        match wheels.get_mut(&handle) {
            Some(entry) => {
                f(&mut entry.style);
                true
            }
            None => false,
        }
    });
    if changed {
        if let Some(widget) = super::get_widget(handle) {
            widget.queue_draw();
        }
    }
}

pub fn set_font_size(handle: i64, size: f64) {
    with_style(handle, |s| s.font_size = Some(size));
}

pub fn set_font_weight(handle: i64, weight: f64) {
    with_style(handle, |s| s.font_weight = Some(weight));
}

pub fn set_text_color(handle: i64, r: f64, g: f64, b: f64, a: f64) {
    with_style(handle, |s| s.text_color = Some((r, g, b, a)));
}

pub fn set_selected_text_color(handle: i64, r: f64, g: f64, b: f64, a: f64) {
    with_style(handle, |s| s.selected_color = Some((r, g, b, a)));
}

pub fn get_selected(handle: i64) -> i64 {
    WHEELS.with(|w| {
        w.borrow()
            .get(&handle)
            .map(|e| if e.items.is_empty() { -1 } else { e.selected })
            .unwrap_or(-1)
    })
}

fn draw_wheel(
    cr: &Context,
    items: &[String],
    selected: i64,
    accum: f64,
    style: &WheelStyle,
    w: f64,
    h: f64,
) {
    let centre = h / 2.0;

    cr.set_source_rgb(1.0, 1.0, 1.0);
    let _ = cr.paint();

    cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, style.cairo_weight());

    // Rows are placed from the CONTINUOUS scroll position — the snapped row
    // plus the sub-row travel still held in `accum` — not from the snapped
    // index alone. Drawing `selected + offset` renders only settled states, so
    // the column teleports between rows and nothing moves under a trackpad
    // scroll: a stepper, not a drum. One extra row each way covers the halves
    // that a partial offset brings into view.
    let half = VISIBLE_ROWS / 2 + 1;
    for offset in -half..=half {
        let index = selected + offset;
        if index < 0 || index as usize >= items.len() {
            continue;
        }
        let y = centre + (offset as f64) * ROW_HEIGHT - accum;
        // Distance from the selection band in rows, fractional, so the
        // emphasis crossfades as the wheel turns instead of snapping.
        let dist = (y - centre).abs() / ROW_HEIGHT;
        let size = style.size_for(dist);
        let (r, g, b, a) = style.color_for(dist);
        cr.set_font_size(size);
        cr.set_source_rgba(r, g, b, a);

        let text = &items[index as usize];
        let text_w = cr.text_extents(text).map(|e| e.width()).unwrap_or(0.0);
        cr.move_to((w - text_w) / 2.0, y + size / 3.0);
        let _ = cr.show_text(text);
    }

    // Selection band.
    cr.set_source_rgb(0.62, 0.62, 0.65);
    cr.set_line_width(1.0);
    for edge in [centre - ROW_HEIGHT / 2.0, centre + ROW_HEIGHT / 2.0] {
        cr.move_to(4.0, edge);
        cr.line_to(w - 4.0, edge);
    }
    let _ = cr.stroke();
}
