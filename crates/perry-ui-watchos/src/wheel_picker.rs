//! WheelPicker FFI (#5873) — split out of lib.rs to stay under the
//! 2000-line file-size gate (`scripts/check_file_size.sh`).
//!
//! `#[no_mangle]` symbols export by name, not by module path, so nothing
//! needs re-exporting from `lib.rs` — the `mod` declaration is enough.

use crate::cstring_from_header;
use crate::tree::{self, NodeData, NodeKind};

// ---- WheelPicker (#5873) ----
//
// watchOS is the one platform where the wheel is the *default* Picker
// presentation (the digital crown drives it), so this differs from
// `Picker` only in carrying an explicit `.pickerStyle(.wheel)` on the Swift
// side — the node payload is identical.

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_create(on_change: f64) -> i64 {
    // Single `Closure` arg to match the dispatch-table ABI — see #5491.
    let mut node = NodeData::new(NodeKind::WheelPicker);
    node.on_change_closure = Some(on_change);
    tree::register_node(node)
}

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_add_item(handle: i64, title_ptr: i64) {
    if let Some(title) = cstring_from_header(title_ptr as *const u8) {
        tree::with_node_mut(handle, |node| {
            node.picker_items.push(title);
        });
    }
}

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_set_selected(handle: i64, index: i64) {
    tree::with_node_mut(handle, |node| {
        node.picker_selected = index;
    });
}

// Row typography reuses the node's generic style fields, which the Swift side
// already exposes via `perry_watchos_node_font_size` / `_font_weight` /
// `_color` — no new node payload is needed.
//
// `wheelPickerSetSelectedTextColor` has no counterpart here: watchOS's wheel
// draws its own selection treatment and SwiftUI gives no hook to colour the
// centred row apart from its neighbours. It is accepted and folded into the
// row colour rather than silently dropped, so an app that sets only the
// selected colour still gets a styled wheel.

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_set_font_size(handle: i64, size: f64) {
    tree::with_node_mut(handle, |node| {
        node.font_size = Some(size);
    });
}

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_set_font_weight(handle: i64, weight: f64) {
    tree::with_node_mut(handle, |node| {
        node.font_weight = Some(weight);
    });
}

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_set_text_color(handle: i64, r: f64, g: f64, b: f64, a: f64) {
    tree::with_node_mut(handle, |node| {
        node.color = Some((r, g, b, a));
    });
}

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_set_selected_text_color(
    handle: i64,
    r: f64,
    g: f64,
    b: f64,
    a: f64,
) {
    tree::with_node_mut(handle, |node| {
        if node.color.is_none() {
            node.color = Some((r, g, b, a));
        }
    });
}

#[no_mangle]
pub extern "C" fn perry_ui_wheelpicker_get_selected(handle: i64) -> i64 {
    // -1 on an empty wheel, matching the other backends: 0 would be
    // indistinguishable from "first row selected".
    tree::with_node(handle, |n| {
        if n.picker_items.is_empty() {
            -1
        } else {
            n.picker_selected
        }
    })
    .unwrap_or(-1)
}
