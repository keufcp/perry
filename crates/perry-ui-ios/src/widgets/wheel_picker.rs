//! WheelPicker — `UIPickerView`, one component (issue #5873).
//!
//! The drum-roll complement to `Picker`, which is a `UISegmentedControl` here
//! and is unusable past ~5 segments. `UIPickerView` supplies snap-to-item,
//! fling deceleration, the selection indicator and VoiceOver rotor behavior
//! from the OS.
//!
//! One object plays both `UIPickerViewDataSource` and `UIPickerViewDelegate`.
//! UIKit resolves both protocols by `respondsToSelector:`, so implementing the
//! selectors is sufficient — the same approach `tree_view.rs` takes for
//! `UITableViewDataSource`/`Delegate`.
//!
//! Rows are supplied as `UILabel`s via `viewForRow:` rather than plain strings
//! via `titleForRow:`. `titleForRow:` hands UIKit a bare `NSString` and gives
//! the app no way to reach the font or the colour, and `attributedTitleForRow:`
//! still cannot style the selected row differently from its neighbours —
//! `wheelPickerSetSelectedTextColor` needs exactly that distinction.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_foundation::{MainThreadMarker, NSObject, NSString};
use objc2_ui_kit::UIView;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

extern "C" {
    fn js_closure_call1(closure: *const u8, arg: f64) -> f64;
    fn js_nanbox_get_pointer(value: f64) -> i64;
    // dispatch_get_main_queue() is a macro; the actual symbol is _dispatch_main_q
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: *const std::ffi::c_void,
        context: *mut std::ffi::c_void,
        work: unsafe extern "C" fn(*mut std::ffi::c_void),
    );
}

/// Row typography. `None` means "app never set this", so the widget keeps the
/// platform look rather than being forced to a value the app did not choose.
#[derive(Clone, Copy)]
struct WheelStyle {
    font_size: Option<f64>,
    font_weight: Option<f64>,
    text_color: Option<(f64, f64, f64, f64)>,
    selected_color: Option<(f64, f64, f64, f64)>,
}

impl WheelStyle {
    /// UIKit's own picker row size.
    const DEFAULT_SIZE: f64 = 21.0;

    fn new() -> Self {
        Self {
            font_size: None,
            font_weight: None,
            text_color: None,
            selected_color: None,
        }
    }

    fn size(&self) -> f64 {
        self.font_size.unwrap_or(Self::DEFAULT_SIZE)
    }

    /// `UIFont`'s weight axis is -1.0..1.0, not the CSS 100..900 scale.
    /// `UIFontWeightRegular` is 0.0 and `UIFontWeightBold` is 0.4.
    fn ui_weight(&self) -> f64 {
        match self.font_weight {
            Some(w) if w >= 600.0 => 0.4,
            _ => 0.0,
        }
    }

    /// UIKit dims the unselected rows itself, so an unstyled wheel needs no
    /// per-row colour; `None` here means "leave UIKit's label colour alone".
    fn color_for(&self, selected: bool) -> Option<(f64, f64, f64, f64)> {
        if selected {
            self.selected_color.or(self.text_color)
        } else {
            self.text_color
        }
    }

    /// Rows must grow with the font or a large size is clipped. The 1.6 factor
    /// is line-height, not padding — it keeps ascenders and descenders inside
    /// the row at any size.
    fn row_height(&self) -> f64 {
        (self.size() * 1.6).max(32.0)
    }
}

struct WheelPickerState {
    /// Row text as Rust strings, and as NSStrings the data source can hand
    /// back without minting a new autoreleased object per row. UIKit asks
    /// during scrolling, so this keeps the hot path allocation-free.
    items: Vec<String>,
    titles: Vec<Retained<NSString>>,
    /// Row views handed to UIKit. Owned here so the pointer returned from
    /// `viewForRow:` outlives the call without depending on autorelease-pool
    /// timing.
    labels: HashMap<i64, Retained<AnyObject>>,
    selected: i64,
    on_change: f64,
    style: WheelStyle,
}

thread_local! {
    static WHEEL_PICKERS: RefCell<HashMap<i64, WheelPickerState>> =
        RefCell::new(HashMap::new());
}

/// Heap payload handed to the main-queue trampoline.
struct WheelDispatch {
    closure_f64: f64,
    index: f64,
}

unsafe extern "C" fn wheel_callback_trampoline(context: *mut std::ffi::c_void) {
    let _ = std::panic::catch_unwind(|| {
        let payload = Box::from_raw(context as *mut WheelDispatch);
        let closure_ptr = js_nanbox_get_pointer(payload.closure_f64);
        js_closure_call1(closure_ptr as *const u8, payload.index);
    });
}

