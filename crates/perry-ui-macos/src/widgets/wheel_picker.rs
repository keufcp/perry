//! macOS WheelPicker (issue #5873).
//!
//! AppKit has no wheel control — there is no `UIPickerView` equivalent, and
//! `NSPopUpButton` is already what `Picker` maps to. So this is a custom
//! `NSView` that draws the column via CoreGraphics in `drawRect:` and turns
//! scroll/click/arrow input into row changes, the same shape as the Chart and
//! Canvas widgets.
//!
//! Motion model: `scrollWheel:` accumulates `scrollingDeltaY`; each whole
//! `ROW_HEIGHT` of travel becomes one row step, and the leftover sub-row travel
//! stays in `accum`. The column is DRAWN from that continuous position, so it
//! turns under a trackpad instead of jumping between settled rows, while the
//! selection itself only ever moves a whole row at a time.
//!
//! `onChange` reports the SETTLED selection, once per gesture — not every row
//! the drum passed on the way. `accum` is zeroed at the same moment (or
//! immediately for a notched mouse wheel, which has no gesture), so the wheel
//! never comes to rest between two rows.

use crate::ffi::CGContextAddLineToPoint;
use crate::ffi::CGContextBeginPath;
use crate::ffi::CGContextFillRect;
use crate::ffi::CGContextMoveToPoint;
use crate::ffi::CGContextSetLineWidth;
use crate::ffi::CGContextSetRGBFillColor;
use crate::ffi::CGContextSetRGBStrokeColor;
use crate::ffi::CGContextStrokePath;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{define_class, DefinedClass, MainThreadOnly};
use objc2_app_kit::NSView;
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{MainThreadMarker, NSString};
use std::cell::{Cell, RefCell};

extern "C" {
    fn js_closure_call1(closure: *const u8, arg: f64) -> f64;
    fn js_nanbox_get_pointer(value: f64) -> i64;
}

const ROW_HEIGHT: f64 = 28.0;
const VISIBLE_ROWS: i64 = 5;

struct WheelEntry {
    handle: i64,
    items: Vec<String>,
    selected: i64,
    /// Sub-row scroll travel not yet converted into a step.
    accum: f64,
    /// Last index handed to the app. `onChange` reports a CHOICE, not the rows
    /// that flew past on the way to it, so it fires when the gesture settles —
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

    /// `NSFont` weight is a -1.0..1.0 scale, not the CSS 100..900 one.
    /// `NSFontWeightRegular` is 0.0 and `NSFontWeightBold` is 0.4.
    fn ns_weight(&self) -> f64 {
        match self.font_weight {
            Some(w) if w >= 600.0 => 0.4,
            _ => 0.0,
        }
    }
}

thread_local! {
    static WHEELS: RefCell<Vec<WheelEntry>> = RefCell::new(Vec::new());
}

fn with_entry<R>(handle: i64, f: impl FnOnce(&mut WheelEntry) -> R) -> Option<R> {
    WHEELS.with(|w| {
        let mut wheels = w.borrow_mut();
        wheels.iter_mut().find(|e| e.handle == handle).map(f)
    })
}

/// Apply a row delta and redraw. Does NOT report — see `report_if_settled`.
fn step_selection(handle: i64, delta: i64) {
    if delta == 0 {
        return;
    }
    with_entry(handle, |e| {
        if e.items.is_empty() {
            return;
        }
        let max = e.items.len() as i64 - 1;
        e.selected = (e.selected + delta).clamp(0, max);
    });

    if let Some(view) = get_view(handle) {
        unsafe {
            let _: () = msg_send![&*view, setNeedsDisplay: true];
        }
    }
}

