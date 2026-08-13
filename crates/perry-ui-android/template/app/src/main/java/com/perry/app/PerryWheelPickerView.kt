package com.perry.app

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.view.GestureDetector
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.widget.OverScroller
import kotlin.math.abs
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * Drum-roll column for `perry/ui`'s WheelPicker (#5873).
 *
 * WHY NOT `android.widget.NumberPicker`: the public SDK exposes only
 * `setTextSize` and `setTextColor` on it — `setSelectedTextColor`,
 * `setSelectedTextSize`, `setTypeface` and `setSelectedTypeface` exist in AOSP
 * but are non-SDK (verified against android-35's `android.jar`). Reaching them
 * means reflecting into a restricted interface. That left
 * `wheelPickerSetFontWeight` accepted-but-inert on Android alone, which is the
 * same silently-inert-API defect this widget's own PR had to fix elsewhere
 * (`setSpinnerCallback` was called by Rust and never defined here). Drawing the
 * column ourselves keeps the API contract identical on every backend and
 * matches what perry already does on Windows, GTK4 and macOS, none of which
 * ship a wheel control either.
 *
 * What that costs, and what is therefore reimplemented below: fling physics
 * (`OverScroller`), the per-detent tick (`HapticFeedbackConstants.CLOCK_TICK`)
 * and accessibility (`AccessibilityNodeInfo` scroll actions plus a spoken
 * announcement per change). NumberPicker gave those for free; dropping them
 * would have been a regression, not a trade.
 *
 * Motion model matches the other custom-drawn backends: a detent wheel. The
 * selection is whatever row is nearest the centre line, and settling always
 * snaps, so one gesture step is exactly one `onChange`.
 */
class PerryWheelPickerView(context: Context) : View(context) {

    companion object {
        private const val VISIBLE_ROWS = 5
        private const val MIN_ROW_DP = 32f
    }

    private var items: Array<String> = emptyArray()
    private var value: Int = -1

    /** Continuous scroll position in pixels; `value` is this snapped to a row. */
    private var scrollPx: Int = 0

    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        textAlign = Paint.Align.CENTER
    }
    private val dividerPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = 0xFFA59E9E.toInt()
        strokeWidth = context.resources.displayMetrics.density
    }

    private var appTextSizePx: Float = 0f
    private var appTypeface: Typeface? = null
    private var appTextColor: Int = 0
    private var appSelectedColor: Int = 0

    private val scroller = OverScroller(context)
    private var callbackKey: Long = 0

    /** Set while the app is moving the wheel, so it cannot echo as an onChange. */
    private var quiet = false

    /**
     * Set between ACTION_DOWN and ACTION_UP. `computeScroll` must not snap
     * while it is set: snapping runs on every frame the drag invalidates, so
     * it would drag the wheel back to the current row as fast as the finger
     * moves it away and the column would sit still under a moving finger.
     */
    private var dragging = false

    /**
     * Last index handed to the app. `onChange` reports a CHOICE, not the rows
     * that flew past on the way to it: the callback fires once the wheel
     * settles, matching UIPickerView, Spinner and `<select>`, and matching
     * `Picker` — the widget this one is the drum-roll complement to. Per-row
     * firing would also re-enter the JS runtime dozens of times per fling, and
     * could never be made uniform because UIPickerView reports only settled
     * selections. The haptic tick and the spoken announcement stay per row:
     * those describe the drum turning, not the choice.
     */
    private var reported = -1

    private val density = context.resources.displayMetrics.density

    private val rowHeight: Int
        get() {
            val fromFont = if (appTextSizePx > 0f) appTextSizePx * 1.6f else 0f
            return max(fromFont, MIN_ROW_DP * density).roundToInt()
        }

    private val maxScroll: Int
        get() = if (items.isEmpty()) 0 else (items.size - 1) * rowHeight

    // Perry builds widgets on its native thread, which has no Looper. The
    // two-argument GestureDetector constructor makes a bare `Handler()` and
    // therefore throws there, so bind one to the main Looper explicitly.
    private val gestures = GestureDetector(context, object : GestureDetector.SimpleOnGestureListener() {
        override fun onDown(e: MotionEvent): Boolean {
            scroller.forceFinished(true)
            return true
        }

        override fun onScroll(
            e1: MotionEvent?,
            e2: MotionEvent,
            distanceX: Float,
            distanceY: Float
        ): Boolean {
            scrollTo(scrollPx + distanceY.roundToInt())
            return true
        }

        override fun onFling(
            e1: MotionEvent?,
            e2: MotionEvent,
            velocityX: Float,
            velocityY: Float
        ): Boolean {
            scroller.fling(0, scrollPx, 0, -velocityY.roundToInt(), 0, 0, 0, maxScroll)
            postInvalidateOnAnimation()
            return true
        }
    }, android.os.Handler(android.os.Looper.getMainLooper()))

    init {
        isFocusable = true
        isFocusableInTouchMode = false
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_YES
    }

    // ---- public surface used by PerryBridge ----

    fun setCallbackKey(key: Long) {
        callbackKey = key
    }

    fun setItems(newItems: Array<String>) {
        items = newItems
        if (value < 0 && items.isNotEmpty()) value = 0
        if (value >= items.size) value = items.size - 1
        // Populating the wheel is never a user choice, so the resulting
        // selection must not read as one at the next settle.
        reported = value
        scrollPx = if (value < 0) 0 else value * rowHeight
        requestLayout()
        invalidate()
        updateAccessibilityText()
    }

    fun setValueQuietly(index: Int) {
        if (items.isEmpty()) return
        val clamped = index.coerceIn(0, items.size - 1)
        quiet = true
        try {
            value = clamped
            // A programmatic move is not a choice, so it must not report — but
            // it does become the baseline the next settle is compared against.
            reported = clamped
            scroller.forceFinished(true)
            scrollPx = clamped * rowHeight
        } finally {
            quiet = false
        }
        invalidate()
        updateAccessibilityText()
    }

    fun currentValue(): Int = value

    /**
     * @param sizeSp row text size in **sp**, not px — sp scales with the
     *   user's font-size preference, and a wheel that ignores that is an
     *   accessibility regression. Sentinels mean "not this call":
     *   size <= 0, weight < 0, colour == 0.
     */
    fun setStyle(sizeSp: Float, weight: Int, textArgb: Int, selectedArgb: Int) {
        if (sizeSp > 0f) {
            appTextSizePx = android.util.TypedValue.applyDimension(
                android.util.TypedValue.COMPLEX_UNIT_SP,
                sizeSp,
                resources.displayMetrics
            )
        }
        if (weight >= 0) {
            appTypeface = if (weight >= 600) Typeface.DEFAULT_BOLD else Typeface.DEFAULT
        }
        if (textArgb != 0) appTextColor = textArgb
        if (selectedArgb != 0) appSelectedColor = selectedArgb
        // Row height tracks the font, so re-anchor the scroll on the selection.
        if (value >= 0) scrollPx = value * rowHeight
        requestLayout()
        invalidate()
    }

    // ---- scrolling ----

    private fun scrollTo(target: Int) {
        val clamped = target.coerceIn(0, maxScroll)
        if (clamped == scrollPx) return
        scrollPx = clamped
        syncValueFromScroll()
        invalidate()
    }

    /**
     * Recompute the selection from the scroll position, ticking and reporting
     * only when it actually crosses into a new row.
     */
    private fun syncValueFromScroll() {
        if (items.isEmpty()) return
        val next = (scrollPx.toFloat() / rowHeight).roundToInt().coerceIn(0, items.size - 1)
        if (next == value) return
        value = next
        if (quiet) return
        // No FLAG_IGNORE_GLOBAL_SETTING: it is deprecated, and overriding the
        // user's haptics preference is not ours to do.
        performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
        updateAccessibilityText()
        announceForAccessibility(items[value])
    }

    /** Report the settled selection, once, if it differs from the last one. */
    private fun reportIfSettled() {
        if (quiet || items.isEmpty()) return
        if (value == reported) return
        reported = value
        if (callbackKey != 0L) PerryBridge.nativeInvokeCallback1(callbackKey, value.toDouble())
    }

    private fun snapToNearest() {
        if (items.isEmpty()) return
        val target = value.coerceIn(0, items.size - 1) * rowHeight
        if (target == scrollPx) return
        scroller.startScroll(0, scrollPx, 0, target - scrollPx, 200)
        postInvalidateOnAnimation()
    }

    override fun computeScroll() {
        if (scroller.computeScrollOffset()) {
            scrollTo(scroller.currY)
            postInvalidateOnAnimation()
        } else if (!dragging && items.isNotEmpty() && scrollPx != value * rowHeight) {
            snapToNearest()
        } else if (!dragging) {
            // Fling finished and the column is on the grid: this is the choice.
            reportIfSettled()
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                dragging = true
                // Let the wheel keep the gesture even inside a scrolling parent.
                parent?.requestDisallowInterceptTouchEvent(true)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> dragging = false
        }
        val handled = gestures.onTouchEvent(event)
        if (!dragging && scroller.isFinished) snapToNearest()
        return handled || super.onTouchEvent(event)
    }

    // ---- measurement / drawing ----

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val desiredHeight = rowHeight * VISIBLE_ROWS
        val desiredWidth = (90 * density).roundToInt()
        setMeasuredDimension(
            resolveSize(desiredWidth, widthMeasureSpec),
            resolveSize(desiredHeight, heightMeasureSpec)
        )
    }

    override fun onDraw(canvas: Canvas) {
        if (items.isEmpty()) return
        val centre = height / 2f
        val row = rowHeight

        paint.textSize = if (appTextSizePx > 0f) appTextSizePx else 15f * density
        paint.typeface = appTypeface ?: Typeface.DEFAULT
        val metrics = paint.fontMetrics

        // Rows are placed from the CONTINUOUS scroll position, never from the
        // snapped index. Drawing `value + offset` renders only settled states,
        // so the column teleports between rows and nothing moves under the
        // finger — a stepper, not a drum. Every row's centre is
        // `centre + (i * row - scrollPx)`, which makes the paper turn while
        // dragging and while the fling decelerates.
        val first = max(0f, floor((scrollPx - centre) / row)).toInt()
        val last = min((items.size - 1).toFloat(), ceil((scrollPx + centre) / row)).toInt()

        for (i in first..last) {
            val y = centre + (i * row - scrollPx)
            // Distance from the selection band in rows, fractional — so the
            // emphasis crossfades as the wheel turns instead of snapping.
            val dist = abs(y - centre) / row
            val selected = dist < 0.5f

            paint.color = when {
                selected && appSelectedColor != 0 -> appSelectedColor
                appTextColor != 0 -> appTextColor
                else -> 0xFF1A1A1A.toInt()
            }
            if (!selected || appSelectedColor == 0) {
                // Fade with distance — the cue that this is a drum and not a
                // list. Full opacity inside the band, tapering outwards.
                val fade = (1f - (dist - 0.5f).coerceAtLeast(0f) * 0.42f).coerceIn(0.25f, 1f)
                paint.alpha = (paint.alpha * fade).toInt()
            }

            // Baseline centres the glyph box, not the em box.
            val baseline = y - (metrics.ascent + metrics.descent) / 2f
            canvas.drawText(items[i], width / 2f, baseline, paint)
            paint.alpha = 0xFF
        }

        canvas.drawLine(4f, centre - row / 2f, width - 4f, centre - row / 2f, dividerPaint)
        canvas.drawLine(4f, centre + row / 2f, width - 4f, centre + row / 2f, dividerPaint)
    }

    // ---- accessibility ----

    private fun updateAccessibilityText() {
        contentDescription = if (value in items.indices) items[value] else null
    }

    override fun onInitializeAccessibilityNodeInfo(info: AccessibilityNodeInfo) {
        super.onInitializeAccessibilityNodeInfo(info)
        info.className = "android.widget.NumberPicker"
        info.isScrollable = items.size > 1
        if (value > 0) {
            info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_BACKWARD)
        }
        if (value in 0 until items.size - 1) {
            info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_FORWARD)
        }
    }

    override fun performAccessibilityAction(action: Int, arguments: android.os.Bundle?): Boolean {
        when (action) {
            AccessibilityNodeInfo.ACTION_SCROLL_FORWARD -> {
                // A discrete accessibility step lands settled, so it reports
                // immediately — there is no gesture to wait out.
                scrollTo(scrollPx + rowHeight)
                reportIfSettled()
                return true
            }
            AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD -> {
                scrollTo(scrollPx - rowHeight)
                reportIfSettled()
                return true
            }
        }
        return super.performAccessibilityAction(action, arguments)
    }

    override fun onInitializeAccessibilityEvent(event: AccessibilityEvent) {
        super.onInitializeAccessibilityEvent(event)
        event.className = "android.widget.NumberPicker"
        event.itemCount = items.size
        event.currentItemIndex = value
    }
}
