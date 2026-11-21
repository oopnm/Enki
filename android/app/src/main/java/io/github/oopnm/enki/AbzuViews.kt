package io.github.oopnm.enki

import android.content.Context
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RadialGradient
import android.graphics.RectF
import android.graphics.Shader
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.View
import kotlin.math.PI
import kotlin.math.exp
import kotlin.math.sin

/** What the water shows (the same three states as the desktop window). */
enum class AbzuVisual(val level: Float) {
    /** Nothing is running: calm water, the two rivers apart. */
    IDLE(0.2f),

    /** The link is being set up: the rivers reach for each other. */
    LINKING(0.55f),

    /** The camera is streaming: one river, lively waves. */
    LIVE(1f),
}

/**
 * One clock for every animated Abzu view. It ticks at most 30 times a second (15 when the water is calm) and does
 * not tick at all while the screen is not in front, in black-screen mode, or when motion is reduced: the views then
 * draw one still frame.
 */
object AbzuMotion {
    private const val SINE_STEPS = 1024
    private const val TAU = (2 * PI).toFloat()
    private val sine = FloatArray(SINE_STEPS) { sin(it.toFloat() / SINE_STEPS * TAU) }

    var t = 0f
        private set
    var level = AbzuVisual.IDLE.level
        private set

    /** 1 right after the link came up, then fading out: the only place the gold color is used. */
    var glow = 0f
        private set
    var visual = AbzuVisual.IDLE
        private set

    private var reduce = false
    private var foreground = false
    private var paused = false
    private var last = 0L
    private var ticks = 0
    private val views = LinkedHashSet<View>()
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            step()
            if (running()) handler.postDelayed(this, 33)
        }
    }

    fun wave(theta: Float): Float {
        val turns = (theta / TAU).let { it - Math.floor(it.toDouble()).toFloat() }
        return sine[(turns * SINE_STEPS).toInt() % SINE_STEPS]
    }

    fun register(view: View) {
        views.add(view)
        sync()
    }

    fun unregister(view: View) {
        views.remove(view)
    }

    fun setVisual(v: AbzuVisual) {
        if (v == AbzuVisual.LIVE && visual != AbzuVisual.LIVE && !reduce) glow = 1f
        visual = v
        sync()
    }

    fun setReduce(value: Boolean) {
        reduce = value
        sync()
    }

    /** The screen is in front of the user. */
    fun setForeground(value: Boolean) {
        foreground = value
        sync()
    }

    /** Black-screen mode: nothing is drawn, so nothing should run. */
    fun setPaused(value: Boolean) {
        paused = value
        sync()
    }

    private fun running() = foreground && !paused && !reduce && views.isNotEmpty()

    private fun sync() {
        handler.removeCallbacks(tick)
        if (reduce) {
            level = visual.level
            glow = 0f
        }
        if (running()) {
            last = SystemClock.uptimeMillis()
            handler.postDelayed(tick, 33)
        }
        invalidateAll()
    }

    private fun step() {
        val now = SystemClock.uptimeMillis()
        val dt = ((now - last) / 1000f).coerceAtMost(0.25f)
        last = now
        t += dt
        level += (visual.level - level) * (1f - exp(-1.5f * dt))
        glow = (glow - dt / 2.5f).coerceAtLeast(0f)
        ticks++
        // Calm water moves at half the frame rate.
        if (visual == AbzuVisual.IDLE && glow <= 0f && ticks % 2 == 0) return
        invalidateAll()
    }

    private fun invalidateAll() {
        for (v in views) v.invalidate()
    }
}

/** Base of the animated views: registers itself with the clock while it is on screen. */
abstract class AbzuView(context: Context) : View(context) {
    protected val dp = resources.displayMetrics.density
    protected fun color(id: Int) = context.getColor(id)

    protected fun paint(colorId: Int, alpha: Float = 1f, stroke: Float = 0f) = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = color(colorId)
        this.alpha = (alpha * 255).toInt().coerceIn(0, 255)
        if (stroke > 0f) {
            style = Paint.Style.STROKE
            strokeWidth = stroke
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
        }
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        AbzuMotion.register(this)
    }

    override fun onDetachedFromWindow() {
        AbzuMotion.unregister(this)
        super.onDetachedFromWindow()
    }
}

/** The waves along the bottom of the screen. Touches go through it. */
class WaveView(context: Context) : AbzuView(context) {
    private class Layer(val base: Float, val amp: Float, val ampLevel: Float, val length: Float, val speed: Float, val colorId: Int, val alpha: Float, val alphaLevel: Float)

