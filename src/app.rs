use crate::data::{self, Record, Stage};
use crate::html::{self, encode, escape, number, Series};
use crate::inputs::{self, Fields, Summary};
use crate::json::Scalar;
use crate::queue;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SELECTED_MAX: usize = 8;
const RESPONSE_BYTES_MAX: usize = 32 << 20;
const PAGE_SIZE: usize = 50;

pub struct Project {
    pub name: String,
    pub root: PathBuf,
}
pub struct Settings {
    pub live: crate::live::Options,
    pub projects: Vec<Project>,
    pub listen: String,
    pub jobs: Option<PathBuf>,
    pub queue: Option<PathBuf>,
    pub pueue: Option<PathBuf>,
    pub artifacts: bool,
    pub export: Option<PathBuf>,
    pub selected: Vec<String>,
    pub queue_cache: Mutex<Option<(Instant, Result<String, String>)>>,
}

pub struct Source {
    pub key: String,
    pub revision: u64,
    pub data: serde_json::Value,
}

pub fn live_sources(settings: &Settings, known: &BTreeMap<String, u64>) -> Result<Vec<Source>, String> {
    use serde_json::json;
    use std::hash::{Hash, Hasher};
    let fingerprint = |paths: &[PathBuf]| {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for path in paths {
            path.hash(&mut hash);
            if let Ok(metadata) = std::fs::metadata(path) {
                metadata.len().hash(&mut hash);
                metadata.modified().ok().hash(&mut hash);
            }
        }
        hash.finish()
    };
    let numbers = |record: &Record| {
        record
            .0
            .iter()
            .filter(|(key, value)| key.len() <= 200 && value.is_finite())
            .take(32)
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect::<serde_json::Map<_, _>>()
    };
    let mut sources = Vec::new();
    for project in &settings.projects {
        for stage in data::stages(&project.root) {
            if sources.len() >= 8192 {
                return Err("Live inputs exceed 8192 entities".into());
            }
            let key = format!("{}/run/{}", project.name, stage.name);
            let mut paths = [
                "_train.log",
                "_meta.json",
                "_summary.json",
                "_TRAINED",
                "_progress.json",
            ]
            .map(|suffix| project.root.join("reports").join(format!("{}{suffix}", stage.name)))
            .to_vec();
            paths.push(project.root.join("checkpoints").join(&stage.name).join("training.json"));
            let heartbeat = stage
                .progress
                .as_ref()
                .map(|progress| progress.modified)
                .unwrap_or(stage.modified);
            let age_band = heartbeat.elapsed().map(|age| (age.as_secs() / 300).min(4)).unwrap_or(0);
            let revision = fingerprint(&paths).wrapping_add(age_band);
            let mut source = Source {
                key,
                revision,
                data: serde_json::Value::Null,
            };
            if known.get(&source.key) != Some(&revision) {
                let entry = Entry::read(project, stage);
                let summaries = entry.summary.as_ref().ok().map(|metrics| metrics.iter().take(32)
                    .map(|metric| json!({"name":metric.name,"value":metric.value,"unit":metric.unit,"direction":metric.direction})).collect::<Vec<_>>()).unwrap_or_default();
                source.data = json!({"kind":"run","project":project.name,"run":entry.stage.name,"state":entry.state(),
                    "metrics":entry.last.as_ref().map(numbers),"summaries":summaries,
                    "progress":entry.stage.progress.as_ref().map(|progress| json!({"phase":progress.phase,"step":progress.step}))});
            }
            sources.push(source);
        }
        let telemetry = project.root.join("reports/device_telemetry.jsonl");
        let key = format!("{}/devices", project.name);
        let revision = fingerprint(&[telemetry]);
        let mut source = Source {
            key,
            revision,
            data: serde_json::Value::Null,
        };
        let device_prefix = format!("{}/device/", project.name);
        if known.get(&source.key) == Some(&revision) {
            sources.extend(
                known
                    .iter()
                    .filter(|(key, _)| key.starts_with(&device_prefix))
                    .map(|(key, revision)| Source {
                        key: key.clone(),
                        revision: *revision,
                        data: serde_json::Value::Null,
                    }),
            );
        } else {
            match inputs::streams(&project.root) {
                Ok(devices) => {
                    for device in devices {
                        let device_key = format!(
                            "{device_prefix}{:016x}",
                            crate::live::hash(format!("{}\0{}", device.host, device.device).as_bytes())
                        );
                        let data = json!({"kind":"device","project":project.name,"host":device.host,"device":device.device,
                            "metrics":device.records.last().map(numbers)});
                        sources.push(Source {
                            key: device_key,
                            revision,
                            data,
                        });
                    }
                    source.data = json!({"kind":"device-source","project":project.name});
                }
                Err(error) => source.data = json!({"kind":"input-error","project":project.name,"error":error}),
            }
        }
        sources.push(source);
        let jobs_path = settings
            .jobs
            .as_ref()
            .cloned()
            .unwrap_or_else(|| project.root.join("reports/jobs.jsonl"));
        let key = format!("{}/history", project.name);
        let revision = fingerprint(std::slice::from_ref(&jobs_path));
        let mut source = Source {
            key,
            revision,
            data: serde_json::Value::Null,
        };
        if known.get(&source.key) != Some(&revision) {
            let history = data::jobs(&jobs_path, 16)
                .iter()
                .map(|job| {
                    json!({"id":job.id,"label":job.label,
                "outcome":job.outcome,"end":job.end})
                })
                .collect::<Vec<_>>();
            source.data = json!({"kind":"history","project":project.name,"tasks":history});
        }
        sources.push(source);
    }
    let snapshots = match queue_body(settings) {
        Ok(Some(snapshot)) => vec![("shared".to_string(), snapshot)],
        Ok(None) => settings
            .projects
            .iter()
            .filter_map(|project| {
                data::read_text(&project.root.join("reports/queue.json"), 4 << 20)
                    .map(|body| (project.name.clone(), body))
            })
            .collect(),
        Err(error) => {
            sources.push(Source {
                key: "queue/error".into(),
                revision: crate::live::hash(error.as_bytes()),
                data: json!({"kind":"input-error","error":error}),
            });
            Vec::new()
        }
    };
    for (name, snapshot) in snapshots {
        let tasks = match queue::tasks(&snapshot) {
            Ok(tasks) => tasks,
            Err(error) => {
                sources.push(Source {
                    key: format!("{name}/queue-error"),
                    revision: crate::live::hash(snapshot.as_bytes()),
                    data: json!({"kind":"input-error","error":error}),
                });
                continue;
            }
        };
        for task in tasks {
            let key = format!("{name}/task/{}", task.id);
            let data = json!({"kind":"task","queue":name,"id":task.id,"label":task.label,"group":task.group,
                "state":task.state,"dependencies":task.dependencies});
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            data.to_string().hash(&mut hash);
            sources.push(Source {
                key,
                revision: hash.finish(),
                data,
            });
            if sources.len() > 8192 {
                return Err("Live inputs exceed 8192 entities".into());
            }
        }
    }
    if sources.len() > 8192 {
        return Err("Live inputs exceed 8192 entities".into());
    }
    Ok(sources)
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub attachment: Option<&'static str>,
}

impl Response {
    fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
            attachment: None,
        }
    }
    pub fn error(status: u16, error: &str) -> Self {
        let mut response = Self::html(html::message(error));
        response.status = status;
        response
    }
}

struct Query(Vec<(String, String)>);
impl Query {
    fn parse(query: &str) -> Result<Self, String> {
        if query.len() > 7000 {
            return Err("query too long".into());
        }
        let mut pairs = Vec::new();
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            if pairs.len() == 64 {
                return Err("too many query fields".into());
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            pairs.push((decode(key)?, decode(value)?));
        }
        Ok(Self(pairs))
    }
    fn get(&self, key: &str) -> &str {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }
    fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .collect()
    }
    fn link(&self, path: &str, key: &str, value: &str) -> String {
        let mut pairs: Vec<String> = self
            .0
            .iter()
            .filter(|(name, _)| name != key)
            .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
            .collect();
        pairs.push(format!("{}={}", encode(key), encode(value)));
        format!("{path}?{}", pairs.join("&"))
    }
}

fn decode(value: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let source = value.as_bytes();
    let mut index = 0;
    while index < source.len() {
        match source[index] {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hex = source.get(index + 1..index + 3).ok_or("short URL escape")?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return Err("invalid URL escape".into());
                }
                let hex = std::str::from_utf8(hex).map_err(|error| error.to_string())?;
                bytes.push(u8::from_str_radix(hex, 16).map_err(|error| error.to_string())?);
                index += 2;
            }
            byte => bytes.push(byte),
        }
        index += 1;
    }
    let decoded = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    if decoded.chars().any(char::is_control) {
        return Err("control character in URL".into());
    }
    Ok(decoded)
}