/// Hand the settled selection to the app, once.
///
/// The closure is invoked AFTER the `WHEELS` borrow is released: it re-enters
/// the JS runtime, which can call back into `wheelPickerSetSelected` on the
/// same widget, and a live `borrow_mut` would panic.
fn report_if_settled(handle: i64) {
    let fired = with_entry(handle, |e| {
        if e.items.is_empty() || e.selected == e.reported {
            return None;
        }
        e.reported = e.selected;
        if e.on_change == 0.0 {
            return None;
        }
        Some((e.on_change, e.selected))
    })
    .flatten();

    if let Some((closure, index)) = fired {
        crate::catch_callback_panic(
            "wheelpicker callback",
            std::panic::AssertUnwindSafe(|| unsafe {
                let ptr = js_nanbox_get_pointer(closure) as *const u8;
                js_closure_call1(ptr, index as f64);
            }),
        );
    }
}

fn get_view(handle: i64) -> Option<Retained<NSView>> {
    super::get_widget(handle)
}

pub struct PerryWheelPickerViewIvars {
    pub handle: Cell<i64>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "PerryWheelPickerView"]
    #[ivars = PerryWheelPickerViewIvars]
    pub struct PerryWheelPickerView;

    impl PerryWheelPickerView {
        // Flipped: row 0 at the top, y growing downward. The row maths below
        // assume this; without it every index would have to be mirrored.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: CGRect) {
            let handle = self.ivars().handle.get();
            let snapshot = WHEELS.with(|w| {
                w.borrow()
                    .iter()
                    .find(|e| e.handle == handle)
                    .map(|e| (e.items.clone(), e.selected, e.accum, e.style))
            });
            let Some((items, selected, accum, style)) = snapshot else {
                return;
            };
            unsafe {
                let bounds: CGRect = msg_send![self, bounds];
                draw_wheel(&items, selected, accum, &style, bounds);
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &AnyObject) {
            let handle = self.ivars().handle.get();
            let dy: CGFloat = unsafe { msg_send![event, scrollingDeltaY] };
            let phase: u64 = unsafe { msg_send![event, phase] };
            let momentum: u64 = unsafe { msg_send![event, momentumPhase] };
            // Positive deltaY is content moving down, i.e. earlier rows.
            let steps = with_entry(handle, |e| {
                e.accum += dy as f64;
                let steps = (e.accum / ROW_HEIGHT) as i64;
                if steps != 0 {
                    e.accum -= steps as f64 * ROW_HEIGHT;
                }
                steps
            })
            .unwrap_or(0);
            step_selection(handle, -steps);

            // Settle back onto the grid, or the wheel sits permanently between
            // two rows. NSEventPhaseNone (0 for both) is a notched mouse
            // wheel — discrete, so settle at once; a trackpad instead reports
            // Ended (8) or Cancelled (16) when the gesture finishes.
            const ENDED_OR_CANCELLED: u64 = 8 | 16;
            let finished = (phase == 0 && momentum == 0)
                || phase & ENDED_OR_CANCELLED != 0
                || momentum & ENDED_OR_CANCELLED != 0;
            if finished {
                with_entry(handle, |e| e.accum = 0.0);
                // The gesture is over: this is the choice.
                report_if_settled(handle);
            }
            // Redraw even when no whole row was crossed: the sub-row remainder
            // is what makes the column move rather than step.
            if let Some(view) = get_view(handle) {
                unsafe {
                    let _: () = msg_send![&*view, setNeedsDisplay: true];
                }
            }
        }

        // Click a visible row to select it — the desktop equivalent of
        // flicking the drum to a neighbour.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &AnyObject) {
            let handle = self.ivars().handle.get();
            unsafe {
                let win_pt: CGPoint = msg_send![event, locationInWindow];
                let local: CGPoint =
                    msg_send![self, convertPoint: win_pt, fromView: std::ptr::null::<AnyObject>()];
                let bounds: CGRect = msg_send![self, bounds];
                let centre = bounds.size.height / 2.0;
                let offset = (local.y as f64) - centre;
                let rows = (offset / ROW_HEIGHT).round() as i64;
                step_selection(handle, rows);
                // A click lands settled — there is no gesture to wait out.
                report_if_settled(handle);
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &AnyObject) {
            let handle = self.ivars().handle.get();
            let code: u16 = unsafe { msg_send![event, keyCode] };
            match code {
                // A keystroke is a discrete, settled step.
                126 => {
                    step_selection(handle, -1); // Up
                    report_if_settled(handle);
                }
                125 => {
                    step_selection(handle, 1); // Down
                    report_if_settled(handle);
                }
                _ => {}
            }
        }
    }
);

