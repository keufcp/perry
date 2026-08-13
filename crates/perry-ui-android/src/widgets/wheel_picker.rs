//! WheelPicker — `PerryWheelPickerView` (issue #5873).
//!
//! The drum-roll complement to `Picker` (a `Spinner` dropdown here).
//!
//! This does NOT use `android.widget.NumberPicker`. NumberPicker's public SDK
//! surface is `setTextSize` and `setTextColor` and nothing else — verified
//! against android-35's `android.jar`. `setSelectedTextColor`,
//! `setSelectedTextSize`, `setTypeface` and `setSelectedTypeface` exist in
//! AOSP but are non-SDK, restricted since API 28. Building on it would leave
//! `wheelPickerSetFontWeight` and `wheelPickerSetSelectedTextColor` accepted
//! and silently inert on Android alone, which is precisely the defect this
//! widget's own PR had to fix elsewhere (`picker.rs` called a
//! `setSpinnerCallback` that was never defined, and discarded the JNI error).
//!
//! So the column is drawn by `PerryWheelPickerView` in the app template, the
//! same choice perry already makes on Windows, GTK4 and macOS — none of which
//! ship a wheel control either. See that class for what it reimplements to pay
//! for leaving the platform widget: fling physics, per-detent haptics and
//! accessibility.

use crate::callback;
use crate::jni_bridge;
use jni::objects::JValue;
use std::cell::RefCell;
use std::collections::HashMap;

const VIEW_CLASS: &str = "com/perry/app/PerryWheelPickerView";

fn str_from_header(ptr: *const u8) -> &'static str {
    crate::app::str_from_header(ptr)
}

struct WheelPickerState {
    items: Vec<String>,
    on_change: f64,
}

thread_local! {
    static WHEEL_PICKER_STATES: RefCell<HashMap<i64, WheelPickerState>> =
        RefCell::new(HashMap::new());
}

pub fn create(on_change: f64) -> i64 {
    let mut env = jni_bridge::get_env();
    let _ = env.push_local_frame(32);

    let activity = super::get_activity(&mut env);
    let view = env
        .new_object(
            VIEW_CLASS,
            "(Landroid/content/Context;)V",
            &[JValue::Object(&activity)],
        )
        .expect("Failed to create PerryWheelPickerView");

    if on_change != 0.0 {
        let cb_key = callback::register(on_change);
        env.call_method(&view, "setCallbackKey", "(J)V", &[JValue::Long(cb_key)])
            .expect("PerryWheelPickerView.setCallbackKey missing — template/runtime version skew");
    }

    let global = env
        .new_global_ref(view)
        .expect("Failed to create global ref");
    let handle = super::register_widget(global);

    WHEEL_PICKER_STATES.with(|s| {
        s.borrow_mut().insert(
            handle,
            WheelPickerState {
                items: Vec::new(),
                on_change,
            },
        );
    });

    unsafe {
        env.pop_local_frame(&jni::objects::JObject::null());
    }
    handle
}

pub fn add_item(handle: i64, title_ptr: *const u8) {
    let title = str_from_header(title_ptr).to_string();
    let items = WHEEL_PICKER_STATES.with(|s| {
        let mut states = s.borrow_mut();
        let state = states.get_mut(&handle)?;
        state.items.push(title);
        Some(state.items.clone())
    });
    // Push outside the borrow: the JNI round-trip can re-enter this
    // thread_local via a callback, and a live `borrow_mut` would panic.
    if let Some(items) = items {
        push_items(handle, &items);
    }
}

pub fn set_selected(handle: i64, index: i64) {
    let count = WHEEL_PICKER_STATES
        .with(|s| s.borrow().get(&handle).map(|st| st.items.len()))
        .unwrap_or(0);
    if index < 0 || index as usize >= count {
        return;
    }
    if let Some(view_ref) = super::get_widget(handle) {
        let mut env = jni_bridge::get_env();
        let _ = env.push_local_frame(8);
        // `setValueQuietly` holds the view's own suppression flag, so a
        // programmatic selection cannot echo back as a user onChange.
        env.call_method(
            view_ref.as_obj(),
            "setValueQuietly",
            "(I)V",
            &[JValue::Int(index as i32)],
        )
        .expect("PerryWheelPickerView.setValueQuietly missing — template/runtime version skew");
        unsafe {
            env.pop_local_frame(&jni::objects::JObject::null());
        }
    }
}