pub struct PerryWheelPickerDelegateIvars {
    handle: Cell<i64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PerryWheelPickerDelegateIOS"]
    #[ivars = PerryWheelPickerDelegateIvars]
    pub struct PerryWheelPickerDelegate;

    impl PerryWheelPickerDelegate {
        // UIPickerViewDataSource: a single spinning column.
        #[unsafe(method(numberOfComponentsInPickerView:))]
        fn number_of_components(&self, _pv: &AnyObject) -> i64 {
            1
        }

        // UIPickerViewDataSource: row count.
        #[unsafe(method(pickerView:numberOfRowsInComponent:))]
        fn number_of_rows(&self, _pv: &AnyObject, _component: i64) -> i64 {
            let handle = self.ivars().handle.get();
            WHEEL_PICKERS.with(|w| {
                w.borrow()
                    .get(&handle)
                    .map(|s| s.items.len() as i64)
                    .unwrap_or(0)
            })
        }

        #[unsafe(method(pickerView:rowHeightForComponent:))]
        fn row_height(&self, _pv: &AnyObject, _component: i64) -> f64 {
            let handle = self.ivars().handle.get();
            WHEEL_PICKERS.with(|w| {
                w.borrow()
                    .get(&handle)
                    .map(|s| s.style.row_height())
                    .unwrap_or(32.0)
            })
        }

        // UIPickerViewDelegate: the row view. Returns a pointer into our own
        // cache rather than an autoreleased temporary — see
        // `WheelPickerState::labels`.
        #[unsafe(method(pickerView:viewForRow:forComponent:reusingView:))]
        fn view_for_row(
            &self,
            _pv: &AnyObject,
            row: i64,
            _component: i64,
            _reusing: *mut AnyObject,
        ) -> *mut AnyObject {
            let handle = self.ivars().handle.get();
            build_row_label(handle, row)
        }

        // UIPickerViewDelegate: selection changed.
        #[unsafe(method(pickerView:didSelectRow:inComponent:))]
        fn did_select_row(&self, pv: &AnyObject, row: i64, _component: i64) {
            let handle = self.ivars().handle.get();
            let on_change = WHEEL_PICKERS.with(|w| {
                let mut wheels = w.borrow_mut();
                let state = wheels.get_mut(&handle)?;
                if row < 0 || row as usize >= state.items.len() {
                    return None;
                }
                state.selected = row;
                if state.on_change == 0.0 {
                    return None;
                }
                Some(state.on_change)
            });
            // Repaint so the selected-row colour follows the selection.
            unsafe {
                let _: () = msg_send![pv, reloadAllComponents];
                let _: () = msg_send![pv, selectRow: row, inComponent: 0i64, animated: false];
            }
            let Some(closure_f64) = on_change else {
                return;
            };
            // Dispatch async to the main queue so the JS runtime is not
            // re-entered synchronously inside UIKit's selection processing —
            // same reason picker.rs does it (avoids iOS-26 crashes).
            let payload = Box::new(WheelDispatch {
                closure_f64,
                index: row as f64,
            });
            unsafe {
                dispatch_async_f(
                    &_dispatch_main_q as *const _ as *const std::ffi::c_void,
                    Box::into_raw(payload) as *mut std::ffi::c_void,
                    wheel_callback_trampoline,
                );
            }
        }
    }
);