    private val layers = listOf(
        Layer(0.50f, 0.10f, 0.12f, 260f, 0.55f, R.color.enki_lapis, 0.40f, 0f),
        Layer(0.62f, 0.08f, 0.10f, 190f, -0.80f, R.color.enki_aqua, 0.14f, 0.10f),
        Layer(0.74f, 0.06f, 0.07f, 130f, 1.10f, R.color.enki_lapis, 0.55f, 0f),
    )
    private val path = Path()
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG)
    private val crest = paint(R.color.enki_foam, 1f, 1.2f * dp)
    private val shaders = HashMap<Int, Shader>()
    private var shaderHeight = -1

    init {
        isClickable = false
        isFocusable = false
    }

    override fun onDraw(canvas: Canvas) {
        val w = width.toFloat()
        val h = height.toFloat()
        if (w <= 0f || h <= 0f) return
        if (shaderHeight != height) {
            shaders.clear()
            shaderHeight = height
        }
        val level = AbzuMotion.level
        val t = AbzuMotion.t
        layers.forEachIndexed { i, l ->
            val phase = t * l.speed * (0.6f + 0.8f * level) + i * 1.7f
            val y0 = h * l.base
            val a = h * (l.amp + l.ampLevel * level)
            val lengthPx = l.length * dp
            path.rewind()
            path.moveTo(0f, h)
            var x = 0f
            val step = 6f * dp
            while (x <= w + step) {
                val theta = x / lengthPx * TAU_F + phase
                val y = y0 + a * (AbzuMotion.wave(theta) + 0.45f * AbzuMotion.wave(theta * 2.3f - phase * 0.6f))
                path.lineTo(x, y)
                x += step
            }
            path.lineTo(w + step, h)
            path.close()
            val shader = shaders.getOrPut(i) {
                LinearGradient(0f, h * (l.base - l.amp - l.ampLevel), 0f, h, color(l.colorId), color(R.color.enki_abyss) and 0x00FFFFFF, Shader.TileMode.CLAMP)
            }
            fill.shader = shader
            fill.alpha = ((l.alpha + l.alphaLevel * level) * 255).toInt().coerceIn(0, 255)
            canvas.drawPath(path, fill)
            if (i == 0) {
                crest.alpha = ((0.10f + 0.22f * level) * 255).toInt()
                // Only the crest line, not the closing edges.
                canvas.drawPath(crestPath(w, y0, a, lengthPx, phase), crest)
            }
        }
    }

    private val crestLine = Path()

    private fun crestPath(w: Float, y0: Float, a: Float, lengthPx: Float, phase: Float): Path {
        crestLine.rewind()
        var x = 0f
        val step = 6f * dp
        var first = true
        while (x <= w + step) {
            val theta = x / lengthPx * TAU_F + phase
            val y = y0 + a * (AbzuMotion.wave(theta) + 0.45f * AbzuMotion.wave(theta * 2.3f - phase * 0.6f))
            if (first) { crestLine.moveTo(x, y); first = false } else crestLine.lineTo(x, y)
            x += step
        }
        return crestLine
    }
}

private const val TAU_F = (2 * PI).toFloat()

/**
 * The two rivers: the computer on one side, the phone on the other. Apart while idle, reaching for each other
 * while linking, one river when streaming.
 */
