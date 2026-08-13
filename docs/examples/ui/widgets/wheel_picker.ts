// demonstrates: WheelPicker drum roll over a long sequential range
// docs: docs/src/ui/widgets.md
// platforms: macos, linux, windows
// targets: ios-simulator, web, wasm

import { App, VStack, Text, WheelPicker, State, wheelPickerAddItem, wheelPickerSetSelected } from "perry/ui"

const hour = State(9)
const wheel = WheelPicker((index: number) => hour.set(index))
for (let i = 0; i <= 23; i++) {
    wheelPickerAddItem(wheel, i < 10 ? `0${i}` : `${i}`)
}
// Programmatic selection does not fire onChange.
wheelPickerSetSelected(wheel, 9)

App({
    title: "WheelPicker",
    width: 320,
    height: 320,
    body: VStack(12, [
        wheel,
        Text(`Hour: ${hour.value}`),
    ]),
})