pub fn get_selected(handle: i64) -> i64 {
    // An empty wheel has no selection; the view reports -1 for that, but
    // answer from our own list so the contract holds even before the view has
    // been laid out.
    let count = WHEEL_PICKER_STATES
        .with(|s| s.borrow().get(&handle).map(|st| st.items.len()))
        .unwrap_or(0);
    if count == 0 {
        return -1;
    }
    if let Some(view_ref) = super::get_widget(handle) {
        let mut env = jni_bridge::get_env();
        let _ = env.push_local_frame(8);
        let result = env.call_method(view_ref.as_obj(), "currentValue", "()I", &[]);
        unsafe {
            env.pop_local_frame(&jni::objects::JObject::null());
        }
        if let Ok(jni::objects::JValueGen::Int(i)) = result {
            return i as i64;
        }
    }
    -1
}

pub fn set_font_size(handle: i64, size: f64) {
    push_style(handle, size, -1.0, 0, 0);
}

pub fn set_font_weight(handle: i64, weight: f64) {
    push_style(handle, 0.0, weight, 0, 0);
}

pub fn set_text_color(handle: i64, r: f64, g: f64, b: f64, a: f64) {
    push_style(handle, 0.0, -1.0, super::argb_color(a, r, g, b), 0);
}

pub fn set_selected_text_color(handle: i64, r: f64, g: f64, b: f64, a: f64) {
    push_style(handle, 0.0, -1.0, 0, super::argb_color(a, r, g, b));
}

/// The view keeps the accumulated style, so each setter pushes only its own
/// field. Sentinels mean "not this call": size <= 0, weight < 0, colour == 0.
fn push_style(handle: i64, size_sp: f64, weight: f64, text_argb: i32, selected_argb: i32) {
    let Some(view_ref) = super::get_widget(handle) else {
        return;
    };
    let mut env = jni_bridge::get_env();
    let _ = env.push_local_frame(8);
    // Passed in sp, converted on the Kotlin side. Not dp: sp scales with the
    // user's font-size preference, and a wheel that ignores it is an
    // accessibility regression.
    env.call_method(
        view_ref.as_obj(),
        "setStyle",
        "(FIII)V",
        &[
            JValue::Float(size_sp as f32),
            JValue::Int(weight as i32),
            JValue::Int(text_argb),
            JValue::Int(selected_argb),
        ],
    )
    .expect("PerryWheelPickerView.setStyle missing — template/runtime version skew");
    unsafe {
        env.pop_local_frame(&jni::objects::JObject::null());
    }
}

fn push_items(handle: i64, items: &[String]) {
    let Some(view_ref) = super::get_widget(handle) else {
        return;
    };
    let mut env = jni_bridge::get_env();
    let _ = env.push_local_frame(32 + items.len() as i32);

    let str_class = env.find_class("java/lang/String").expect("String class");
    let arr = env
        .new_object_array(
            items.len() as i32,
            &str_class,
            &jni::objects::JObject::null(),
        )
        .expect("Failed to create String array");
    for (i, item) in items.iter().enumerate() {
        let jstr = env.new_string(item).expect("Failed to create JNI string");
        let _ = env.set_object_array_element(&arr, i as i32, &jstr);
    }

    env.call_method(
        view_ref.as_obj(),
        "setItems",
        "([Ljava/lang/String;)V",
        &[JValue::Object(&arr)],
    )
    .expect("PerryWheelPickerView.setItems missing — template/runtime version skew");

    unsafe {
        env.pop_local_frame(&jni::objects::JObject::null());
    }
}