impl PerryWheelPickerDelegate {
    fn new(handle: i64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PerryWheelPickerDelegateIvars {
            handle: Cell::new(handle),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Build (or rebuild) the `UILabel` for `row` and return a borrowed pointer to
/// the cached instance.
fn build_row_label(handle: i64, row: i64) -> *mut AnyObject {
    WHEEL_PICKERS.with(|w| {
        let mut wheels = w.borrow_mut();
        let Some(state) = wheels.get_mut(&handle) else {
            return std::ptr::null_mut();
        };
        let Some(title) = state.titles.get(row as usize).cloned() else {
            return std::ptr::null_mut();
        };
        let is_selected = row == state.selected;
        let style = state.style;

        unsafe {
            let cls = objc2::runtime::AnyClass::get(c"UILabel").unwrap();
            let obj: *mut AnyObject = msg_send![cls, alloc];
            let obj: *mut AnyObject = msg_send![obj, init];
            let label = Retained::retain(obj).unwrap();

            let _: () = msg_send![&*label, setText: &*title];
            let _: () = msg_send![&*label, setTextAlignment: 1i64]; // centre

            let font_cls = objc2::runtime::AnyClass::get(c"UIFont").unwrap();
            let font: *mut AnyObject = msg_send![
                font_cls,
                systemFontOfSize: style.size(),
                weight: style.ui_weight()
            ];
            let _: () = msg_send![&*label, setFont: font];

            if let Some((r, g, b, a)) = style.color_for(is_selected) {
                let color_cls = objc2::runtime::AnyClass::get(c"UIColor").unwrap();
                let color: *mut AnyObject =
                    msg_send![color_cls, colorWithRed: r, green: g, blue: b, alpha: a];
                let _: () = msg_send![&*label, setTextColor: color];
            }

            let ptr = Retained::as_ptr(&label) as *mut AnyObject;
            state.labels.insert(row, label);
            ptr
        }
    })
}

fn str_from_header(ptr: *const u8) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    unsafe {
        let header = ptr as *const perry_runtime::string::StringHeader;
        let len = (*header).byte_len as usize;
        let data = ptr.add(std::mem::size_of::<perry_runtime::string::StringHeader>());
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(data, len))
    }
}

/// Drop cached row views and ask UIKit to rebuild them.
fn invalidate_rows(handle: i64) {
    WHEEL_PICKERS.with(|w| {
        if let Some(state) = w.borrow_mut().get_mut(&handle) {
            state.labels.clear();
        }
    });
    if let Some(view) = super::get_widget(handle) {
        unsafe {
            let _: () = msg_send![&*view, reloadAllComponents];
        }
    }
}

pub fn create(on_change: f64) -> i64 {
    let _mtm = MainThreadMarker::new().expect("perry/ui must run on the main thread");
    unsafe {
        let cls = objc2::runtime::AnyClass::get(c"UIPickerView").unwrap();
        let obj: *mut AnyObject = msg_send![cls, alloc];
        let obj: *mut AnyObject = msg_send![obj, init];
        let view: Retained<UIView> = Retained::retain(obj as *mut UIView).unwrap();
        let handle = super::register_widget(view.clone());

        WHEEL_PICKERS.with(|w| {
            w.borrow_mut().insert(
                handle,
                WheelPickerState {
                    items: Vec::new(),
                    titles: Vec::new(),
                    labels: HashMap::new(),
                    selected: -1,
                    on_change,
                    style: WheelStyle::new(),
                },
            );
        });

        // The delegate must outlive the picker; UIKit holds both dataSource
        // and delegate weakly.
        let delegate = PerryWheelPickerDelegate::new(handle);
        let _: () = msg_send![&*view, setDataSource: &*delegate];
        let _: () = msg_send![&*view, setDelegate: &*delegate];
        std::mem::forget(delegate);

        #[cfg(feature = "geisterhand")]
        {
            extern "C" {
                fn perry_geisterhand_register(h: i64, wt: u8, ck: u8, cb: f64, lbl: *const u8);
            }
            // Widget type 4 = picker family; the wheel shares the
            // index-callback contract, so automation drives it identically.
            perry_geisterhand_register(handle, 4, 1, on_change, std::ptr::null());
        }

        handle
    }
}

pub fn add_item(handle: i64, title_ptr: *const u8) {
    let title = str_from_header(title_ptr).to_string();
    let ns = NSString::from_str(&title);
    WHEEL_PICKERS.with(|w| {
        if let Some(state) = w.borrow_mut().get_mut(&handle) {
            state.items.push(title);
            state.titles.push(ns);
            if state.selected < 0 {
                state.selected = 0;
            }
        }
    });
    invalidate_rows(handle);
}

pub fn set_selected(handle: i64, index: i64) {
    let ok = WHEEL_PICKERS.with(|w| {
        let mut wheels = w.borrow_mut();
        let Some(state) = wheels.get_mut(&handle) else {
            return false;
        };
        if index < 0 || index as usize >= state.items.len() {
            return false;
        }
        state.selected = index;
        true
    });
    if !ok {
        return;
    }
    if let Some(view) = super::get_widget(handle) {
        unsafe {
            // UIKit does not post didSelectRow: for a programmatic move, so
            // this cannot echo back as a user onChange.
            let _: () = msg_send![&*view, selectRow: index, inComponent: 0i64, animated: true];
        }
    }
    invalidate_rows(handle);
}

pub fn get_selected(handle: i64) -> i64 {
    WHEEL_PICKERS.with(|w| {
        w.borrow()
            .get(&handle)
            .map(|s| if s.items.is_empty() { -1 } else { s.selected })
            .unwrap_or(-1)
    })
}

fn with_style(handle: i64, f: impl FnOnce(&mut WheelStyle)) {
    let found = WHEEL_PICKERS.with(|w| {
        let mut wheels = w.borrow_mut();
        match wheels.get_mut(&handle) {
            Some(state) => {
                f(&mut state.style);
                true
            }
            None => false,
        }
    });
    if found {
        invalidate_rows(handle);
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