struct Entry<'a> {
    project: &'a Project,
    stage: Stage,
    metadata: Fields,
    last: Option<Record>,
    summary: Result<Vec<Summary>, String>,
    warnings: Vec<String>,
}
impl<'a> Entry<'a> {
    fn read(project: &'a Project, stage: Stage) -> Self {
        let mut warnings = inputs::log_warnings(&stage);
        let metadata = inputs::metadata(&project.root, &stage.name).unwrap_or_else(|error| {
            warnings.push(format!("metadata: {error}"));
            Vec::new()
        });
        let summary = inputs::summaries(&project.root, &stage.name);
        let last = data::last_record(&stage);
        Self {
            project,
            stage,
            metadata,
            last,
            summary,
            warnings,
        }
    }
    fn metric(&self, name: &str) -> Option<f64> {
        self.last.as_ref().and_then(|record| record.get(name)).or_else(|| {
            self.summary
                .as_ref()
                .ok()?
                .iter()
                .find(|metric| metric.name == name)
                .map(|metric| metric.value)
        })
    }
    fn id(&self) -> String {
        format!("{}/{}", self.project.name, self.stage.name)
    }
    fn state(&self) -> &'static str {
        match self.stage.trained.as_deref() {
            Some("train exit 0") => "completed",
            Some(value) if value.starts_with("cancel") || matches!(value, "train exit 130" | "train exit 143") => {
                "cancelled"
            }
            Some(_) => "failed",
            None => {
                if self.stage.log_path.is_none() {
                    return "recorded";
                }
                if self.last.is_none() && self.stage.progress.is_none() {
                    return "no-data";
                }
                let modified = self
                    .stage
                    .progress
                    .as_ref()
                    .map(|progress| progress.modified)
                    .unwrap_or(self.stage.modified);
                let age = modified.elapsed().map(|value| value.as_secs()).unwrap_or(0);
                match age {
                    0..=299 => "running",
                    300..=1199 => "slow",
                    _ => "stopped",
                }
            }
        }
    }
    fn field(&self, key: &str) -> String {
        self.value(key).map(inputs::scalar).unwrap_or_default()
    }
    fn value(&self, key: &str) -> Option<&Scalar> {
        self.metadata
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
}

fn entries(settings: &Settings) -> Vec<Entry<'_>> {
    let mut entries = Vec::new();
    for project in &settings.projects {
        for stage in data::stages(&project.root) {
            if entries.len() == 4096 {
                return entries;
            }
            entries.push(Entry::read(project, stage));
        }
    }
    entries
}

fn selection<'a>(settings: &'a Settings, ids: &[&str]) -> Result<Vec<Entry<'a>>, String> {
    if ids.is_empty() || ids.len() > SELECTED_MAX {
        return Err("Select between one and eight runs".into());
    }
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for id in ids {
        let (project, run) = id.split_once('/').ok_or("Run IDs have the form project/run")?;
        if !data::name_is_safe(project) || !data::name_is_safe(run) {
            return Err("unsafe run ID".into());
        }
        if !seen.insert(*id) {
            continue;
        }
        let project = settings
            .projects
            .iter()
            .find(|value| value.name == project)
            .ok_or("project not found")?;
        let stage = data::stage(&project.root, run).ok_or("run not found")?;
        selected.push(Entry::read(project, stage));
    }
    Ok(selected)
}

fn notice(errors: &[String]) -> String {
    if errors.is_empty() {
        return String::new();
    }
    format!(
        "<ul class=\"warn\">{}</ul>",
        errors
            .iter()
            .take(100)
            .map(|error| format!("<li>{}</li>", escape(error)))
            .collect::<String>()
    )
}

pub fn route(settings: &Settings, target: &str) -> Response {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let query = match Query::parse(raw_query) {
        Ok(query) => query,
        Err(error) => return Response::error(400, &error),
    };
    for key in ["start", "end", "minimum", "maximum"] {
        if !query.get(key).is_empty() && !query.get(key).parse::<f64>().is_ok_and(f64::is_finite) {
            return Response::error(400, &format!("{key} must be a finite number"));
        }
    }
    if !query.get("axis").is_empty() && !matches!(query.get("axis"), "step" | "seconds" | "time") {
        return Response::error(400, "Axis must be step, seconds or time");
    }
    if !query.get("smooth").is_empty()
        && !query
            .get("smooth")
            .parse::<usize>()
            .is_ok_and(|value| (1..=100).contains(&value))
    {
        return Response::error(400, "Smoothing window must be between 1 and 100");
    }
    let response = match path {
        "/" => Response::html(index(settings, &query)),
        "/run" => match selection(settings, &[query.get("run")]) {
            Ok(selected) => Response::html(run_page(settings, &selected[0], &query)),
            Err(error) => Response::error(404, &error),
        },
        "/compare" => {
            let mut ids = query.all("run");
            if ids.len() > SELECTED_MAX {
                return Response::error(400, "Select at most eight runs");
            }
            ids.retain(|id| !id.is_empty());
            match selection(settings, &ids) {
                Ok(selected) => Response::html(comparison(&selected, &query, false)),
                Err(error) => Response::error(400, &error),
            }
        }
        "/export" => match selection(settings, &query.all("run")) {
            Ok(selected) => match csv(&selected) {
                Ok(body) => Response {
                    status: 200,
                    content_type: "text/csv; charset=utf-8",
                    body: body.into_bytes(),
                    attachment: Some("runs.csv"),
                },
                Err((status, error)) => Response::error(status, &error),
            },
            Err(error) => Response::error(400, &error),
        },
        "/report" => match selection(settings, &query.all("run")) {
            Ok(selected) => {
                let mut response = Response::html(comparison(&selected, &query, true));
                response.attachment = Some("runs.html");
                response
            }
            Err(error) => Response::error(400, &error),
        },
        "/jobs" => Response::html(jobs(settings, &query)),
        "/device" => Response::html(devices(settings, &query)),
        "/diagnostics" => {
            let mut body = String::from("<p>Bounded checks of producer files. Missing optional files are allowed.</p>");
            for project in &settings.projects {
                let errors = inputs::diagnostics(&project.root);
                let _ = write!(
                    body,
                    "<h2>{}</h2>{}",
                    escape(&project.name),
                    if errors.is_empty() {
                        "<p>No errors in inspected inputs.</p>".into()
                    } else {
                        notice(&errors)
                    }
                );
            }
            Response::html(html::page("Inputs", &body, false))
        }
        "/artifact" => artifact_response(settings, &query),
        _ => Response::error(404, "No such page"),
    };
    if response.body.len() > RESPONSE_BYTES_MAX {
        return Response::error(413, "Response exceeds 32 MiB; reduce the selection");
    }
    response
}