impl PerryWheelPickerView {
    fn new(handle: i64, frame: CGRect) -> Retained<Self> {
        let mtm = MainThreadMarker::new().expect("perry/ui must run on the main thread");
        let this = Self::alloc(mtm).set_ivars(PerryWheelPickerViewIvars {
            handle: Cell::new(handle),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

unsafe fn current_cg_context() -> *mut std::ffi::c_void {
    let ctx_cls = AnyClass::get(c"NSGraphicsContext").unwrap();
    let cur: *mut AnyObject = msg_send![ctx_cls, currentContext];
    if cur.is_null() {
        return std::ptr::null_mut();
    }
    msg_send![cur, CGContext]
}

unsafe fn draw_wheel(
    items: &[String],
    selected: i64,
    accum: f64,
    style: &WheelStyle,
    bounds: CGRect,
) {
    let ctx = current_cg_context();
    if ctx.is_null() {
        return;
    }
    let h = bounds.size.height as f64;
    let w = bounds.size.width as f64;
    let centre = h / 2.0;

    // Backing.
    CGContextSetRGBFillColor(ctx, 1.0, 1.0, 1.0, 1.0);
    CGContextFillRect(ctx, bounds);

    // Rows are placed from the CONTINUOUS scroll position — the snapped row
    // plus the sub-row travel still held in `accum` — not from the snapped
    // index alone. Drawing `selected + offset` renders only settled states, so
    // the column teleports between rows and nothing moves under a trackpad
    // scroll: a stepper, not a drum. One extra row each way covers the halves
    // that a partial offset brings into view. `accum` is ADDED here (unlike
    // the GTK4 impl, which subtracts): positive `scrollingDeltaY` moves the
    // content down, i.e. toward earlier rows.
    let half = VISIBLE_ROWS / 2 + 1;
    for offset in -half..=half {
        let index = selected + offset;
        if index < 0 || index as usize >= items.len() {
            continue;
        }
        let row_centre = centre + (offset as f64) * ROW_HEIGHT + accum;
        // Distance from the selection band in rows, fractional, so the
        // emphasis crossfades as the wheel turns instead of snapping.
        let dist = (row_centre - centre).abs() / ROW_HEIGHT;
        let rect = CGRect::new(
            CGPoint::new(0.0, row_centre - ROW_HEIGHT / 2.0),
            CGSize::new(w, ROW_HEIGHT),
        );
        draw_text_centered(
            &items[index as usize],
            rect,
            style.size_for(dist),
            style.ns_weight(),
            style.color_for(dist),
        );
    }

    // Selection band.
    CGContextSetRGBStrokeColor(ctx, 0.62, 0.62, 0.65, 1.0);
    CGContextSetLineWidth(ctx, 1.0);
    for edge in [centre - ROW_HEIGHT / 2.0, centre + ROW_HEIGHT / 2.0] {
        CGContextBeginPath(ctx);
        CGContextMoveToPoint(ctx, 4.0, edge);
        CGContextAddLineToPoint(ctx, w - 4.0, edge);
        CGContextStrokePath(ctx);
    }
}

unsafe fn draw_text_centered(
    text: &str,
    rect: CGRect,
    size: f64,
    weight: f64,
    rgba: (f64, f64, f64, f64),
) {
    let ns_text = NSString::from_str(text);

    let font_cls = AnyClass::get(c"NSFont").unwrap();
    let font: *mut AnyObject =
        msg_send![font_cls, systemFontOfSize: size as CGFloat, weight: weight as CGFloat];

    let para_cls = AnyClass::get(c"NSMutableParagraphStyle").unwrap();
    let para: *mut AnyObject = msg_send![para_cls, new];
    let _: () = msg_send![para, setAlignment: 1i64]; // NSTextAlignmentCenter

    let dict_cls = AnyClass::get(c"NSMutableDictionary").unwrap();
    let attrs: *mut AnyObject = msg_send![dict_cls, new];
    let font_key = NSString::from_str("NSFont");
    let para_key = NSString::from_str("NSParagraphStyle");
    let color_key = NSString::from_str("NSColor");
    let _: () = msg_send![attrs, setObject: font, forKey: &*font_key];
    let _: () = msg_send![attrs, setObject: para, forKey: &*para_key];

    let color_cls = AnyClass::get(c"NSColor").unwrap();
    let (r, g, b, a) = rgba;
    let color: *mut AnyObject = msg_send![
        color_cls,
        colorWithCalibratedRed: r as CGFloat,
        green: g as CGFloat,
        blue: b as CGFloat,
        alpha: a as CGFloat
    ];
    let _: () = msg_send![attrs, setObject: color, forKey: &*color_key];

    let _: () = msg_send![&*ns_text, drawInRect: rect, withAttributes: attrs];
}

// ===========================================================================
// Public API.
// ===========================================================================

pub fn create(on_change: f64) -> i64 {
    let frame = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(90.0, ROW_HEIGHT * VISIBLE_ROWS as f64),
    );
    let provisional = WHEELS.with(|w| w.borrow().len() as i64 + 1);
    let view = PerryWheelPickerView::new(provisional, frame);
    let cast: Retained<NSView> = unsafe { Retained::cast_unchecked(view) };
    let handle = super::register_widget(cast);

    // `register_widget` owns handle allocation; align the view's ivar to the
    // real handle (same dance as chart.rs).
    if handle != provisional {
        if let Some(view) = super::get_widget(handle) {
            unsafe {
                let typed = Retained::as_ptr(&view) as *const PerryWheelPickerView;
                (*typed).ivars().handle.set(handle);
            }
        }
    }

    WHEELS.with(|w| {
        w.borrow_mut().push(WheelEntry {
            handle,
            items: Vec::new(),
            selected: -1,
            accum: 0.0,
            reported: -1,
            on_change,
            style: WheelStyle::new(),
        })
    });
    handle
}

pub fn add_item(handle: i64, title_ptr: *const u8) {
    let title = str_from_header(title_ptr).to_string();
    with_entry(handle, |e| {
        e.items.push(title);
        if e.selected < 0 {
            e.selected = 0;
            // Populating the wheel is never a user choice, so the resulting
            // selection must not read as one at the next settle.
            e.reported = 0;
        }
    });
    if let Some(view) = get_view(handle) {
        unsafe {
            let _: () = msg_send![&*view, setNeedsDisplay: true];
        }
    }
}

pub fn set_selected(handle: i64, index: i64) {
    // Programmatic: no onChange. Only `step_selection` fires the closure.
    let ok = with_entry(handle, |e| {
        if index < 0 || index as usize >= e.items.len() {
            return false;
        }
        e.selected = index;
        e.accum = 0.0;
        // Programmatic: not a choice, but it becomes the baseline the next
        // settle is compared against.
        e.reported = index;
        true
    })
    .unwrap_or(false);
    if ok {
        if let Some(view) = get_view(handle) {
            unsafe {
                let _: () = msg_send![&*view, setNeedsDisplay: true];
            }
        }
    }
}

fn with_style(handle: i64, f: impl FnOnce(&mut WheelStyle)) {
    let found = with_entry(handle, |e| f(&mut e.style)).is_some();
    if found {
        if let Some(view) = get_view(handle) {
            unsafe {
                let _: () = msg_send![&*view, setNeedsDisplay: true];
            }
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
    with_entry(handle, |e| if e.items.is_empty() { -1 } else { e.selected }).unwrap_or(-1)
}

fn str_from_header(ptr: *const u8) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    unsafe {
        let header = ptr as *const crate::string_header::StringHeader;
        let len = (*header).byte_len as usize;
        let data = ptr.add(std::mem::size_of::<crate::string_header::StringHeader>());
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(data, len))
    }
}
