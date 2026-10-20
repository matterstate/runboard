use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Series {
    pub label: String,
    pub points: Vec<(f64, f64)>,
}

pub fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

pub fn age(time: SystemTime) -> String {
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
    format!("<time data-time=\"{timestamp}\">{label}</time>")
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn page(title: &str, body: &str, offline: bool) -> String {
    let navigation = if offline {
        ""
    } else {
        "<nav><a href=\"/\">Runs</a><a href=\"/jobs\">Queue</a><a href=\"/device\">Devices</a><a href=\"/diagnostics\">Inputs</a><span id=\"live-status\" class=\"dim\" role=\"status\">Connecting…</span></nav>"
    };
    let live = if offline { "" } else { LIVE_SCRIPT };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>{} · runboard</title>
<style>{STYLE}</style></head><body>{navigation}<main><h1>{}</h1>{body}</main>
<script>{SCRIPT}
{live}
</script></body></html>"#,
        escape(title),
        escape(title)
    )
}

const STYLE: &str = r#"
* { box-sizing: border-box; }
body { font: 14px/1.5 system-ui,sans-serif; max-width: 80rem; margin: 2rem auto; padding: 0 1.25rem; color: #222; background: #fcfcfc; }
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
svg { display: block; width: 100%; background: #f3f3f3; touch-action: none; }
figure .actions { margin: .5rem 0 0; }
output:not(:empty) { display: block; margin-top: .5rem; }
.good { color: #187238; } .warn { color: #a00020; } .dim { color: #666; }
pre { overflow: auto; background: #f3f3f3; padding: 1rem; }
img.preview { max-width: min(100%, 40rem); max-height: 25rem; }
details { margin: 1rem 0; }
summary { cursor: pointer; }
"#;

pub fn message(message: &str) -> String {
    page("runboard", &format!("<p>{}</p>", escape(message)), false)
}

pub fn chart(title: &str, series: &[Series]) -> String {
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
        return format!("<p>No points for {} on this axis.</p>", escape(title));
    }
    let span_x = (high_x / 2.0 - low_x / 2.0).max(f64::MIN_POSITIVE);
    let span_y = (high_y / 2.0 - low_y / 2.0).max(f64::MIN_POSITIVE);
    let mut paths = String::new();
    let mut legend = String::new();
    for (index, points) in reduced.iter().enumerate() {
        let hash = series[index].label.bytes().fold(2166136261u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16777619)
        });
        let colour = format!("hsl({},65%,36%)", hash % 360);
        let mut path = String::new();
        for (x, y) in points {
            let px = (x / 2.0 - low_x / 2.0) / span_x * 900.0;
            let py = 180.0 - (y / 2.0 - low_y / 2.0) / span_y * 180.0;
            let _ = write!(path, "{px:.2},{py:.2} ");
        }
        let _ = write!(paths, "<polyline fill=\"none\" stroke=\"{colour}\" stroke-width=\"1.5\" vector-effect=\"non-scaling-stroke\" points=\"{path}\"/>");
        let last = points.last().map(|point| number(point.1)).unwrap_or_else(|| "–".into());
        let _ = write!(
            legend,
            "<span style=\"color:{colour}\">{}: {last}</span>",
            escape(&series[index].label)
        );
    }
    let key = encode(&format!(
        "{title}|{}",
        series
            .iter()
            .map(|line| line.label.as_str())
            .collect::<Vec<_>>()
            .join("|")
    ));
    format!("<figure data-key=\"{key}\"><figcaption>{} <span class=\"dim\">· {} to {}</span></figcaption><div class=\"legend\">{legend}</div><svg class=\"chart\" role=\"img\" width=\"900\" height=\"180\" style=\"aspect-ratio:5/1;height:auto\" preserveAspectRatio=\"none\" aria-label=\"{}\" viewBox=\"0 0 900 180\" data-low=\"{low_x}\" data-high=\"{high_x}\">{paths}</svg><div class=\"actions\"><span class=\"dim\">x: {} to {} · wheel to zoom, drag to pan</span><button type=\"button\" data-reset>Reset zoom</button></div><output></output></figure>", escape(title), number(low_y), number(high_y), escape(title), number(low_x), number(high_x))
}

const SCRIPT: &str = r#"
let drag = null;
document.addEventListener("wheel", (event) => {
    const svg = event.target.closest("svg.chart");
    if (!svg) return;
    event.preventDefault();
    const box = svg.viewBox.baseVal;
    const rectangle = svg.getBoundingClientRect();
    const width = Math.max(9, Math.min(900, box.width * (event.deltaY > 0 ? 1.25 : 0.8)));
    const fraction = (event.clientX - rectangle.left) / rectangle.width;
    box.x = Math.max(0, Math.min(900 - width, box.x + (box.width - width) * fraction));
    box.width = width;
    svg.dataset.zoomed = "1";
    show_range(svg);
}, { passive: false });

document.addEventListener("pointerdown", (event) => {
    const svg = event.target.closest("svg.chart");
    if (!svg) return;
    drag = { svg, x: event.clientX, start: svg.viewBox.baseVal.x };
    svg.setPointerCapture(event.pointerId);
});

document.addEventListener("pointermove", (event) => {
    if (!drag) return;
    const box = drag.svg.viewBox.baseVal;
    const distance = (event.clientX - drag.x) * box.width / drag.svg.getBoundingClientRect().width;
    box.x = Math.max(0, Math.min(900 - box.width, drag.start - distance));
    drag.svg.dataset.zoomed = "1";
    show_range(drag.svg);
});

document.addEventListener("pointerup", () => { drag = null; });
document.addEventListener("pointercancel", () => { drag = null; });
document.addEventListener("click", (event) => {
    if (!event.target.closest("[data-reset]")) return;
    const svg = event.target.closest("figure").querySelector("svg");
    svg.setAttribute("viewBox", "0 0 900 180");
    delete svg.dataset.zoomed;
    show_range(svg);
});

function show_range(svg) {
    const low = Number(svg.dataset.low);
    const span = Number(svg.dataset.high) - low;
    const box = svg.viewBox.baseVal;
    const start = (low + span * box.x / 900).toPrecision(5);
    const end = (low + span * (box.x + box.width) / 900).toPrecision(5);
    svg.closest("figure").querySelector("output").textContent = `Visible x: ${start} to ${end}`;
}
"#;

const LIVE_SCRIPT: &str = r#"
const live_status = document.querySelector('#live-status');
const updates = new EventSource('/events');
let dirty = false;
let updating = false;
let scheduled = false;
let sequence = 0n;
let status_time = Date.now();
let snapshot_failed = false;

function schedule_snapshot() {
    dirty = true;
    if (scheduled || updating || document.hidden || drag) return;
    scheduled = true;
    setTimeout(update_snapshot, 250);
}

function field_key(element, form_index, field_index) {
    return `${form_index}:${element.name}:${element.type}:${element.type === 'checkbox' ? element.value : field_index}`;
}

async function update_snapshot() {
    scheduled = false;
    if (document.hidden || drag || updating) return;
    updating = true;
    dirty = false;
    try {
        const response = await fetch(location.pathname + location.search, {signal: AbortSignal.timeout(10000)});
        if (!response.ok) throw new Error('snapshot failed');
        const markup = await response.text();
        if (markup.length > 33554432) throw new Error('snapshot too large');
        const next = new DOMParser().parseFromString(markup, 'text/html').querySelector('main');
        const current = document.querySelector('main');
        if (!next || !current) throw new Error('snapshot missing');
        const fields = new Map();
        let active = null;
        current.querySelectorAll('form').forEach((form, form_index) => {
            form.querySelectorAll('input,select').forEach((element, field_index) => {
                const key = field_key(element, form_index, field_index);
                fields.set(key, {value: element.value, checked: element.checked, start: element.selectionStart, end: element.selectionEnd});
                if (element === document.activeElement) active = key;
            });
        });
        const details = new Map();
        current.querySelectorAll('details').forEach((element, index) => {
            details.set(element.id || `${index}:${element.querySelector('summary')?.textContent.split('·')[0]}`, element.open);
        });
        next.querySelectorAll('details').forEach((element, index) => {
            element.open = details.get(element.id || `${index}:${element.querySelector('summary')?.textContent.split('·')[0]}`) || false;
        });
        const charts = new Map();
        current.querySelectorAll('svg[data-zoomed]').forEach(svg => {
            const figure = svg.closest('figure');
            charts.set(figure.dataset.key, figure);
        });
        next.querySelectorAll('figure[data-key]').forEach(figure => {
            const previous = charts.get(figure.dataset.key);
            if (previous) {
                previous.querySelector('output').textContent = 'Chart paused while zoomed; Reset zoom resumes live updates.';
                figure.replaceWith(previous);
            }
        });
        const scroll = {x: scrollX, y: scrollY};
        const table_scroll = Array.from(current.querySelectorAll('table'), table => table.scrollLeft);
        current.replaceWith(next);
        next.querySelectorAll('form').forEach((form, form_index) => {
            form.querySelectorAll('input,select').forEach((element, field_index) => {
                const key = field_key(element, form_index, field_index);
                const previous = fields.get(key);
                if (!previous) return;
                element.value = previous.value;
                if (element.type === 'checkbox') element.checked = previous.checked;
                if (active === key) {
                    element.focus({preventScroll: true});
                    if (previous.start !== null && element.setSelectionRange) element.setSelectionRange(previous.start, previous.end);
                }
            });
        });
        next.querySelectorAll('table').forEach((table, index) => { table.scrollLeft = table_scroll[index] || 0; });
        scrollTo(scroll.x, scroll.y);
        snapshot_failed = false;
    } catch (_) {
        dirty = true;
        snapshot_failed = true;
        live_status.textContent = 'Update delayed';
    } finally {
        updating = false;
        if (dirty) setTimeout(schedule_snapshot, 1000);
    }
}

updates.addEventListener('resync', () => { sequence = 0n; schedule_snapshot(); });
updates.addEventListener('update', event => {
    const next = BigInt(event.lastEventId || 0);
    if (next <= sequence) return;
    sequence = next;
    schedule_snapshot();
});
updates.addEventListener('status', event => {
    status_time = Date.now();
    live_status.textContent = event.data === 'live' ? (snapshot_failed ? 'Update delayed' : 'Live') : event.data === 'input-error' ? 'Input error' : 'Reconnecting…';
});
updates.onerror = () => { live_status.textContent = 'Reconnecting…'; };
document.addEventListener('visibilitychange', () => { if (!document.hidden) schedule_snapshot(); });
document.addEventListener('pointerup', () => { if (dirty) schedule_snapshot(); });
document.addEventListener('click', event => { if (event.target.closest('[data-reset]')) schedule_snapshot(); });
setInterval(() => {
    if (Date.now() - status_time > 15000) live_status.textContent = 'Disconnected · data may be stale';
    document.querySelectorAll('time[data-time]').forEach(element => {
        const seconds = Math.max(0, Math.floor(Date.now() / 1000 - Number(element.dataset.time)));
        element.textContent = seconds < 120 ? `${seconds}s ago` : seconds < 7200 ? `${Math.floor(seconds / 60)}m ago` : seconds < 172800 ? `${Math.floor(seconds / 3600)}h ago` : `${Math.floor(seconds / 86400)}d ago`;
    });
}, 5000);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_and_charts_extreme_values() {
        assert_eq!(escape("<script>\"&"), "&lt;script&gt;&quot;&amp;");
        assert_eq!(encode("a/b &"), "a%2Fb%20%26");
        let page = chart(
            "<metric>",
            &[Series {
                label: "<run>".into(),
                points: vec![(-1e308, -1e308), (1e308, 1e308), (2.0, f64::NAN)],
            }],
        );
        assert!(!page.contains("NaN"));
        assert!(!page.contains("inf"));
        assert!(page.contains("&lt;metric&gt;"));
        assert!(page.contains("&lt;run&gt;"));
    }

    #[test]
    fn chart_reduction_preserves_short_spikes_and_endpoints() {
        let points = (0..20000)
            .map(|index| (f64::from(index), if index == 9123 { 100.0 } else { 1.0 }))
            .collect();
        let page = chart(
            "spike",
            &[Series {
                label: "run".into(),
                points,
            }],
        );
        let path = page.split("points=\"").nth(1).unwrap().split('"').next().unwrap();
        let points: Vec<&str> = path.split_whitespace().collect();
        assert!(points.len() <= 1004);
        assert!(points.first().unwrap().starts_with("0.00,"));
        assert!(points.last().unwrap().starts_with("900.00,"));
        assert!(points.iter().any(|point| point.ends_with(",0.00")));
    }
}
