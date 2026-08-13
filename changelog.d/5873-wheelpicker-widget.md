Added a `WheelPicker` widget to `perry/ui` (#5873) — a drum-roll selector for
long sequential ranges such as hours, minutes and quantities, where `Picker`
(a segmented control on iOS, a dropdown elsewhere) becomes unusable past about
five items. `WheelPicker(onChange)` plus `wheelPickerAddItem`,
`wheelPickerSetSelected` and `wheelPickerGetSelected`; `onChange` receives the
selected index, matching `Picker`, and a programmatic `wheelPickerSetSelected`
deliberately does not fire it. `wheelPickerGetSelected` returns `-1` on an
empty wheel rather than `0`, which would be indistinguishable from "first row
selected".

The constructor takes a single `Closure` argument for the same reason `Picker`
does: a multi-argument constructor mis-binds `on_change` on the Windows x64 ABI
(#5491). That is also why this is a separate widget rather than a `.wheel`
style variant of `Picker` — the style would have to become a constructor
argument.

Backends use each platform's native wheel where one exists — `UIPickerView`
(iOS/tvOS/visionOS), `WKInterfacePicker` (watchOS), `android.widget.NumberPicker`
(Android) — and a custom-drawn snapping column where none does: CoreGraphics on
macOS, GDI owner-draw on Windows, Cairo on a `GtkDrawingArea` for GTK4, all
following the existing Chart widget's shape. Web and WASM use CSS
`scroll-snap-type: y mandatory`. The three custom-drawn backends share a detent
motion model — scroll travel accumulates and each whole row height becomes one
step — so one gesture step is exactly one `onChange` on every platform, with no
settling animation or timer to arbitrate with.

Two Android fixes were required to get there and are included:

- `crates/perry-ui-android/src/json.rs` still called the `is_raw_pointer`
  helper that #7448 deleted, so `perry-ui-android` did not compile for any
  Android target. #7448 converted the object path to `extract_pointer` but not
  the array-element path; routing it through the same helper also extends the
  #7447 fix to array elements, where the old bit test — which is the IEEE-754
  positive-subnormal predicate — classified every positive denormal element as
  a pointer and dereferenced it.
- `PerryBridge.setSpinnerCallback` did not exist. `picker.rs` has always called
  it and discarded the resulting JNI error with `let _ =`, so `Picker`'s
  `onChange` could never fire on Android. The Kotlin side now defines it, and
  the new `WheelPicker` bridge entry points surface a missing-method failure
  instead of swallowing it.

`NumberPicker` has no incremental append and validates `setDisplayedValues`
against the current min/max, so the bridge records the array and coalesces all
appends from one JS turn into a single apply on the next UI message; rebuilding
per item visibly thrashed the selector wheel. Programmatic selection routes
through `setNumberPickerValue`, which flushes that pending rebuild
synchronously before setting the value, so `wheelPickerGetSelected` never lags
a message behind and the value is not clamped against a stale `maxValue`.