pub fn export(settings: &Settings) -> Result<(), String> {
    let path = settings.export.as_ref().ok_or("export path missing")?;
    let ids: Vec<&str> = settings.selected.iter().map(String::as_str).collect();
    let selected = selection(settings, &ids)?;
    let body = comparison(&selected, &Query(Vec::new()), true);
    if body.len() > RESPONSE_BYTES_MAX {
        return Err("report exceeds 32 MiB".into());
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    if let Err(error) = file.write_all(body.as_bytes()) {
        drop(file);
        return match std::fs::remove_file(path) {
            Ok(()) => Err(error.to_string()),
            Err(cleanup) => Err(format!("{error}; cannot remove partial report: {cleanup}")),
        };
    }
    Ok(())
}

fn index(settings: &Settings, query: &Query) -> String {
    let mut runs = entries(settings);
    let capped = runs.len() == 4096;
    let status_counts = ["running", "slow", "failed", "completed"]
        .iter()
        .map(|state| {
            format!(
                "<a href=\"/?state={state}\">{} {state}</a>",
                runs.iter().filter(|entry| entry.state() == *state).count()
            )
        })
        .collect::<String>();
    runs.retain(|entry| {
        (query.get("project").is_empty() || entry.project.name == query.get("project"))
            && (query.get("name").is_empty()
                || entry
                    .stage
                    .name
                    .to_lowercase()
                    .contains(&query.get("name").to_lowercase()))
            && (query.get("state").is_empty() || entry.state() == query.get("state"))
            && (query.get("trainer").is_empty() || entry.stage.trainer == query.get("trainer"))
            && (query.get("tag").is_empty()
                || entry
                    .metadata
                    .iter()
                    .any(|(key, value)| key.starts_with("tags.") && inputs::scalar(value) == query.get("tag")))
    });
    let metric = query.get("metric");
    let value = |entry: &Entry<'_>| entry.metric(metric);
    if let Ok(minimum) = query.get("minimum").parse::<f64>() {
        runs.retain(|entry| value(entry).is_some_and(|value| value >= minimum));
    }
    if let Ok(maximum) = query.get("maximum").parse::<f64>() {
        runs.retain(|entry| value(entry).is_some_and(|value| value <= maximum));
    }
    runs.sort_by(|a, b| {
        let order = match query.get("sort") {
            "name" => a.id().cmp(&b.id()),
            "metric" => match (value(a), value(b)) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            },
            "recent" => b.stage.modified.cmp(&a.stage.modified),
            _ => {
                let active = |entry: &Entry<'_>| matches!(entry.state(), "running" | "slow");
                active(b)
                    .cmp(&active(a))
                    .then_with(|| b.stage.modified.cmp(&a.stage.modified))
            }
        };
        let order = if query.get("descending") == "1" {
            order.reverse()
        } else {
            order
        };
        order.then_with(|| a.id().cmp(&b.id()))
    });
    let page_max = runs.len().saturating_sub(1) / PAGE_SIZE;
    let page = query.get("page").parse::<usize>().unwrap_or(0).min(page_max);
    let active_filters = query
        .0
        .iter()
        .filter(|(key, value)| key != "page" && !value.is_empty())
        .count();
    let mut body = format!("<div class=\"actions\">{status_counts}</div><details><summary>Filters and sorting · {active_filters} active</summary><form class=\"filters\" action=\"/\"><fieldset><legend>Find runs</legend>{}
<label>State <select name=\"state\">{}</select></label></fieldset><div class=\"columns\"><fieldset><legend>Metric range</legend>{}</fieldset><fieldset><legend>Order</legend><label>Sort <select name=\"sort\">{}</select></label><label>Descending <input type=\"checkbox\" name=\"descending\" value=\"1\" {}></label></fieldset></div><div class=\"actions\"><button>Filter</button><a href=\"/\">Reset</a></div></form></details>",
        [("project", "Project"), ("name", "Name"), ("trainer", "Trainer"), ("tag", "Tag")].iter().map(|(key, label)| format!("<label>{label}<input name=\"{key}\" value=\"{}\"></label>", escape(query.get(key)))).collect::<String>(),
        options(&["", "running", "slow", "stopped", "completed", "failed", "cancelled", "recorded", "no-data"], query.get("state")),
        [("metric", "Metric"), ("minimum", "Minimum"), ("maximum", "Maximum")].iter().map(|(key, label)| format!("<label>{label}<input name=\"{key}\" value=\"{}\"></label>", escape(query.get(key)))).collect::<String>(),
        options(&["active", "recent", "name", "metric"], if query.get("sort").is_empty() { "active" } else { query.get("sort") }), if query.get("descending") == "1" { "checked" } else { "" });
    if capped {
        body.push_str("<p class=\"warn\">The combined run index reached its 4096-run limit.</p>");
    }
    let _ = write!(
        body,
        "<p>{} matching runs · page {} of {} · select up to eight.</p>\
        <form action=\"/compare\"><div class=\"actions\"><button>Compare selected</button><button formaction=\"/export\">CSV</button><button formaction=\"/report\">Static report</button></div><table><tr><th>Select</th><th>Project / run</th><th>State</th>\
        <th>Step</th><th>Latest metrics</th><th>Updated</th><th>Trainer time</th><th>Trainer</th></tr>",
        runs.len(),
        page + 1,
        page_max + 1
    );
    for entry in runs.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
        let id = entry.id();
        let mut values = entry
            .last
            .as_ref()
            .map(|record| {
                record
                    .0
                    .iter()
                    .filter(|(key, _)| !matches!(key.as_str(), "step" | "time" | "seconds"))
                    .take(4)
                    .map(|(key, value)| format!("{}: {}", escape(key), number(*value)))
                    .collect::<Vec<_>>()
                    .join("<br>")
            })
            .unwrap_or_default();
        if !metric.is_empty() {
            values = entry
                .metric(metric)
                .map(|value| format!("{}: {}", escape(metric), number(value)))
                .unwrap_or_default();
        } else if values.is_empty() {
            if let Ok(summary) = &entry.summary {
                values = summary
                    .iter()
                    .take(4)
                    .map(|metric| {
                        format!(
                            "{}: {} {}",
                            escape(&metric.name),
                            number(metric.value),
                            escape(&metric.unit)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("<br>");
            }
        }
        let _ = write!(
            body,
            "<tr><td><input aria-label=\"Select {id}\" type=\"checkbox\" name=\"run\" value=\"{id}\"></td>\
             <td><a href=\"/run?run={url}\">{id}</a>{warnings}</td><td>{state}</td>\
             <td>{step}</td><td>{values}</td><td>{updated}</td><td>{time}</td><td>{trainer}</td></tr>",
            id = escape(&id),
            url = encode(&id),
            warnings = notice(&entry.warnings),
            trainer = escape(&entry.stage.trainer),
            state = entry.state(),
            step = entry
                .last
                .as_ref()
                .and_then(|record| record.get("step"))
                .map(number)
                .unwrap_or_default(),
            time = training_time(entry, entry.last.as_ref()),
            updated = html::age(entry.stage.modified)
        );
    }
    body.push_str("</table></form>");
    if page > 0 {
        let _ = write!(
            body,
            "<a href=\"{}\">Previous</a> ",
            escape(&query.link("/", "page", &(page - 1).to_string()))
        );
    }
    if page < page_max {
        let _ = write!(
            body,
            "<a href=\"{}\">Next</a>",
            escape(&query.link("/", "page", &(page + 1).to_string()))
        );
    }
    html::page("Runs", &body, false)
}

fn options(values: &[&str], current: &str) -> String {
    values
        .iter()
        .map(|value| {
            format!(
                "<option value=\"{}\" {}>{}</option>",
                escape(value),
                if current == *value { "selected" } else { "" },
                if value.is_empty() { "All" } else { value }
            )
        })
        .collect()
}

fn controls(selected: &[Entry<'_>], query: &Query, metrics: &[String]) -> String {
    let hidden = selected
        .iter()
        .map(|entry| format!("<input type=\"hidden\" name=\"run\" value=\"{}\">", escape(&entry.id())))
        .collect::<String>();
    let choices = metrics
        .iter()
        .map(|metric| format!("<option value=\"{}\"></option>", escape(metric)))
        .collect::<String>();
    let mut body = format!(
        "<details><summary>Chart options · {} · {}</summary>",
        escape(if query.get("metric").is_empty() {
            "all metrics"
        } else {
            query.get("metric")
        }),
        escape(if query.get("axis").is_empty() {
            "step"
        } else {
            query.get("axis")
        })
    );
    body.push_str(&format!("<form class=\"filters\">{hidden}<fieldset><legend>Chart</legend><label>Metric <input name=\"metric\" list=\"metrics\" value=\"{}\"></label><datalist id=\"metrics\">{choices}</datalist><label>Axis <select name=\"axis\">{}</select></label><label>Smoothing window <input type=\"number\" name=\"smooth\" min=\"1\" max=\"100\" value=\"{}\"></label><label>Seed groups <input type=\"checkbox\" name=\"group\" value=\"1\" {}></label></fieldset><fieldset><legend>Visible range</legend><label>From <input type=\"number\" step=\"any\" name=\"start\" value=\"{}\"></label><label>To <input type=\"number\" step=\"any\" name=\"end\" value=\"{}\"></label><button>Apply</button></fieldset><p class=\"dim\">Blank metric shows all curves. Smoothing is a trailing mean; 1 shows raw values. Axes use logged fields.</p></form></details>", escape(query.get("metric")), options(&["step", "seconds", "time"], query.get("axis")), query.get("smooth").parse::<u32>().unwrap_or(1).clamp(1, 100), if query.get("group") == "1" { "checked" } else { "" }, escape(query.get("start")), escape(query.get("end"))));
    body
}

fn records(entry: &Entry<'_>) -> Vec<Record> {
    data::training_records(&entry.stage)
}

fn training_time(entry: &Entry<'_>, last: Option<&Record>) -> String {
    let Some(time) = data::training_time(&entry.project.root, &entry.stage, last) else {
        return "–".into();
    };
    let seconds = match time {
        data::TrainingTime::Final(seconds) | data::TrainingTime::Recorded(seconds) => seconds,
    };
    let duration = match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        3600..=86399 => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86400, seconds % 86400 / 3600),
    };
    match time {
        data::TrainingTime::Final(_) => duration,
        data::TrainingTime::Recorded(_) => format!("{duration} (last logged)"),
    }
}

fn curve(records: &[Record], metric: &str, query: &Query) -> Vec<(f64, f64)> {
    let axis = match query.get("axis") {
        "seconds" => "seconds",
        "time" => "time",
        _ => "step",
    };
    let window = query.get("smooth").parse::<usize>().unwrap_or(1).clamp(1, 100);
    let start = query.get("start").parse::<f64>().unwrap_or(f64::NEG_INFINITY);
    let end = query.get("end").parse::<f64>().unwrap_or(f64::INFINITY);
    let mut history = std::collections::VecDeque::new();
    let mut points = Vec::new();
    for record in records {
        let (Some(x), Some(y)) = (record.get(axis), record.get(metric)) else {
            continue;
        };
        history.push_back(y);
        if history.len() > window {
            history.pop_front();
        }
        let y = history.iter().map(|value| value / history.len() as f64).sum::<f64>();
        if x >= start && x <= end && y.is_finite() {
            points.push((x, y));
        }
    }
    points
}

fn metric_names(logs: &[Vec<Record>], selected: &str) -> Vec<String> {
    if !selected.is_empty() {
        return vec![selected.into()];
    }
    let mut names = BTreeSet::new();
    for log in logs {
        for record in log.iter().rev() {
            for (key, _) in &record.0 {
                if !matches!(key.as_str(), "step" | "time" | "seconds") && names.len() < 32 {
                    names.insert(key.clone());
                }
            }
        }
    }
    names.into_iter().collect()
}

fn metadata_table(selected: &[Entry<'_>]) -> String {
    let mut keys = BTreeSet::new();
    for entry in selected {
        for (key, _) in &entry.metadata {
            if keys.len() < 128 {
                keys.insert(key.as_str());
            }
        }
    }
    if keys.is_empty() {
        return "<p>No run metadata.</p>".into();
    }
    let mut body = String::from("<table><tr><th>Metadata</th>");
    for entry in selected {
        let _ = write!(body, "<th>{}</th>", escape(&entry.id()));
    }
    body.push_str("</tr>");
    for key in keys {
        let values: Vec<Option<&Scalar>> = selected.iter().map(|entry| entry.value(key)).collect();
        let differs = values.iter().any(|value| *value != values[0]);
        let _ = write!(
            body,
            "<tr><td>{}{}</td>",
            escape(key),
            if differs { " (differs)" } else { "" }
        );
        for value in values {
            let value = match value {
                None => "(missing)".into(),
                Some(Scalar::Text(value)) => format!("{value:?}"),
                Some(value) => inputs::scalar(value),
            };
            let _ = write!(body, "<td>{}</td>", escape(&value));
        }
        body.push_str("</tr>");
    }
    body.push_str("</table>");
    body
}

fn summary_table(selected: &[Entry<'_>]) -> String {
    let mut body = String::new();
    let mut summaries = Vec::new();
    for entry in selected {
        let metrics = match benchmark_metrics(entry) {
            Ok(metrics) => metrics,
            Err(error) => {
                body.push_str(&notice(&[format!("{} summary: {error}", entry.id())]));
                Vec::new()
            }
        };
        summaries.push(metrics);
    }
    let names: BTreeSet<&str> = summaries
        .iter()
        .flat_map(|metrics| metrics.iter().map(|metric| metric.name.as_str()))
        .collect();
    if names.is_empty() {
        return body;
    }
    body.push_str("<h2 id=\"benchmarks\">Benchmark summary</h2><table><tr><th>Metric / unit</th>");
    for entry in selected {
        let _ = write!(body, "<th>{}</th>", escape(&entry.id()));
    }
    body.push_str("</tr>");
    for name in names {
        let values: Vec<Option<&Summary>> = summaries
            .iter()
            .map(|metrics| metrics.iter().find(|metric| metric.name == name))
            .collect();
        let first = *values.iter().flatten().next().expect("metric name came from a summary");
        let _ = write!(
            body,
            "<tr><td>{} ({}, {})</td>",
            escape(name),
            escape(&first.unit),
            escape(&first.direction)
        );
        let baseline = values[0];
        for metric in values {
            if let Some(metric) = metric {
                let compatible = metric.unit == first.unit && metric.direction == first.direction;
                let delta = baseline
                    .filter(|base| compatible && base.unit == metric.unit && base.direction == metric.direction)
                    .map(|base| metric.value - base.value);
                let class = match delta {
                    Some(value)
                        if metric.direction == "higher" && value > 0.0
                            || metric.direction == "lower" && value < 0.0 =>
                    {
                        "good"
                    }
                    Some(value)
                        if metric.direction == "higher" && value < 0.0
                            || metric.direction == "lower" && value > 0.0 =>
                    {
                        "warn"
                    }
                    _ => "",
                };
                let delta = delta.map(|value| format!(" (Δ {})", number(value))).unwrap_or_default();
                let _ = write!(
                    body,
                    "<td class=\"{class}\">{}{} {}</td>",
                    number(metric.value),
                    delta,
                    if compatible { "" } else { "incompatible unit/direction" }
                );
            } else {
                body.push_str("<td>–</td>");
            }
        }
        body.push_str("</tr>");
    }
    body.push_str("</table>");
    body
}

fn benchmark_metrics(entry: &Entry<'_>) -> Result<Vec<Summary>, String> {
    let mut metrics = entry.summary.as_ref().map_err(Clone::clone)?.clone();
    for suffix in data::VARIANTS {
        let system = format!("{}{suffix}", entry.stage.name);
        let variant = if suffix.is_empty() { "base" } else { &suffix[1..] };
        for score in data::panel_scores(&entry.project.root, &system) {
            for (name, value, unit, direction) in [
                ("accuracy", score.system.accuracy, "fraction", "higher"),
                ("nll", score.system.nll, "loss", "lower"),
                ("brier", score.system.brier, "score", "lower"),
                ("reference/accuracy", score.jev.accuracy, "fraction", "higher"),
                ("reference/nll", score.jev.nll, "loss", "lower"),
                ("reference/brier", score.jev.brier, "score", "lower"),
            ] {
                if !value.is_finite() {
                    continue;
                }
                let name = format!("paired/{}/{variant}/{name}", score.panel);
                if metrics.iter().any(|metric| metric.name == name) {
                    continue;
                }
                if metrics.len() == 512 {
                    return Err("benchmark table exceeds 512 metrics".into());
                }
                metrics.push(Summary {
                    name,
                    value,
                    unit: unit.into(),
                    direction: direction.into(),
                });
            }
        }
        for score in data::jevbench_scores(&entry.project.root, &system) {
            for (name, value, unit, direction) in [
                ("accuracy", score.accuracy, "fraction", "higher"),
                ("ece", score.ece, "fraction", "lower"),
                ("brier", score.brier, "score", "lower"),
                ("coverage", score.coverage, "fraction", "higher"),
                ("complete", if score.complete { 1.0 } else { 0.0 }, "boolean", "none"),
            ] {
                if !value.is_finite() {
                    continue;
                }
                let name = format!("jevbench/{}/{variant}/{name}", score.tier);
                if metrics.iter().any(|metric| metric.name == name) {
                    continue;
                }
                if metrics.len() == 512 {
                    return Err("benchmark table exceeds 512 metrics".into());
                }
                metrics.push(Summary {
                    name,
                    value,
                    unit: unit.into(),
                    direction: direction.into(),
                });
            }
        }
    }
    Ok(metrics)
}

fn comparison(selected: &[Entry<'_>], query: &Query, offline: bool) -> String {
    let logs: Vec<Vec<Record>> = selected.iter().map(records).collect();
    let benchmarks = summary_table(selected);
    let mut body = if offline {
        "<p>Selected run metrics and summaries. Metadata tables, queue data, device logs and artifacts are excluded.</p>".into()
    } else {
        String::new()
    };
    if !offline {
        for entry in selected {
            body.push_str(&notice(&entry.warnings));
        }
    }
    body.push_str("<table><tr><th>Run</th><th>State</th><th>Step</th><th>Latest metrics</th><th>Benchmarks</th><th>Trainer time</th></tr>");
    for (entry, log) in selected.iter().zip(&logs) {
        let time = training_time(entry, log.last());
        let latest = log
            .last()
            .map(|record| {
                record
                    .0
                    .iter()
                    .filter(|(key, _)| {
                        !matches!(key.as_str(), "step" | "time" | "seconds")
                            && (query.get("metric").is_empty() || key == query.get("metric"))
                    })
                    .take(2)
                    .map(|(name, value)| format!("{}: {}", escape(name), number(*value)))
                    .collect::<Vec<_>>()
                    .join("<br>")
            })
            .unwrap_or_default();
        let summaries = entry
            .summary
            .as_ref()
            .ok()
            .map(|metrics| {
                metrics
                    .iter()
                    .take(2)
                    .map(|metric| {
                        format!(
                            "{}: {} {}",
                            escape(&metric.name),
                            number(metric.value),
                            escape(&metric.unit)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("<br>")
            })
            .unwrap_or_default();
        let _ = write!(
            body,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{latest}</td><td>{summaries}</td><td>{time}</td></tr>",
            escape(&entry.id()),
            entry.state(),
            log.last()
                .and_then(|record| record.get("step"))
                .map(number)
                .unwrap_or_else(|| "–".into())
        );
    }
    body.push_str("</table>");
    if !offline {
        body.push_str("<div class=\"actions\">");
        if !benchmarks.is_empty() {
            body.push_str("<a href=\"#benchmarks\">Benchmarks</a>");
        }
        body.push_str("<a href=\"#comparison-metadata\">Metadata differences</a></div>");
        body.push_str(&controls(selected, query, &metric_names(&logs, "")));
    }
    for metric in metric_names(&logs, query.get("metric")) {
        let series: Vec<Series> = selected
            .iter()
            .zip(&logs)
            .map(|(entry, log)| Series {
                label: entry.id(),
                points: curve(log, &metric, query),
            })
            .collect();
        body.push_str(&html::chart(&metric, &series));
        if query.get("group") == "1" {
            body.push_str(&seed_groups(selected, &series));
        }
    }
    if !offline {
        body.push_str("<details id=\"comparison-metadata\"><summary>Metadata differences</summary>");
        body.push_str(&metadata_table(selected));
        body.push_str("</details>");
    }
    body.push_str(&benchmarks);
    if !offline {
        let selection = query
            .0
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let _ = write!(body, "<div class=\"actions\"><a href=\"/export?{selection}\">Download CSV</a><a href=\"/report?{selection}\">Download static report</a></div>");
    }
    html::page(if offline { "Run report" } else { "Compare runs" }, &body, offline)
}

fn seed_groups(selected: &[Entry<'_>], series: &[Series]) -> String {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, entry) in selected.iter().enumerate() {
        let group = entry.field("group");
        if !group.is_empty() && !entry.field("seed").is_empty() {
            groups
                .entry(format!("{}/{}", entry.project.name, group))
                .or_default()
                .push(index);
        }
    }
    let mut body = String::from("<table><tr><th>Seed group</th><th>Distinct seeds</th><th>Last shared x</th><th>Mean</th><th>Sample standard deviation</th><th>Standard error</th></tr>");
    let mut curves = Vec::new();
    for (group, indices) in groups {
        let mut seeds = BTreeSet::new();
        if indices
            .iter()
            .any(|index| !seeds.insert(selected[*index].field("seed")))
        {
            let _ = write!(
                body,
                "<tr><td>{}</td><td colspan=\"5\">Duplicate seed; aggregation withheld</td></tr>",
                escape(&group)
            );
            continue;
        }
        if indices.len() < 2 {
            continue;
        }
        let baseline = &selected[indices[0]];
        let comparable: BTreeSet<&str> = indices
            .iter()
            .flat_map(|index| selected[*index].metadata.iter().map(|(key, _)| key.as_str()))
            .filter(|key| matches!(*key, "revision" | "dataset") || key.starts_with("parameters."))
            .collect();
        if comparable.iter().any(|key| {
            indices
                .iter()
                .any(|index| selected[*index].value(key) != baseline.value(key))
        }) {
            let _ = write!(body, "<tr><td>{}</td><td colspan=\"5\">Different parameters, revision or dataset; aggregation withheld</td></tr>", escape(&group));
            continue;
        }
        let mut common = BTreeMap::<String, Vec<f64>>::new();
        for (position, index) in indices.iter().enumerate() {
            let mut observed = BTreeSet::new();
            for (x, y) in &series[*index].points {
                let key = x.to_string();
                if !observed.insert(key.clone()) {
                    continue;
                }
                let values = common.entry(key).or_default();
                if values.len() == position {
                    values.push(*y);
                }
            }
        }
        let Some((x, values)) = common
            .iter()
            .filter(|(_, values)| values.len() == indices.len())
            .max_by(|(a, _), (b, _)| {
                a.parse::<f64>()
                    .unwrap_or(0.0)
                    .total_cmp(&b.parse::<f64>().unwrap_or(0.0))
            })
        else {
            continue;
        };
        let count = values.len() as f64;
        let mean = values.iter().map(|value| value / count).sum::<f64>();
        let variance = values
            .iter()
            .map(|value| (value - mean).powi(2) / (count - 1.0))
            .sum::<f64>();
        let deviation = variance.sqrt();
        let mut average = Vec::new();
        let mut upper = Vec::new();
        let mut lower = Vec::new();
        for (x, values) in common.iter().filter(|(_, values)| values.len() == indices.len()) {
            let x = x.parse::<f64>().expect("keys come from finite x values");
            let mean = values.iter().map(|value| value / count).sum::<f64>();
            let deviation = values
                .iter()
                .map(|value| (value - mean).powi(2) / (count - 1.0))
                .sum::<f64>()
                .sqrt();
            average.push((x, mean));
            upper.push((x, mean + deviation));
            lower.push((x, mean - deviation));
        }
        for points in [&mut average, &mut upper, &mut lower] {
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        curves.push(Series {
            label: format!("{group} mean"),
            points: average,
        });
        curves.push(Series {
            label: format!("{group} + sample SD"),
            points: upper,
        });
        curves.push(Series {
            label: format!("{group} − sample SD"),
            points: lower,
        });
        let _ = write!(
            body,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&group),
            indices.len(),
            escape(x),
            number(mean),
            number(deviation),
            number(deviation / count.sqrt())
        );
    }
    body.push_str("</table><p>Aggregation uses distinct declared seeds at exactly shared x values; standard error assumes independent seeds.</p>");
    if !curves.is_empty() {
        body.push_str(&html::chart("Seed group mean ± sample standard deviation", &curves));
    }
    body
}

fn run_page(settings: &Settings, entry: &Entry<'_>, query: &Query) -> String {
    let selected = std::slice::from_ref(entry);
    let log = records(entry);
    let mut body = String::new();
    let _ =
        write!(
        body,
        "<p><strong>{}</strong> · step {} · updated {} · trainer time {}.</p><p class=\"dim\">{} · {} records</p>{}",
        entry.state(),
        log.last().and_then(|record| record.get("step")).map(number).unwrap_or_else(|| "–".into()),
        html::age(entry.stage.modified),
        training_time(entry, log.last()),
        escape(&entry.stage.trainer),
        log.len(),
        notice(&entry.warnings)
    );
    if let Some(progress) = &entry.stage.progress {
        let _ = write!(
            body,
            "<p>Producer PID {} · {} at step {} · heartbeat {}</p>",
            progress.pid,
            escape(&progress.phase),
            progress.step,
            html::age(progress.modified)
        );
    }
    let names = metric_names(std::slice::from_ref(&log), query.get("metric"));
    body.push_str("<dl class=\"metrics\">");
    for metric in names.iter().take(6) {
        let value = log.iter().rev().find_map(|record| record.get(metric));
        let _ = write!(
            body,
            "<div><dt>{}</dt><dd>{}</dd></div>",
            escape(metric),
            value.map(number).unwrap_or_else(|| "–".into())
        );
    }
    if let Ok(metrics) = &entry.summary {
        for metric in metrics.iter().take(2) {
            let _ = write!(
                body,
                "<div><dt>Summary: {}</dt><dd>{} {}</dd></div>",
                escape(&metric.name),
                number(metric.value),
                escape(&metric.unit)
            );
        }
    }
    body.push_str("</dl>");
    if names.len() > 6 {
        body.push_str("<p class=\"dim\">Showing six latest metrics; all metrics are available in the charts.</p>");
    }
    body.push_str("<div class=\"actions\"><a href=\"#metadata\">Metadata</a>");
    let benchmarks = summary_table(selected);
    if !benchmarks.is_empty() {
        body.push_str("<a href=\"#benchmarks\">Benchmarks</a>");
    }
    body.push_str("</div>");
    if log.is_empty() {
        body.push_str(&benchmarks);
    }
    body.push_str(&controls(
        selected,
        query,
        &metric_names(std::slice::from_ref(&log), ""),
    ));
    for metric in names {
        body.push_str(&html::chart(
            &metric,
            &[Series {
                label: entry.id(),
                points: curve(&log, &metric, query),
            }],
        ));
    }
    if !log.is_empty() {
        body.push_str(&benchmarks);
    }
    body.push_str("<details id=\"metadata\"><summary>Metadata</summary>");
    body.push_str(&metadata_table(selected));
    body.push_str("</details>");
    let events = data::monitor_events(&entry.stage);
    if !events.is_empty() {
        body.push_str("<h2>Monitor events</h2><table><tr><th>Event</th><th>Phase</th><th>Step</th><th>Idle seconds</th><th>Exit</th><th>Time</th></tr>");
        for event in events {
            let _ = write!(
                body,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&event.kind),
                escape(event.phase.as_deref().unwrap_or("")),
                event.step.map(|value| value.to_string()).unwrap_or_default(),
                event.idle_seconds.map(|value| value.to_string()).unwrap_or_default(),
                event.exit_code.map(|value| value.to_string()).unwrap_or_default(),
                html::age(event.time)
            );
        }
        body.push_str("</table>");
    }
    if settings.artifacts {
        body.push_str(&artifact_list(entry));
    }
    let _ = write!(
        body,
        "<p><a href=\"/compare?run={}\">Compare / export this run</a></p>",
        encode(&entry.id())
    );
    html::page(&entry.id(), &body, false)
}

fn csv_field(value: &str) -> String {
    let significant = value.trim_start_matches(|character: char| character.is_whitespace() || character == '\u{feff}');
    let dangerous = significant.starts_with(['=', '+', '-', '@']) || value.starts_with(['\t', '\r', '\n']);
    format!("\"{}{}\"", if dangerous { "'" } else { "" }, value.replace('"', "\"\""))
}

fn csv(selected: &[Entry<'_>]) -> Result<String, (u16, String)> {
    let mut output = String::from("project,run,step,seconds,time,metric,value,kind,unit,direction\r\n");
    for entry in selected {
        for record in records(entry) {
            let axis = |key: &str| record.get(key).map(|value| value.to_string()).unwrap_or_default();
            for (key, value) in &record.0 {
                if matches!(key.as_str(), "step" | "seconds" | "time") {
                    continue;
                }
                let _ = write!(
                    output,
                    "{},{},{},{},{},{},{},record,,\r\n",
                    csv_field(&entry.project.name),
                    csv_field(&entry.stage.name),
                    axis("step"),
                    axis("seconds"),
                    axis("time"),
                    csv_field(key),
                    value
                );
                if output.len() > RESPONSE_BYTES_MAX {
                    return Err((413, "CSV exceeds 32 MiB; export fewer runs".into()));
                }
            }
        }
        let summaries = benchmark_metrics(entry).map_err(|error| (422, format!("{}: {error}", entry.id())))?;
        for metric in summaries {
            let _ = write!(
                output,
                "{},{},,,,{},{},summary,{},{}\r\n",
                csv_field(&entry.project.name),
                csv_field(&entry.stage.name),
                csv_field(&metric.name),
                metric.value,
                csv_field(&metric.unit),
                csv_field(&metric.direction)
            );
            if output.len() > RESPONSE_BYTES_MAX {
                return Err((413, "CSV exceeds 32 MiB".into()));
            }
        }
    }
    Ok(output)
}

fn artifact_list(entry: &Entry<'_>) -> String {
    let files = match inputs::artifacts(&entry.project.root, &entry.stage.name) {
        Ok(files) => files,
        Err(error) => return notice(&[format!("artifacts: {error}")]),
    };
    let mut body = String::from("<h2>Artifacts</h2><ul>");
    for (name, size) in files {
        let url = format!("/artifact?run={}&file={}", encode(&entry.id()), encode(&name));
        let _ = write!(body, "<li><a href=\"{url}\">{}</a> ({size} bytes)", escape(&name));
        if matches!(name.rsplit('.').next(), Some("png" | "webp" | "jpg" | "jpeg")) && size <= 4 << 20 {
            let _ = write!(body, "<details><summary>Preview</summary><img class=\"preview\" loading=\"lazy\" src=\"{url}\" alt=\"{}\"></details>", escape(&name));
        } else if matches!(name.rsplit('.').next(), Some("txt" | "json" | "csv" | "toml" | "md")) && size <= 64 << 10 {
            if let Ok(bytes) = inputs::artifact(&entry.project.root, &entry.stage.name, &name) {
                let _ = write!(
                    body,
                    "<details><summary>Preview</summary><pre>{}</pre></details>",
                    escape(&String::from_utf8_lossy(&bytes))
                );
            }
        }
        body.push_str("</li>");
    }
    body.push_str("</ul>");
    body
}

fn artifact_response(settings: &Settings, query: &Query) -> Response {
    if !settings.artifacts {
        return Response::error(404, "Artifact access is disabled");
    }
    let selected = match selection(settings, &[query.get("run")]) {
        Ok(selected) => selected,
        Err(error) => return Response::error(404, &error),
    };
    let entry = &selected[0];
    let name = query.get("file");
    match inputs::artifact(&entry.project.root, &entry.stage.name, name) {
        Ok(body) => {
            let content_type = match name.rsplit('.').next() {
                Some("png") => "image/png",
                Some("webp") => "image/webp",
                Some("jpg" | "jpeg") => "image/jpeg",
                _ => "application/octet-stream",
            };
            Response {
                status: 200,
                content_type,
                body,
                attachment: if content_type.starts_with("image/") {
                    None
                } else {
                    Some("artifact.bin")
                },
            }
        }
        Err(error) => Response::error(404, &error),
    }
}

fn queue_body(settings: &Settings) -> Result<Option<String>, String> {
    if let Some(config) = &settings.pueue {
        let mut cached = settings.queue_cache.lock().map_err(|_| "queue cache unavailable")?;
        if let Some((time, body)) = &*cached {
            if time.elapsed() < Duration::from_secs(5) {
                return body.clone().map(Some);
            }
        }
        let body = queue::snapshot(config);
        *cached = Some((Instant::now(), body.clone()));
        return body.map(Some);
    }
    if let Some(path) = &settings.queue {
        return data::read_text(path, 4 << 20)
            .map(Some)
            .ok_or_else(|| "queue snapshot unreadable or exceeds 4 MiB".into());
    }
    Ok(None)
}

fn jobs(settings: &Settings, query: &Query) -> String {
    let runs = entries(settings);
    let mut body = format!(
        "<details><summary>Queue filters · {}{}</summary><form class=\"filters\"><fieldset><legend>Queue filters</legend><label>Group <input name=\"group\" value=\"{}\"></label><label>Show completed <input type=\"checkbox\" name=\"completed\" value=\"1\" {}></label><button>Filter</button></fieldset></form></details>",
        escape(if query.get("group").is_empty() { "all groups" } else { query.get("group") }),
        if query.get("completed") == "1" { " · includes completed" } else { "" },
        escape(query.get("group")), if query.get("completed") == "1" { "checked" } else { "" }
    );
    let mut snapshots = Vec::new();
    match queue_body(settings) {
        Ok(Some(body)) => snapshots.push(("shared".to_string(), body)),
        Ok(None) => {
            for project in &settings.projects {
                let path = project.root.join("reports/queue.json");
                if path.exists() {
                    if let Some(body) = data::read_text(&path, 4 << 20) {
                        snapshots.push((project.name.clone(), body));
                    } else {
                        body.push_str(&notice(&[format!("{}: queue snapshot unreadable", project.name)]));
                    }
                }
            }
        }
        Err(error) => body.push_str(&notice(&[error])),
    }
    if snapshots.is_empty() {
        body.push_str(
            "<p>No live queue source. Use --pueue &lt;config&gt;, --queue &lt;snapshot&gt; or reports/queue.json.</p>",
        );
    }
    if let Some(path) = &settings.queue {
        if let Ok(time) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) {
            let _ = write!(body, "<p>Shared snapshot updated {}.</p>", html::age(time));
        }
    }
    for project in &settings.projects {
        if settings.queue.is_none() && settings.pueue.is_none() {
            if let Ok(time) =
                std::fs::metadata(project.root.join("reports/queue.json")).and_then(|metadata| metadata.modified())
            {
                let _ = write!(
                    body,
                    "<p>{} snapshot updated {}.</p>",
                    escape(&project.name),
                    html::age(time)
                );
            }
        }
    }
    body.push_str("<h2>Live tasks</h2><table><tr><th>Queue / task</th><th>Group</th><th>State</th><th>Label / run</th><th>Dependencies</th></tr>");
    for (source, snapshot) in snapshots {
        match queue::tasks(&snapshot) {
            Ok(tasks) => {
                for task in tasks {
                    if task.state == "Done" && query.get("completed") != "1" {
                        continue;
                    }
                    if !query.get("group").is_empty() && task.group != query.get("group") {
                        continue;
                    }
                    let global = settings.queue.is_some() || settings.pueue.is_some();
                    let mut matches = runs.iter().filter(|entry| {
                        let local = source == entry.project.name
                            || settings.projects.len() == 1
                            || Path::new(&task.directory).starts_with(&entry.project.root);
                        task.label == entry.id()
                            || ((global || source == entry.project.name) && entry.field("task_id") == task.id)
                            || (local && task.label == entry.stage.name)
                    });
                    let first = matches.next();
                    let associated = if matches.next().is_none() { first } else { None };
                    let label = associated
                        .map(|entry| {
                            format!(
                                "<a href=\"/run?run={}\">{}</a>",
                                encode(&entry.id()),
                                escape(&entry.id())
                            )
                        })
                        .unwrap_or_else(|| escape(&task.label));
                    let dependencies = task
                        .dependencies
                        .iter()
                        .map(|id| {
                            format!(
                                "<a href=\"/jobs?completed=1#task-{}-{}\">{}</a>",
                                encode(&source),
                                encode(id),
                                escape(id)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = write!(body, "<tr id=\"task-{}-{}\"><td>{}/{}</td><td>{}</td><td>{}</td><td>{label}</td><td>{dependencies}</td></tr>", encode(&source), encode(&task.id), escape(&source), escape(&task.id), escape(&task.group), escape(&task.state));
                }
            }
            Err(error) => body.push_str(&notice(&[format!("{source}: {error}")])),
        }
    }
    body.push_str("</table><h2>Completion history</h2><table><tr><th>Project / task</th><th>Label</th><th>Outcome</th><th>Start</th><th>End</th><th>Faults</th></tr>");
    let sources: Vec<(String, PathBuf)> = match &settings.jobs {
        Some(path) => vec![("shared".into(), path.clone())],
        None => settings
            .projects
            .iter()
            .map(|project| (project.name.clone(), project.root.join("reports/jobs.jsonl")))
            .collect(),
    };
    for (source, path) in sources {
        for job in data::jobs(&path, 100) {
            let faults = job
                .faults
                .iter()
                .map(|(kind, count)| format!("{} × {}", escape(kind), number(*count)))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(
                body,
                "<tr><td>{}/{}</td><td>{}</td><td class=\"{}\">{}</td><td>{}</td><td>{}</td><td>{faults}</td></tr>",
                escape(&source),
                number(job.id),
                escape(&job.label),
                if job.outcome == "Success" { "good" } else { "warn" },
                escape(&job.outcome),
                escape(&job.start),
                escape(&job.end)
            );
        }
    }
    body.push_str("</table>");
    html::page("Queue", &body, false)
}

fn devices(settings: &Settings, query: &Query) -> String {
    let mut body = format!(
        "<details><summary>Device filters · {} active</summary><form class=\"filters\"><fieldset><legend>Device filters</legend>{}<button>Filter</button></fieldset></form></details>",
        ["project", "host", "device"].iter().filter(|key| !query.get(key).is_empty()).count(),
        [("project", "Project"), ("host", "Host"), ("device", "Device")]
            .iter()
            .map(|(key, label)| format!(
                "<label>{label}<input name=\"{key}\" value=\"{}\"></label>",
                escape(query.get(key))
            ))
            .collect::<String>()
    );
    for project in &settings.projects {
        if !query.get("project").is_empty() && project.name != query.get("project") {
            continue;
        }
        let streams = match inputs::streams(&project.root) {
            Ok(streams) => streams,
            Err(error) => {
                body.push_str(&notice(&[format!("{}: {error}", project.name)]));
                continue;
            }
        };
        for stream in streams {
            if !query.get("host").is_empty() && stream.host != query.get("host") {
                continue;
            }
            if !query.get("device").is_empty() && stream.device != query.get("device") {
                continue;
            }
            let title = format!("{} / {} / {}", project.name, stream.host, stream.device);
            let latest = stream
                .records
                .last()
                .and_then(|record| record.get("time"))
                .unwrap_or(0.0);
            let age = (html::now() - latest).max(0.0);
            let _ = write!(
                body,
                "<h2>{}</h2><p class=\"{}\">Sample age: {} seconds{}</p>",
                escape(&title),
                if age >= 300.0 { "warn" } else { "" },
                number(age),
                if age >= 300.0 {
                    " · stale; charts show historical readings"
                } else {
                    ""
                }
            );
            let names = metric_names(std::slice::from_ref(&stream.records), "");
            body.push_str("<dl class=\"metrics\">");
            if let Some(latest) = stream.records.last() {
                for metric in names.iter().take(6) {
                    let _ = write!(
                        body,
                        "<div><dt>{}</dt><dd>{}</dd></div>",
                        escape(metric),
                        latest.get(metric).map(number).unwrap_or_else(|| "–".into())
                    );
                }
            }
            body.push_str("</dl>");
            for metric in names {
                let points = stream
                    .records
                    .iter()
                    .filter_map(|record| Some(((record.get("time")? - html::now()) / 3600.0, record.get(&metric)?)))
                    .collect();
                body.push_str(&html::chart(
                    &format!("{metric} · hours before now"),
                    &[Series {
                        label: title.clone(),
                        points,
                    }],
                ));
            }
        }
    }
    html::page("Devices", &body, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        settings: Settings,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "runboard-features-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut projects = Vec::new();
            for (name, loss) in [("a", 1), ("b", 2)] {
                let path = root.join(name);
                fs::create_dir_all(path.join("reports")).unwrap();
                fs::write(path.join("reports/same_train.log"), format!("{{\"step\":1,\"seconds\":10,\"loss\":4}}\n{{\"step\":2,\"seconds\":20,\"loss\":{loss},\"custom\":0.75}}\n")).unwrap();
                fs::write(path.join("reports/same_meta.json"), r#"{"group":"trial","seed":1,"revision":"abc","dataset":"demo","parameters":{"lr":0.01},"tags":["pilot"],"task_id":7,"secret":"private-metadata"}"#).unwrap();
                fs::write(
                    path.join("reports/same_summary.json"),
                    format!(r#"{{"metrics":{{"latency":{{"value":{loss},"unit":"ms","direction":"lower"}}}}}}"#),
                )
                .unwrap();
                projects.push(Project {
                    name: name.into(),
                    root: path,
                });
            }
            let settings = Settings {
                live: crate::live::Options::default(),
                projects,
                listen: "127.0.0.1:0".into(),
                jobs: None,
                queue: None,
                pueue: None,
                artifacts: false,
                export: None,
                selected: Vec::new(),
                queue_cache: Mutex::new(None),
            };
            Self { root, settings }
        }
        fn text(&self, path: &str) -> String {
            let response = route(&self.settings, path);
            assert_eq!(response.status, 200, "{}", String::from_utf8_lossy(&response.body));
            String::from_utf8(response.body).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn project_ids_do_not_collide_and_filters_are_stable() {
        let fixture = Fixture::new();
        let page = fixture.text("/");
        assert!(page.contains("a/same"));
        assert!(page.contains("b/same"));
        let filtered = fixture.text("/?project=a&name=same&tag=pilot&metric=loss&maximum=1.5&sort=metric");
        assert!(filtered.contains("a/same"));
        assert!(!filtered.contains("b/same"));
        let page = fixture.text("/compare?run=a%2Fsame&run=b%2Fsame&metric=loss&axis=seconds&smooth=2");
        assert!(page.contains("a/same"));
        assert!(page.contains("b/same"));
        assert!(page.contains("Benchmark summary"));
        assert!(page.contains("Metadata differences"));
        assert_eq!(route(&fixture.settings, "/run?run=unknown/same").status, 404);
        assert_eq!(route(&fixture.settings, "/run?run=a%2F..%2Fprivate").status, 404);
        assert_eq!(route(&fixture.settings, "/?name=%ZZ").status, 400);
    }

    #[test]
    fn pagination_keeps_filters_and_orders_ties() {
        let fixture = Fixture::new();
        for index in 0..55 {
            fs::write(
                fixture.root.join(format!("a/reports/run-{index:02}_train.log")),
                "{\"step\":1}\n",
            )
            .unwrap();
        }
        let first = fixture.text("/?project=a&sort=name");
        assert!(first.contains("a/run-00"));
        assert!(!first.contains("a/run-54"));
        assert!(first.contains("project=a&amp;sort=name&amp;page=1"));
        let last = fixture.text("/?project=a&sort=name&page=1");
        assert!(last.contains("a/run-54"));
        assert!(!last.contains("a/run-00"));
    }

    #[test]
    fn active_runs_precede_newer_completed_runs_by_default() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("b/reports/same_TRAINED"), "train exit 0").unwrap();
        let completed = fs::File::open(fixture.root.join("b/reports/same_train.log")).unwrap();
        completed
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now() + Duration::from_secs(60)))
            .unwrap();
        let page = fixture.text("/");
        assert!(page.find("a/same").unwrap() < page.find("b/same").unwrap());
        let page = fixture.text("/?sort=recent");
        assert!(page.find("b/same").unwrap() < page.find("a/same").unwrap());
    }

    #[test]
    fn reports_exclude_private_metadata_and_csv_is_inert() {
        let fixture = Fixture::new();
        let page = fixture.text("/run?run=a/same");
        assert!(page.contains("private-metadata"));
        let report = fixture.text("/report?run=a/same&metric=loss");
        assert!(!report.contains("private-metadata"));
        assert!(!report.contains("Metadata differences"));
        assert!(!report.contains("<nav>"));
        assert!(!report.contains("<form>"));
        assert!(!report.contains("/artifact?"));
        assert!(!report.contains("new EventSource"));
        let response = route(&fixture.settings, "/export?run=a/same&run=b/same");
        assert_eq!(response.status, 200);
        assert_eq!(response.attachment, Some("runs.csv"));
        let csv = String::from_utf8(response.body).unwrap();
        assert!(csv.contains("\"a\",\"same\",2,20,,\"loss\",1"));
        assert!(csv.contains("\"b\",\"same\",2,20,,\"loss\",2"));
        assert_eq!(csv_field("=SUM(A1)"), "\"'=SUM(A1)\"");
        assert_eq!(csv_field("  =SUM(A1)"), "\"'  =SUM(A1)\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn metadata_is_escaped_and_bad_inputs_are_visible() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root.join("a/reports/same_meta.json"),
            r#"{"revision":"<script>alert(1)</script>"}"#,
        )
        .unwrap();
        let page = fixture.text("/run?run=a/same");
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!page.contains("<script>alert(1)</script>"));
        fs::write(
            fixture.root.join("a/reports/same_meta.json"),
            r#"{"parameters":{"lr":"0.01"},"empty":""}"#,
        )
        .unwrap();
        let differences = fixture.text("/compare?run=a/same&run=b/same");
        assert!(differences.contains("parameters.lr (differs)"));
        assert!(differences.contains("empty (differs)"));
        assert!(differences.contains("(missing)"));
        fs::write(fixture.root.join("a/reports/same_meta.json"), "broken").unwrap();
        fs::write(
            fixture.root.join("a/reports/same_train.log"),
            "{\"step\":1,\"loss\":2}\n{\"step\":",
        )
        .unwrap();
        let page = fixture.text("/run?run=a/same");
        assert!(page.contains("metadata:"));
        assert!(page.contains("<dt>loss</dt><dd>2</dd>"));
        let diagnostics = fixture.text("/diagnostics");
        assert!(diagnostics.contains("same_meta.json"));
        assert!(diagnostics.contains("invalid or partial"));
    }

    #[test]
    fn queue_snapshot_links_runs_and_dependencies_without_commands() {
        let mut fixture = Fixture::new();
        fs::write(fixture.root.join("a/reports/queue.json"), r#"{"tasks":{"7":{"label":"same","group":"gpu","command":"hidden-command","status":{"Running":{"start":"now"}},"dependencies":[3]},"3":{"status":"Queued"}}}"#).unwrap();
        let page = fixture.text("/jobs");
        assert!(page.contains("Running"));
        assert!(page.contains("Queued"));
        assert!(page.contains("/run?run=a%2Fsame"));
        assert!(page.contains("#task-a-3"));
        assert!(!page.contains("hidden-command"));
        fixture.settings.queue = Some(fixture.root.join("a/reports/queue.json"));
        assert!(!fixture.text("/jobs").contains("/run?run=a%2Fsame"));
        fs::write(fixture.root.join("b/reports/same_meta.json"), r#"{"task_id":8}"#).unwrap();
        assert!(fixture.text("/jobs").contains("/run?run=a%2Fsame"));
    }

    #[test]
    fn host_device_streams_have_independent_freshness() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a/reports/device_telemetry.jsonl"), format!("{{\"time\":1,\"host\":\"old-host\",\"device\":\"gpu0\",\"temperature\":70}}\n{{\"time\":{},\"host\":\"new-host\",\"device\":\"gpu1\",\"temperature\":60}}\n", html::now())).unwrap();
        let page = fixture.text("/device");
        assert!(page.contains("old-host / gpu0"));
        assert!(page.contains("new-host / gpu1"));
        assert!(page.contains("stale; charts show historical"));
        let page = fixture.text("/device?host=new-host");
        assert!(page.contains("new-host / gpu1"));
        assert!(!page.contains("old-host / gpu0"));
    }

    #[test]
    fn artifacts_require_opt_in_and_reject_traversal() {
        let mut fixture = Fixture::new();
        let directory = fixture.root.join("a/artifacts/same");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("result.txt"), "<script>unsafe</script>").unwrap();
        assert_eq!(
            route(&fixture.settings, "/artifact?run=a/same&file=result.txt").status,
            404
        );
        fixture.settings.artifacts = true;
        let page = fixture.text("/run?run=a/same");
        assert!(page.contains("result.txt"));
        assert!(page.contains("&lt;script&gt;unsafe&lt;/script&gt;"));
        let response = route(&fixture.settings, "/artifact?run=a/same&file=result.txt");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"<script>unsafe</script>");
        assert_eq!(response.content_type, "application/octet-stream");
        assert_eq!(
            route(&fixture.settings, "/artifact?run=a/same&file=..%2F..%2Fprivate").status,
            404
        );
        #[cfg(unix)]
        {
            fs::write(fixture.root.join("private"), "secret").unwrap();
            std::os::unix::fs::symlink(fixture.root.join("private"), directory.join("link.txt")).unwrap();
            assert_eq!(
                route(&fixture.settings, "/artifact?run=a/same&file=link.txt").status,
                404
            );
            assert!(!fixture.text("/run?run=a/same").contains("link.txt"));
        }
    }

    #[test]
    fn seed_groups_use_shared_steps_and_distinct_seeds() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root.join("a/reports/next_train.log"),
            "{\"step\":1,\"loss\":6}\n{\"step\":2,\"loss\":3}\n",
        )
        .unwrap();
        fs::write(
            fixture.root.join("a/reports/next_meta.json"),
            r#"{"group":"trial","seed":2,"revision":"abc","dataset":"demo","parameters":{"lr":0.01}}"#,
        )
        .unwrap();
        let page = fixture.text("/compare?run=a/same&run=a/next&metric=loss&group=1");
        assert!(page.contains("Sample standard deviation"));
        assert!(page.contains("a/trial"));
        assert!(page.contains("<td>2</td><td>2</td><td>2</td>"));
        fs::write(
            fixture.root.join("a/reports/next_meta.json"),
            r#"{"group":"trial","seed":1}"#,
        )
        .unwrap();
        assert!(fixture
            .text("/compare?run=a/same&run=a/next&group=1")
            .contains("Duplicate seed"));
    }

    #[test]
    fn smoothing_and_ranges_keep_their_meaning() {
        let records = vec![
            Record(vec![
                ("step".into(), 1.0),
                ("seconds".into(), 10.0),
                ("loss".into(), 4.0),
            ]),
            Record(vec![
                ("step".into(), 2.0),
                ("seconds".into(), 20.0),
                ("loss".into(), 2.0),
            ]),
        ];
        let query = Query::parse("axis=seconds&smooth=2&start=15&end=25").unwrap();
        assert_eq!(curve(&records, "loss", &query), [(20.0, 3.0)]);
        let query = Query::parse("axis=time").unwrap();
        assert!(curve(&records, "loss", &query).is_empty());
    }

    #[test]
    fn summary_only_runs_and_incompatible_units_are_explicit() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.join("a/reports/benchmark_summary.json"),
            r#"{"metrics":{"latency":{"value":0.5,"unit":"seconds","direction":"lower"}}}"#,
        )
        .unwrap();
        assert!(fixture.text("/?state=recorded").contains("a/benchmark"));
        let filtered = fixture.text("/?metric=latency&maximum=0.6&sort=metric");
        assert!(filtered.contains("a/benchmark"));
        assert!(!filtered.contains("a/same"));
        let page = fixture.text("/run?run=a/benchmark");
        assert!(page.contains("Benchmark summary"));
        assert!(page.contains("0 records"));
        let page = fixture.text("/compare?run=a/same&run=a/benchmark");
        assert!(page.contains("incompatible unit/direction"));
        let csv = fixture.text("/export?run=a/benchmark");
        assert!(csv.contains("summary,\"seconds\",\"lower\""));
        std::fs::write(
            fixture.root.join("a/reports/benchmark_summary.json"),
            r#"{"metrics":{"latency":{"value":1,"direction":"sideways"}}}"#,
        )
        .unwrap();
        assert!(fixture.text("/run?run=a/benchmark").contains("direction must be"));
        assert_eq!(route(&fixture.settings, "/export?run=a/benchmark").status, 422);
    }

    #[test]
    fn native_benchmark_scores_are_compared_and_exported() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.join("a/reports/panel_same_paired.json"),
            r#"{"all":{"candidate":{"accuracy":0.8,"nll":0.3,"brier":0.2},"reference":{"accuracy":0.6,"nll":0.7,"brier":0.4}}}"#,
        )
        .unwrap();
        let directory = fixture.root.join("a/reports/jevbench/same/hard");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("summary.json"),
            r#"{"accuracy":0.8,"ece":{"ece":0.1},"brier_mean":0.2,"coverage":0.9,"complete":true}"#,
        )
        .unwrap();
        let page = fixture.text("/compare?run=a/same&run=b/same");
        assert!(page.contains("paired/panel/base/reference/nll"));
        assert!(page.contains("jevbench/hard/base/ece"));
        let csv = fixture.text("/export?run=a/same");
        assert!(csv.contains("paired/panel/base/reference/nll\",0.7"));
        assert!(csv.contains("jevbench/hard/base/ece\",0.1"));
    }

    #[test]
    fn validates_query_bounds_and_metadata_limits() {
        let fixture = Fixture::new();
        for query in ["smooth=0", "smooth=101", "minimum=NaN", "axis=bogus", "start=Infinity"] {
            assert_eq!(route(&fixture.settings, &format!("/?{query}")).status, 400);
        }
        let fields = (0..129)
            .map(|index| format!("\"field{index}\":1"))
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(fixture.root.join("a/reports/same_meta.json"), format!("{{{fields}}}")).unwrap();
        assert!(fixture.text("/run?run=a/same").contains("metadata exceeds 128 fields"));
        let selection = ["run=a/same"; 9].join("&");
        assert_eq!(route(&fixture.settings, &format!("/compare?{selection}")).status, 400);
    }

    #[test]
    fn live_sources_emit_changed_metrics_and_isolate_queue_errors() {
        let fixture = Fixture::new();
        let initial = live_sources(&fixture.settings, &BTreeMap::new()).unwrap();
        let run = initial.iter().find(|source| source.key == "a/run/same").unwrap();
        assert_eq!(run.data["metrics"]["loss"].as_f64(), Some(1.0));
        assert!(!run.data.to_string().contains("private-metadata"));
        let known = initial
            .iter()
            .map(|source| (source.key.clone(), source.revision))
            .collect::<BTreeMap<_, _>>();
        let unchanged = live_sources(&fixture.settings, &known).unwrap();
        assert!(unchanged
            .iter()
            .find(|source| source.key == "a/run/same")
            .unwrap()
            .data
            .is_null());
        fs::write(
            fixture.root.join("a/reports/same_train.log"),
            "{\"step\":3,\"loss\":0.5}\n",
        )
        .unwrap();
        fs::write(fixture.root.join("a/reports/queue.json"), "broken").unwrap();
        let changed = live_sources(&fixture.settings, &known).unwrap();
        assert_eq!(
            changed.iter().find(|source| source.key == "a/run/same").unwrap().data["metrics"]["loss"],
            0.5
        );
        assert!(changed.iter().any(|source| source.data["kind"] == "input-error"));
    }

    #[test]
    fn static_export_requires_selection_and_never_overwrites() {
        let mut fixture = Fixture::new();
        let path = fixture.root.join("report.html");
        fixture.settings.export = Some(path.clone());
        assert!(export(&fixture.settings).is_err());
        fixture.settings.selected = vec!["a/same".into()];
        export(&fixture.settings).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&original).contains("private-metadata"));
        assert!(export(&fixture.settings).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}
