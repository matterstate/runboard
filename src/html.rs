use rstml_ssr::{component, html, render_scope_with_limit, RenderResult, TrustedHtml};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

pub const PAGE_BYTES_MAX: u32 = 32 << 20;

pub struct Series {
    pub label: String,
    pub points: Vec<(f64, f64)>,
}

pub fn encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

pub fn number(value: f64) -> String {
    if !value.is_finite() {
        return "–".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    if value.abs() >= 10000.0 || value.abs() < 0.001 {
        return format!("{value:.3e}");
    }
    format!("{value:.4}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

pub fn age(output: &mut rstml_ssr::Output, time: SystemTime) -> RenderResult {
    let seconds = SystemTime::now()
        .duration_since(time)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let label = match seconds {
        0..=119 => format!("{seconds}s ago"),
        120..=7199 => format!("{}m ago", seconds / 60),
        7200..=172799 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    };
    let timestamp = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    html! { output, <time data-time=timestamp>{label}</time> }
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

#[component]
fn Navigation() -> RenderResult {
    html! {
        <nav>
            <a href="/">"Runs"</a>
            <a href="/jobs">"Queue"</a>
            <a href="/device">"Devices"</a>
            <a href="/diagnostics">"Inputs"</a>
            <span id="live-status" class="dim" role="status">
                "Connecting…"
            </span>
            <button id="live-toggle" type="button" aria-pressed="false">
                "Pause updates"
            </button>
            <button id="live-apply" type="button" hidden>
                "Apply updates"
            </button>
        </nav>
    }
}

pub fn page(
    title: &str,
    body: impl FnOnce(&mut rstml_ssr::Output) -> RenderResult,
    offline: bool,
) -> RenderResult<String> {
    let live = if offline { "" } else { LIVE_SCRIPT };
    let navigation = component(|output| {
        if !offline {
            html! { output, <Navigation /> }
        } else {
            Ok(())
        }
    });
    render_scope_with_limit(
        |output| {
            html! { output,
                <!DOCTYPE html>
                <html lang="en">
                    <head>
                        <meta charset="utf-8" />
                        <meta name="viewport" content="width=device-width" />
                        <title>{title}" · runboard"</title>
                        <style>{TrustedHtml(STYLE)}</style>
                    </head>
                    <body>
                        {navigation} <main>
                            <h1>{title}</h1>
                            {|output| body(output)}
                        </main> <script>{TrustedHtml(SCRIPT)}{TrustedHtml(live)}</script>
                    </body>
                </html>
            }
        },
        PAGE_BYTES_MAX,
    )
    .map(|markup| markup.into_string())
}

const STYLE: &str = r#"
* { box-sizing: border-box; }
body { font: 14px/1.5 system-ui,sans-serif; max-width: 80rem; margin: 2rem auto; padding: 0 1.25rem; color: #222; background: #fcfcfc; }
body:has(#run-selection) { max-width: none; }
.table-scroll { overflow-x: auto; }
.table-scroll table { display: table; width: 100%; }
nav, .actions { display: flex; flex-wrap: wrap; align-items: center; gap: .75rem 1.25rem; }
nav { padding-bottom: 1rem; border-bottom: 1px solid #ddd; }
#live-status { margin-left: auto; }
h1 { margin: 1rem 0; line-height: 1.2; overflow-wrap: anywhere; }
h2 { margin: 2rem 0 .75rem; font-size: 1.15rem; }
form { margin: 1.5rem 0; }
form.filters { padding-bottom: 1.25rem; border-bottom: 1px solid #ddd; }
fieldset { display: flex; flex-wrap: wrap; align-items: end; gap: .75rem 1rem; min-width: 0; margin: 0 0 1rem; padding: 0; border: 0; }
legend { margin-bottom: .5rem; padding: 0; font-weight: 600; }
label { display: flex; flex-direction: column; gap: .25rem; }
label:has(input[type=checkbox]) { flex-direction: row-reverse; align-items: center; align-self: end; gap: .4rem; padding-bottom: .35rem; }
input, select, button { font: inherit; max-width: 100%; }
input:where(:not([type=checkbox]):not([type=hidden])), select { width: 11rem; min-height: 2rem; padding: .2rem .4rem; }
input[type=number] { width: 8rem; }
button { padding: .25rem .65rem; }
.actions { margin: 1rem 0; }
.metrics { display: flex; flex-wrap: wrap; gap: .75rem 2rem; margin: 1rem 0; }
.metrics dt { color: #666; overflow-wrap: anywhere; }
.metrics dd { margin: 0; font-size: 1.25rem; font-weight: 600; font-variant-numeric: tabular-nums; }
.columns { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 24rem), 1fr)); gap: 0 2rem; }
table { border-collapse: collapse; max-width: 100%; display: block; overflow: auto; margin: .75rem 0 1.25rem; font-variant-numeric: tabular-nums; }
th, td { text-align: left; padding: .6rem .8rem; border-bottom: 1px solid #ddd; vertical-align: top; }
th { white-space: nowrap; }
figure { margin: 1.5rem 0 2.5rem; width: 900px; max-width: 100%; }
figcaption { margin-bottom: .4rem; font-weight: 600; }
.legend { display: flex; flex-wrap: wrap; gap: .25rem 1rem; margin-bottom: .5rem; }
.legend span { min-width: 0; overflow-wrap: anywhere; }
.plot { display: block; width: 100%; height: 252px; background: #f5f7fa; }
.plot text { font: 12px system-ui,sans-serif; fill: #526070; }
@media (max-width: 40rem) {
    .plot [data-x-tick="0.25"], .plot [data-x-tick="0.75"] { display: none; }
}
.chart { touch-action: pan-y; }
.chart:active { cursor: crosshair; }
figure .actions { margin: .5rem 0 0; }
figure output { display: block; min-height: 1.5rem; margin-top: .5rem; overflow-wrap: anywhere; }
.good { color: #187238; } .warn { color: #a00020; } .dim { color: #666; }
pre { overflow: auto; background: #f3f3f3; padding: 1rem; }
img.preview { max-width: min(100%, 40rem); max-height: 25rem; }
details { margin: 1rem 0; }
summary { cursor: pointer; }
[hidden] { display: none !important; }
:focus-visible { outline: 2px solid #245ac7; outline-offset: 3px; }
button { cursor: pointer; min-height: 2.25rem; border: 1px solid #bac3cf; border-radius: .35rem; background: white; }
button:disabled { cursor: default; opacity: .55; }
.status-summary a { border: 1px solid #d7dee7; border-radius: .5rem; padding: .65rem 1rem; color: inherit; text-decoration: none; }
.status-summary a:hover { background: #edf2f8; }
.selection-toolbar { position: sticky; top: 0; z-index: 1; padding: .75rem; background: #fcfcfc; border-block: 1px solid #d7dee7; }
th { background: #edf2f8; }
tr:has(input[name=run]:checked) { background: #eaf1ff; }
.state { display: inline-block; padding: .15rem .5rem; border-radius: .3rem; background: #edf0f4; white-space: nowrap; }
.state[data-state=running], .state[data-state=completed] { color: #126333; background: #e4f3e9; }
.state[data-state=failed], .state[data-state=slow] { color: #872b19; background: #fff0e8; }
.empty { padding: 1rem; background: #edf2f8; }
"#;

pub fn message(message: &str) -> RenderResult<String> {
    page("runboard", |output| html! { output, <p>{message}</p> }, false)
}

pub fn chart(output: &mut rstml_ssr::Output, title: &str, axis: &str, series: &[Series]) -> RenderResult {
    let mut low_x = f64::INFINITY;
    let mut high_x = f64::NEG_INFINITY;
    let mut low_y = f64::INFINITY;
    let mut high_y = f64::NEG_INFINITY;
    let mut reduced = Vec::new();
    for line in series.iter().take(32) {
        let points: Vec<(f64, f64)> = line
            .points
            .iter()
            .copied()
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .collect();
        for (x, y) in &points {
            low_x = low_x.min(*x);
            high_x = high_x.max(*x);
            low_y = low_y.min(*y);
            high_y = high_y.max(*y);
        }
        let mut shown = Vec::new();
        let bucket_size = points.len().div_ceil(500).max(1);
        for bucket in points.chunks(bucket_size) {
            let low = bucket
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| a.1.total_cmp(&b.1))
                .map(|(index, _)| index)
                .unwrap_or(0);
            let high = bucket
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.1.total_cmp(&b.1))
                .map(|(index, _)| index)
                .unwrap_or(0);
            if low <= high {
                shown.push(bucket[low]);
                if low != high {
                    shown.push(bucket[high]);
                }
            } else {
                shown.push(bucket[high]);
                shown.push(bucket[low]);
            }
        }
        if let Some(first) = points.first() {
            shown.insert(0, *first);
        }
        if let Some(last) = points.last() {
            shown.push(*last);
        }
        reduced.push(shown);
    }
    if !low_x.is_finite() {
        return html! { output, <p>"No points for "{title}" on this axis."</p> };
    }
    (low_x, high_x) = chart_domain(low_x, high_x);
    (low_y, high_y) = chart_domain(low_y, high_y);
    let span_x = (high_x / 2.0 - low_x / 2.0).max(f64::MIN_POSITIVE);
    let span_y = (high_y / 2.0 - low_y / 2.0).max(f64::MIN_POSITIVE);
    let mut lines = Vec::new();
    for (index, points) in reduced.iter().enumerate() {
        let hash = series[index].label.bytes().fold(2166136261u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16777619)
        });
        // Only this bounded numeric hash supplies the legend's CSS colour.
        let colour = format!("hsl({},65%,36%)", hash % 360);
        let mut path = String::new();
        for (x, y) in points {
            let px = (x / 2.0 - low_x / 2.0) / span_x * 900.0;
            let py = 180.0 - (y / 2.0 - low_y / 2.0) / span_y * 180.0;
            let _ = write!(path, "{px:.2},{py:.2} ");
        }
        let last = points.last().map(|point| number(point.1)).unwrap_or_else(|| "–".into());
        let samples = serde_json::to_string(points).expect("finite chart points serialize");
        lines.push((colour, path, &series[index].label, last, samples));
    }
    let key = encode(&format!(
        "{title}|{}",
        series
            .iter()
            .map(|line| line.label.as_str())
            .collect::<Vec<_>>()
            .join("|")
    ));
    let legend = component(|output| {
        output.each(&lines, |output, (colour, _, label, last, _)| {
            output.trusted(TrustedHtml("<span"))?;
            output.attribute("style", &format!("color:{colour}"))?;
            output.trusted(TrustedHtml(">"))?;
            output.text(label)?;
            output.text(": ")?;
            output.text(last)?;
            output.trusted(TrustedHtml("</span>"))
        })
    });
    let paths = component(|output| {
        output.each(&lines, |output, (colour, path, label, _, samples)| {
            html! { output,
                <polyline
                    fill="none"
                    stroke=colour
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    vector-effect="non-scaling-stroke"
                    data-label=label
                    data-samples=samples
                    points=path
                />
            }
        })
    });
    let ticks = component(|output| {
        for index in 0..=4 {
            let fraction = f64::from(index) / 4.0;
            let x = low_x * (1.0 - fraction) + high_x * fraction;
            let y = low_y * (1.0 - fraction) + high_y * fraction;
            html! { output,
                <line
                    data-grid
                    x1="60"
                    x2="960"
                    y1={196.0 - fraction * 180.0}
                    y2={196.0 - fraction * 180.0}
                    stroke="#dce2eb"
                />
                <text x="52" y={200.0 - fraction * 180.0} text-anchor="end">
                    {chart_tick(y)}
                </text>
                <text
                    data-x-tick=fraction
                    x={60.0 + fraction * 900.0}
                    y="218"
                    text-anchor={if index == 0 { "start" } else if index == 4 { "end" } else { "middle" }}
                >
                    {chart_tick(x)}
                </text>
            }?;
        }
        Ok(())
    });
    html! { output,
        <figure data-key=key>
            <figcaption>
                {title}" "<span class="dim">"· "{chart_tick(low_y)}" to "{chart_tick(high_y)}</span>
                <span class="dim" data-chart-state></span>
            </figcaption>
            <div class="legend">{legend}</div>
            <svg
                class="plot"
                role="img"
                width="984"
                height="252"
                viewBox="0 0 984 252"
                aria-label=format!("{title} by {axis}")
            >
                {ticks}
                <text data-axis-label x="510" y="244" text-anchor="middle">
                    {axis}
                </text>
                <svg
                    class="chart"
                    x="60"
                    y="16"
                    width="900"
                    height="180"
                    viewBox="0 0 900 180"
                    preserveAspectRatio="none"
                    tabindex="0"
                    aria-label="Chart samples: use left and right arrow keys to inspect"
                    data-axis=axis
                    data-low=low_x
                    data-high=high_x
                >
                    <rect width="900" height="180" fill="transparent" />
                    {paths}
                    <line
                        data-cursor
                        x1="0"
                        x2="0"
                        y1="0"
                        y2="180"
                        stroke="#63738a"
                        stroke-dasharray="3 3"
                        visibility="hidden"
                    />
                </svg>
            </svg>
            <div class="actions">
                <span class="dim">
                    "Hover or use arrow keys for values · Ctrl/⌘ + scroll to zoom · drag to pan when zoomed"
                </span>
                <button type="button" data-zoom="0.8" aria-label="Zoom in">
                    "+"
                </button>
                <button type="button" data-zoom="1.25" aria-label="Zoom out">
                    "−"
                </button>
                <button type="button" data-reset>
                    "Reset zoom"
                </button>
            </div>
            <output aria-live="off"></output>
        </figure>
    }
}

fn chart_tick(value: f64) -> String {
    if value == 0.0 || (0.001..1e12).contains(&value.abs()) {
        let rounded = format!("{value:.11e}")
            .parse::<f64>()
            .expect("bounded finite chart tick");
        return rounded.to_string();
    }
    format!("{value:.5e}")
}

fn chart_domain(low: f64, high: f64) -> (f64, f64) {
    if low != high {
        return (low, high);
    }
    let padding = if low == 0.0 {
        1.0
    } else {
        (low.abs() * 0.05).max(f64::MIN_POSITIVE)
    };
    let lower = low - padding;
    let upper = high + padding;
    (
        if lower.is_finite() { lower } else { low },
        if upper.is_finite() { upper } else { high },
    )
}

const SCRIPT: &str = r##"
let drag = null;
const sample_cache = new WeakMap();

// Keep labels in CSS-pixel coordinates; scale only the inner plot.
const chart_observer = new ResizeObserver(entries => {
    entries.forEach(({target, contentRect}) => {
        const width = contentRect.width;
        if (width <= 84) return;
        const plot_width = width - 84;
        target.setAttribute("viewBox", "0 0 " + width + " 252");
        target.querySelector("svg.chart").setAttribute("width", plot_width);
        target.querySelectorAll("[data-grid]").forEach(line => { line.setAttribute("x2", width - 24); });
        target.querySelectorAll("[data-x-tick]").forEach(tick => {
            tick.setAttribute("x", 60 + Number(tick.dataset.xTick) * plot_width);
        });
        target.querySelector("[data-axis-label]").setAttribute("x", 60 + plot_width / 2);
    });
});

function observe_charts() {
    chart_observer.disconnect();
    document.querySelectorAll("svg.plot").forEach(plot => { chart_observer.observe(plot); });
}

function chart_samples(line) {
    if (!sample_cache.has(line)) sample_cache.set(line, JSON.parse(line.dataset.samples));
    return sample_cache.get(line);
}

function chart_number(value) {
    return Number(value).toLocaleString(undefined, {maximumSignificantDigits: 12});
}

function range_value(svg, fraction) {
    return Number(svg.dataset.low) * (1 - fraction) + Number(svg.dataset.high) * fraction;
}

function zoom_chart(svg, factor, fraction = 0.5) {
    const box = svg.viewBox.baseVal;
    const width = Math.max(9, Math.min(900, box.width * factor));
    box.x = Math.max(0, Math.min(900 - width, box.x + (box.width - width) * fraction));
    box.width = width;
    if (width < 900) svg.dataset.zoomed = "1";
    else delete svg.dataset.zoomed;
    show_range(svg);
}

document.addEventListener("wheel", event => {
    const svg = event.target.closest("svg.chart");
    if (!svg || !(event.ctrlKey || event.metaKey)) return;
    event.preventDefault();
    const rectangle = svg.getBoundingClientRect();
    zoom_chart(svg, event.deltaY > 0 ? 1.25 : 0.8, (event.clientX - rectangle.left) / rectangle.width);
}, {passive: false});

document.addEventListener("pointerdown", event => {
    const svg = event.target.closest("svg.chart");
    if (!svg) return;
    const box = svg.viewBox.baseVal;
    const rectangle = svg.getBoundingClientRect();
    inspect_chart(svg, box.x + (event.clientX - rectangle.left) / rectangle.width * box.width);
    if (box.width >= 900) return;
    drag = {svg, x: event.clientX, start: box.x};
    svg.setPointerCapture(event.pointerId);
});

function inspect_chart(svg, x) {
    const values = [];
    svg.querySelectorAll("polyline").forEach(line => {
        if (!line.points.numberOfItems) return;
        let nearest = 0;
        for (let index = 1; index < line.points.numberOfItems; index++) {
            if (Math.abs(line.points.getItem(index).x - x) < Math.abs(line.points.getItem(nearest).x - x)) nearest = index;
        }
        const point = chart_samples(line)[nearest];
        values.push(line.dataset.label + ": " + chart_number(point[1]) + " at " + svg.dataset.axis + " " + chart_number(point[0]));
    });
    const cursor = svg.querySelector("[data-cursor]");
    cursor.setAttribute("x1", x);
    cursor.setAttribute("x2", x);
    cursor.setAttribute("visibility", "visible");
    svg.closest("figure").querySelector("output").textContent = values.join(" · ");
}

document.addEventListener("pointermove", event => {
    if (drag) {
        const box = drag.svg.viewBox.baseVal;
        const distance = (event.clientX - drag.x) * box.width / drag.svg.getBoundingClientRect().width;
        box.x = Math.max(0, Math.min(900 - box.width, drag.start - distance));
        show_range(drag.svg);
        return;
    }
    const svg = event.target.closest("svg.chart");
    if (!svg) return;
    const box = svg.viewBox.baseVal;
    const rectangle = svg.getBoundingClientRect();
    inspect_chart(svg, box.x + (event.clientX - rectangle.left) / rectangle.width * box.width);
});

document.addEventListener("keydown", event => {
    const svg = event.target.closest("svg.chart");
    if (!svg || !["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    const line = svg.querySelector("polyline");
    if (!line || !line.points.numberOfItems) return;
    event.preventDefault();
    const last = line.points.numberOfItems - 1;
    let index = Number(svg.dataset.sampleIndex || 0);
    if (event.key === "Home") index = 0;
    else if (event.key === "End") index = last;
    else index = Math.max(0, Math.min(last, index + (event.key === "ArrowRight" ? 1 : -1)));
    svg.dataset.sampleIndex = index;
    const x = line.points.getItem(index).x;
    const box = svg.viewBox.baseVal;
    if (x < box.x) box.x = x;
    else if (x > box.x + box.width) box.x = x - box.width;
    show_range(svg);
    inspect_chart(svg, x);
});

document.addEventListener("pointerup", () => { drag = null; });
document.addEventListener("pointercancel", () => { drag = null; });
document.addEventListener("click", event => {
    const button = event.target.closest("[data-reset],[data-zoom]");
    if (!button) return;
    const svg = button.closest("figure").querySelector("svg.chart");
    if (button.hasAttribute("data-reset")) {
        svg.setAttribute("viewBox", "0 0 900 180");
        delete svg.dataset.zoomed;
    } else zoom_chart(svg, Number(button.dataset.zoom));
    show_range(svg);
});

function show_range(svg) {
    const box = svg.viewBox.baseVal;
    const figure = svg.closest("figure");
    figure.querySelector("[data-chart-state]").textContent = svg.dataset.zoomed ? " · paused while zoomed" : "";
    figure.querySelectorAll("[data-x-tick]").forEach(tick => {
        tick.textContent = chart_number(range_value(svg, (box.x + box.width * Number(tick.dataset.xTick)) / 900));
    });
    figure.querySelector("output").textContent = "Visible " + svg.dataset.axis + ": " +
        chart_number(range_value(svg, box.x / 900)) + " to " +
        chart_number(range_value(svg, (box.x + box.width) / 900)) +
        (svg.dataset.zoomed ? " · chart paused while zoomed; Reset zoom resumes updates." : "");
}

let selected_runs = new Set();
try {
    const saved = sessionStorage.getItem("runboard:selected") || "[]";
    const values = saved.length <= 8192 ? JSON.parse(saved) : [];
    if (Array.isArray(values)) selected_runs = new Set(values.filter(value => typeof value === "string" && value.length > 0 && value.length <= 512).slice(0, 8));
} catch (_) { /* Selection still works when storage is unavailable. */ }

function refresh_selection() {
    const form = document.querySelector("#run-selection");
    if (!form) return;
    form.querySelectorAll("input[name=run]").forEach(input => { input.checked = selected_runs.has(input.value); });
    form.querySelector("#selection-count").textContent = selected_runs.size + " selected (up to eight)";
    form.querySelector("#selection-clear").hidden = selected_runs.size === 0;
    form.querySelectorAll("button:not([type=button])").forEach(button => { button.disabled = selected_runs.size === 0; });
}

document.addEventListener("change", event => {
    const input = event.target;
    if (!input.matches("#run-selection input[name=run]")) return;
    if (input.checked && selected_runs.size < 8) selected_runs.add(input.value);
    else selected_runs.delete(input.value);
    try { sessionStorage.setItem("runboard:selected", JSON.stringify(Array.from(selected_runs))); } catch (_) {}
    refresh_selection();
});

document.addEventListener("click", event => {
    if (!event.target.closest("#selection-clear")) return;
    selected_runs.clear();
    try { sessionStorage.removeItem("runboard:selected"); } catch (_) {}
    refresh_selection();
});

document.addEventListener("submit", event => {
    if (event.target.id !== "run-selection") return;
    event.preventDefault();
    const target = new URL(event.submitter?.getAttribute("formaction") || event.target.action, location.href);
    selected_runs.forEach(run => target.searchParams.append("run", run));
    location.assign(target);
});
refresh_selection();
observe_charts();
"##;

const LIVE_SCRIPT: &str = r##"
const live_status = document.querySelector("#live-status");
const live_toggle = document.querySelector("#live-toggle");
const live_apply = document.querySelector("#live-apply");
const updates = new EventSource("/events");
let dirty = false;
let updating = false;
let scheduled = null;
let sequence = 0n;
let status_time = Date.now();
let snapshot_failed = false;
let paused = false;
let connection_status = "Connecting…";

function interacting(ignore_pause = false) {
    if (drag || (paused && !ignore_pause) || document.hidden) return true;
    const selection = getSelection();
    if (selection && !selection.isCollapsed) return true;
    if (document.activeElement?.closest("main")) return true;
    return Boolean(document.querySelector("table:hover,figure:hover,.selection-toolbar:hover"));
}

function update_status() {
    const label = snapshot_failed ? "Update delayed" : connection_status;
    const text = label + (paused ? " · updates paused" : dirty ? " · updates waiting" : "");
    if (live_status.textContent !== text) live_status.textContent = text;
    live_apply.hidden = !dirty;
    live_apply.disabled = updating;
}

function schedule_snapshot() {
    dirty = true;
    update_status();
    if (scheduled !== null || updating || interacting()) return;
    scheduled = setTimeout(update_snapshot, 250);
}

function field_key(element, form) {
    const identity = ["checkbox", "radio", "hidden"].includes(element.type) ? element.value : "";
    return (form.id || form.getAttribute("action") || location.pathname) + ":" + element.name + ":" + element.type + ":" + identity;
}

async function update_snapshot(force = false) {
    if (scheduled !== null) clearTimeout(scheduled);
    scheduled = null;
    if (updating || interacting(force) || (!dirty && !force)) return;
    updating = true;
    dirty = false;
    update_status();
    try {
        const response = await fetch(location.pathname + location.search, {signal: AbortSignal.timeout(10000)});
        if (!response.ok) throw new Error("snapshot failed");
        const markup = await response.text();
        if (markup.length > 33554432) throw new Error("snapshot too large");
        if (interacting(force)) { dirty = true; return; }
        const next = new DOMParser().parseFromString(markup, "text/html").querySelector("main");
        const current = document.querySelector("main");
        if (!next || !current) throw new Error("snapshot missing");
        const fields = new Map();
        current.querySelectorAll("form").forEach(form => {
            form.querySelectorAll("input,select").forEach(element => {
                const key = field_key(element, form);
                fields.set(key, {value: element.value, checked: element.checked});
            });
        });
        const details = new Map();
        current.querySelectorAll("details").forEach((element, index) => {
            details.set(element.id || String(index) + ":" + element.querySelector("summary")?.textContent.split("·")[0], element.open);
        });
        next.querySelectorAll("details").forEach((element, index) => {
            const key = element.id || String(index) + ":" + element.querySelector("summary")?.textContent.split("·")[0];
            if (details.has(key)) element.open = details.get(key);
        });
        const charts = new Map();
        current.querySelectorAll("svg[data-zoomed]").forEach(svg => {
            const figure = svg.closest("figure");
            charts.set(figure.dataset.key, figure);
        });
        next.querySelectorAll("figure[data-key]").forEach(figure => {
            const previous = charts.get(figure.dataset.key);
            if (previous) figure.replaceWith(previous);
        });
        const scroll = {x: scrollX, y: scrollY};
        const table_scroll = Array.from(current.querySelectorAll("table,.table-scroll"), table => table.scrollLeft);
        current.replaceWith(next);
        next.querySelectorAll("form").forEach(form => {
            form.querySelectorAll("input,select").forEach(element => {
                const key = field_key(element, form);
                const previous = fields.get(key);
                if (!previous) return;
                element.value = previous.value;
                if (element.type === "checkbox") element.checked = previous.checked;
            });
        });
        refresh_selection();
        observe_charts();
        next.querySelectorAll("table,.table-scroll").forEach((table, index) => { table.scrollLeft = table_scroll[index] || 0; });
        scrollTo(scroll.x, scroll.y);
        snapshot_failed = false;
    } catch (_) {
        dirty = true;
        snapshot_failed = true;
    } finally {
        updating = false;
        update_status();
        if (dirty && !interacting()) {
            scheduled = setTimeout(() => { scheduled = null; schedule_snapshot(); }, 1000);
        }
    }
}

updates.addEventListener("resync", () => { sequence = 0n; schedule_snapshot(); });
updates.addEventListener("update", event => {
    const next = BigInt(event.lastEventId || 0);
    if (next <= sequence) return;
    sequence = next;
    schedule_snapshot();
});
updates.addEventListener("status", event => {
    status_time = Date.now();
    connection_status = event.data === "live" ? "Connected" : event.data === "input-error" ? "Some inputs unavailable · see Inputs" : "Reconnecting…";
    update_status();
});
updates.onerror = () => { connection_status = "Reconnecting…"; update_status(); };
live_toggle.addEventListener("click", () => {
    paused = !paused;
    live_toggle.setAttribute("aria-pressed", String(paused));
    live_toggle.textContent = paused ? "Resume updates" : "Pause updates";
    update_status();
    if (!paused && dirty) schedule_snapshot();
});
live_apply.addEventListener("click", () => { getSelection()?.removeAllRanges(); update_snapshot(true); });
function resume_snapshot() { if (dirty) schedule_snapshot(); }
document.addEventListener("visibilitychange", resume_snapshot);
document.addEventListener("selectionchange", resume_snapshot);
document.addEventListener("focusout", () => { setTimeout(resume_snapshot, 0); });
document.addEventListener("pointerup", resume_snapshot);
document.addEventListener("pointercancel", resume_snapshot);
document.addEventListener("pointerout", resume_snapshot);
document.addEventListener("click", event => { if (event.target.closest("[data-reset]")) schedule_snapshot(); });
setInterval(() => {
    if (Date.now() - status_time > 15000) { connection_status = "Disconnected · data may be stale"; update_status(); }
    if (interacting()) return;
    document.querySelectorAll("time[data-time]").forEach(element => {
        const seconds = Math.max(0, Math.floor(Date.now() / 1000 - Number(element.dataset.time)));
        element.textContent = seconds < 120 ? seconds + "s ago" : seconds < 7200 ? Math.floor(seconds / 60) + "m ago" : seconds < 172800 ? Math.floor(seconds / 3600) + "h ago" : Math.floor(seconds / 86400) + "d ago";
    });
}, 5000);
"##;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn page_scope_preserves_offline_behavior_and_rejects_partial_documents() {
        let live = page("<title>", |output| html! { output, <p>"escaped <fragment>"</p> }, false).unwrap();
        assert!(live.contains("<title>&lt;title&gt; · runboard</title>"));
        assert!(live.contains("<p>escaped &lt;fragment&gt;</p>"));
        assert!(live.contains("<nav>"));
        assert!(live.contains("new EventSource(\"/events\")"));
        let offline = page("report", |_| Ok(()), true).unwrap();
        assert!(!offline.contains("<nav>"));
        assert!(!offline.contains("new EventSource"));
        assert!(offline.contains("function show_range"));
        assert_eq!(
            page("bad\0title", |_| Ok(()), false),
            Err(rstml_ssr::RenderError::NullCharacter)
        );
        assert_eq!(
            page(
                "large",
                |output| output.text(&"x".repeat(PAGE_BYTES_MAX as usize)),
                false
            ),
            Err(rstml_ssr::RenderError::OutputLimit)
        );
    }

    #[test]
    fn escapes_and_charts_extreme_values() {
        assert_eq!(encode("a/b &"), "a%2Fb%20%26");
        let page = page(
            "chart",
            |output| {
                chart(
                    output,
                    "<metric>",
                    "step",
                    &[Series {
                        label: "<run>".into(),
                        points: vec![(-1e308, -1e308), (1e308, 1e308), (2.0, f64::NAN)],
                    }],
                )
            },
            true,
        )
        .unwrap();
        assert!(!page.contains("NaN"));
        assert!(!page.contains("inf"));
        assert!(page.contains("&lt;metric&gt;"));
        assert!(page.contains("&lt;run&gt;"));
    }

    #[test]
    fn chart_domains_handle_constant_extremes_and_ticks_preserve_step_precision() {
        for value in [0.0, 1.0, -1.0, f64::MAX, -f64::MAX, f64::MIN_POSITIVE] {
            let (low, high) = chart_domain(value, value);
            assert!(low.is_finite());
            assert!(high.is_finite());
            assert!(low < high);
            assert!(low <= value);
            assert!(value <= high);
        }
        assert_ne!(chart_tick(1_000_001.0), chart_tick(1_000_002.0));
    }

    #[test]
    fn chart_reduction_preserves_short_spikes_and_endpoints() {
        let points = (0..20000)
            .map(|index| (f64::from(index), if index == 9123 { 100.0 } else { 1.0 }))
            .collect();
        let page = page(
            "chart",
            |output| {
                chart(
                    output,
                    "spike",
                    "step",
                    &[Series {
                        label: "run".into(),
                        points,
                    }],
                )
            },
            true,
        )
        .unwrap();
        let path = page.split("points=\"").nth(1).unwrap().split('"').next().unwrap();
        let points: Vec<&str> = path.split_whitespace().collect();
        assert!(points.len() <= 1004);
        assert!(points.first().unwrap().starts_with("0.00,"));
        assert!(points.last().unwrap().starts_with("900.00,"));
        assert!(points.iter().any(|point| point.ends_with(",0.00")));
    }
}