class RiverView(context: Context) : AbzuView(context) {
    private val path = Path()
    private val rect = RectF()
    private val icon = paint(R.color.enki_foam, 1f, 2f * dp)
    private val dot = paint(R.color.enki_foam)
    private val line = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
        strokeJoin = Paint.Join.ROUND
    }
    private val glowPaint = Paint(Paint.ANTI_ALIAS_FLAG)

    override fun onDraw(canvas: Canvas) {
        val w = width.toFloat()
        val h = height.toFloat()
        if (w <= 0f || h <= 0f) return
        val t = AbzuMotion.t
        val visual = AbzuMotion.visual
        val live = visual == AbzuVisual.LIVE
        val ym = h / 2
        val xl = 34f * dp
        val xr = w - 34f * dp
        val cx = w / 2
        val reach = when (visual) {
            AbzuVisual.IDLE -> 0.34f
            AbzuVisual.LINKING -> 0.55f + 0.17f * AbzuMotion.wave(t * 2.2f)
            AbzuVisual.LIVE -> 1f
        }
        val amp = (if (live) 5f else 7f) * dp
        val wavelength = 120f * dp
        val flow = t * 3.2f
        val node = 22f * dp

        fun river(from: Float, to: Float, sign: Float) {
            path.rewind()
            val steps = (Math.abs(to - from) / (4f * dp)).toInt().coerceAtLeast(2)
            for (s in 0..steps) {
                val x = from + (to - from) * s / steps
                val edge = (minOf(x - xl, xr - x) / (40f * dp)).coerceIn(0f, 1f)
                val y = ym + amp * edge * AbzuMotion.wave(x / wavelength * TAU_F + sign * flow)
                if (s == 0) path.moveTo(x, y) else path.lineTo(x, y)
            }
        }

        if (live) {
            for ((width, alpha, id) in listOf(
                Triple(9f * dp, 0.10f, R.color.enki_aqua),
                Triple(4f * dp, 0.55f, R.color.enki_aqua),
                Triple(1.6f * dp, 0.95f, R.color.enki_foam),
            )) {
                river(xl + node, xr - node, -1f)
                line.pathEffect = null
                line.strokeWidth = width
                line.color = color(id)
                line.alpha = (alpha * 255).toInt()
                canvas.drawPath(path, line)
            }
        } else {
            val id = if (visual == AbzuVisual.LINKING) R.color.enki_aqua else R.color.enki_lapis
            line.color = color(id)
            line.alpha = (if (visual == AbzuVisual.LINKING) 0.75f else 0.85f).times(255).toInt()
            line.strokeWidth = 3f * dp
            line.pathEffect = DashPathEffect(floatArrayOf(9f * dp, 7f * dp), (t * 30f * dp) % (16f * dp))
            river(xl + node, xl + node + (cx - xl - node) * reach, 1f)
            canvas.drawPath(path, line)
            river(xr - node, xr - node - (xr - cx - node) * reach, -1f)
            canvas.drawPath(path, line)
            line.pathEffect = null
        }

        val glow = AbzuMotion.glow
        if (glow > 0f) {
            val radius = (14f + 26f * (1f - glow)) * dp
            val gold = color(R.color.enki_gold)
            glowPaint.shader = RadialGradient(
                cx, ym, radius,
                (gold and 0x00FFFFFF) or ((0.85f * glow * 255).toInt() shl 24), gold and 0x00FFFFFF,
                Shader.TileMode.CLAMP,
            )
            canvas.drawCircle(cx, ym, radius, glowPaint)
        }

        icon.alpha = ((if (live) 0.95f else 0.55f) * 255).toInt()
        dot.alpha = icon.alpha
        // The computer screen and its stand.
        rect.set(xl - 16f * dp, ym - 12f * dp, xl + 16f * dp, ym + 9f * dp)
        canvas.drawRoundRect(rect, 4f * dp, 4f * dp, icon)
        canvas.drawLine(xl - 7f * dp, ym + 15f * dp, xl + 7f * dp, ym + 15f * dp, icon)
        // The phone.
        rect.set(xr - 8f * dp, ym - 14f * dp, xr + 8f * dp, ym + 14f * dp)
        canvas.drawRoundRect(rect, 4f * dp, 4f * dp, icon)
        canvas.drawCircle(xr, ym + 9.5f * dp, 1.3f * dp, dot)
    }
}

/** A thin line of abstract wedge marks, like writing pressed into clay. Shapes only, never text. Never animated. */
class CuneiformView(context: Context, seed: String) : View(context) {
    private val dp = resources.displayMetrics.density
    private val seed = seed.fold(7) { h, c -> h * 31 + c.code }
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG)
    private val tail = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeCap = Paint.Cap.ROUND
        strokeWidth = 1.1f * dp
    }
    private val head = Path()

    init {
        isClickable = false
        minimumHeight = (14 * dp).toInt()
    }

    override fun onMeasure(widthSpec: Int, heightSpec: Int) {
        setMeasuredDimension(getDefaultSize(suggestedMinimumWidth, widthSpec), (14 * dp).toInt())
    }

    private fun wedge(canvas: Canvas, x: Float, y: Float, angle: Float, size: Float) {
        canvas.save()
        canvas.translate(x, y)
        canvas.rotate(angle)
        head.rewind()
        head.moveTo(-size * 0.35f, -size * 0.5f)
        head.lineTo(size * 0.35f, -size * 0.5f)
        head.lineTo(0f, size * 0.05f)
        head.close()
        canvas.drawPath(head, fill)
        canvas.drawLine(0f, 0f, 0f, size * 0.55f, tail)
        canvas.restore()
    }

    override fun onDraw(canvas: Canvas) {
        val w = width.toFloat()
        val h = height.toFloat()
        val foam = context.getColor(R.color.enki_foam)
        var state = seed or 1
        fun next(): Int {
            state = state xor (state shl 13)
            state = state xor (state ushr 17)
            state = state xor (state shl 5)
            return state ushr 1
        }
        var x = 6f * dp
        while (x < w - 6f * dp) {
            val edge = (minOf(x, w - x) / (70f * dp)).coerceIn(0f, 1f)
            val a = (0.22f * edge * 255).toInt()
            fill.color = foam
            fill.alpha = a
            tail.color = foam
            tail.alpha = a
            val size = (7 + next() % 4) * dp
            when (next() % 4) {
                0 -> wedge(canvas, x, h / 2, 0f, size)
                1 -> wedge(canvas, x, h / 2, 90f, size)
                2 -> {
                    wedge(canvas, x - 2.5f * dp, h / 2, 0f, size * 0.8f)
                    wedge(canvas, x + 2.5f * dp, h / 2, 0f, size * 0.8f)
                }
                else -> wedge(canvas, x, h / 2, 45f, size)
            }
            x += (12 + (next() % 3) * 6) * dp
        }
    }
}
