use crate::data::{self, Record, Stage};
use crate::html::{self, encode, number, Series};
use crate::inputs::{self, Fields, Summary};
use crate::json::Scalar;
use crate::queue;
use rstml_ssr::{html, Output, RenderResult};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SELECTED_MAX: usize = 8;
const RESPONSE_BYTES_MAX: usize = html::PAGE_BYTES_MAX as usize;
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
                    .map(|metric| json!({"name":metric.name,"value":metric.value,"unit":metric.unit,"direction":metric.direction.as_str()})).collect::<Vec<_>>()).unwrap_or_default();
                source.data = json!({"kind":"run","project":project.name,"run":entry.stage.name,"state":entry.state().as_str(),
                    "metrics":entry.last.as_ref().map(numbers),"summaries":summaries,
                    "progress":entry.stage.progress.as_ref().map(|progress| json!({"phase":progress.phase.as_str(),"step":progress.step}))});
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
    fn html(body: rstml_ssr::RenderResult<String>) -> Self {
        match body {
            Ok(body) => Self {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: body.into_bytes(),
                attachment: None,
            },
            Err(error) => Self::error(
                if error == rstml_ssr::RenderError::OutputLimit {
                    413
                } else {
                    500
                },
                &error.to_string(),
            ),
        }
    }
    pub fn error(status: u16, error: &str) -> Self {
        match html::message(error) {
            Ok(body) => Self {
                status,
                content_type: "text/html; charset=utf-8",
                body: body.into_bytes(),
                attachment: None,
            },
            Err(_) => Self {
                status,
                content_type: "text/plain; charset=utf-8",
                body: b"Unable to render error response".to_vec(),
                attachment: None,
            },
        }
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
            .filter(|(name, _)| name != key && (key == "page" || name != "page"))
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunState {
    Running,
    Slow,
    Failed,
    Completed,
    Cancelled,
    Recorded,
    NoData,
    Stopped,
}

impl RunState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Slow => "slow",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Recorded => "recorded",
            Self::NoData => "no-data",
            Self::Stopped => "stopped",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "slow" => Some(Self::Slow),
            "failed" => Some(Self::Failed),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            "recorded" => Some(Self::Recorded),
            "no-data" => Some(Self::NoData),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
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
    fn state(&self) -> RunState {
        match self.stage.trained.as_deref() {
            Some("train exit 0") => RunState::Completed,
            Some(value) if value.starts_with("cancel") || matches!(value, "train exit 130" | "train exit 143") => {
                RunState::Cancelled
            }
            Some(_) => RunState::Failed,
            None => {
                if self.stage.log_path.is_none() {
                    return RunState::Recorded;
                }
                if self.last.is_none() && self.stage.progress.is_none() {
                    return RunState::NoData;
                }
                let modified = self
                    .stage
                    .progress
                    .as_ref()
                    .map(|progress| progress.modified)
                    .unwrap_or(self.stage.modified);
                let age = modified.elapsed().map(|value| value.as_secs()).unwrap_or(0);
                match age {
                    0..=299 => RunState::Running,
                    300..=1199 => RunState::Slow,
                    _ => RunState::Stopped,
                }
            }
        }
    }
    fn task_id(&self) -> Option<String> {
        let id = match self.value("task_id")? {
            Scalar::Number(value) if (0.0..=9_007_199_254_740_991.0).contains(value) && value.fract() == 0.0 => {
                format!("{value:.0}")
            }
            Scalar::Text(value) => value.clone(),
            _ => return None,
        };
        if id.is_empty() || id.len() > 16 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(id)
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

fn notice(output: &mut Output, errors: &[String]) -> RenderResult {
    if errors.is_empty() {
        return Ok(());
    }
    let errors = &errors[..errors.len().min(100)];
    html! { output,
        <ul class="warn">
            {|output| output.each(errors, |output, error| html! { output, <li>{error}</li> })}
        </ul>
    }
}

fn diagnostics(settings: &Settings) -> RenderResult<String> {
    html::page(
        "Inputs",
        |output| {
            html! { output, <p>"Bounded checks of producer files. Missing optional files are allowed."</p> }?;
            output.each(&settings.projects, |output, project| {
                let errors = inputs::diagnostics(&project.root);
                html! { output, <h2>{project.name}</h2> }?;
                if errors.is_empty() {
                    html! { output, <p>"No errors in inspected inputs."</p> }
                } else {
                    notice(output, &errors)
                }
            })
        },
        false,
    )
}

fn metric_lines(output: &mut Output, values: &[String]) -> RenderResult {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            html! { output, <br /> }?;
        }
        output.text(value)?;
    }
    Ok(())
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
    if !query.get("axis").is_empty()
        && !matches!(
            query.get("axis"),
            "auto" | "tokens" | "flops" | "step" | "seconds" | "time"
        )
    {
        return Response::error(400, "Axis must be auto, tokens, flops, step, seconds or time");
    }
    if !query.get("smooth").is_empty()
        && !query
            .get("smooth")
            .parse::<usize>()
            .is_ok_and(|value| (1..=100).contains(&value))
    {
        return Response::error(400, "Smoothing window must be between 1 and 100");
    }
    if MetricAggregation::parse(query.get("aggregate")).is_none() {
        return Response::error(400, "aggregate must be latest, mean, median, min or max");
    }
    if Smoothing::parse(query.get("smoothing"), data::MetricKind::Training).is_none() {
        return Response::error(400, "smoothing must be auto, raw, mean, ema or gaussian");
    }
    if !query.get("bandwidth").is_empty()
        && !query
            .get("bandwidth")
            .parse::<f64>()
            .is_ok_and(|value| value.is_finite() && (0.5..=10.0).contains(&value))
    {
        return Response::error(400, "bandwidth must be between 0.5 and 10 percent");
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
                if response.status == 200 {
                    response.attachment = Some("runs.html");
                }
                response
            }
            Err(error) => Response::error(400, &error),
        },
        "/jobs" => Response::html(jobs(settings, &query)),
        "/device" => Response::html(devices(settings, &query)),
        "/diagnostics" => Response::html(diagnostics(settings)),
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
    let body = comparison(&selected, &Query(Vec::new()), true).map_err(|error| error.to_string())?;
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

fn index(settings: &Settings, query: &Query) -> RenderResult<String> {
    let mut runs = entries(settings);
    let mut queue_warnings = Vec::new();
    let mut task_links: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (source, snapshot) in queue_snapshots(settings, &mut queue_warnings) {
        match queue::tasks(&snapshot) {
            Ok(tasks) => {
                for task in tasks {
                    if let Some(entry) = queue_run(settings, &source, &task, &runs) {
                        task_links
                            .entry(entry.id())
                            .or_default()
                            .push((source.clone(), task.id));
                    }
                }
            }
            Err(error) => queue_warnings.push(format!("{source}: {error}")),
        }
    }
    let capped = runs.len() == 4096;
    runs.retain(|entry| query.get("project").is_empty() || entry.project.name == query.get("project"));
    let no_reports = runs.is_empty();
    let status_counts: Vec<_> = [RunState::Running, RunState::Slow, RunState::Failed, RunState::Completed]
        .iter()
        .map(|state| {
            (
                state.as_str(),
                runs.iter().filter(|entry| entry.state() == *state).count(),
            )
        })
        .collect();
    runs.retain(|entry| {
        (query.get("name").is_empty()
            || entry
                .stage
                .name
                .to_lowercase()
                .contains(&query.get("name").to_lowercase()))
            && (query.get("state").is_empty() || Some(entry.state()) == RunState::parse(query.get("state")))
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
            "active" => {
                let active = |entry: &Entry<'_>| matches!(entry.state(), RunState::Running | RunState::Slow);
                active(b).cmp(&active(a))
            }
            _ => b.stage.modified.cmp(&a.stage.modified),
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
    html::page(
        "Runs",
        |output| {
            notice(output, &queue_warnings)?;
            html! { output,
                <div class="actions status-summary">
                    {|output| {
                        output
                            .each(
                                &status_counts,
                                |output, (state, count)| {
                                    html! { output,
                                        <a href=query
                                            .link(
                                                "/",
                                                "state",
                                                state,
                                            )>
                                            {count}" "
                                            {if *state == "slow" { "no recent reports" } else { state }}
                                        </a>
                                    }
                                },
                            )
                    }}
                </div>
                <form id="run-filters" class="filters" action="/">
                    <fieldset>
                        <legend>"Find runs"</legend>
                        {|output| fields(output, query, &[("project", "Project"), ("name", "Search runs")])}
                        <label>
                            "State "
                            <select name="state">
                                {|output| options(
                                    output,
                                    &[
                                        "",
                                        "running",
                                        "slow",
                                        "stopped",
                                        "completed",
                                        "failed",
                                        "cancelled",
                                        "recorded",
                                        "no-data",
                                    ],
                                    query.get("state"),
                                )}
                            </select>
                        </label>
                    </fieldset>
                    <details>
                        <summary>"More filters and sorting · "{active_filters}" active"</summary>
                        <div class="columns">
                            <fieldset>
                                <legend>"Metadata"</legend>
                                {|output| fields(output, query, &[("trainer", "Trainer"), ("tag", "Tag")])}
                            </fieldset>
                            <fieldset>
                                <legend>"Metric range"</legend>
                                {|output| fields(
                                    output,
                                    query,
                                    &[("metric", "Metric"), ("minimum", "Minimum"), ("maximum", "Maximum")],
                                )}
                            </fieldset>
                            <fieldset>
                                <legend>"Order"</legend>
                                <label>
                                    "Sort "
                                    <select name="sort">
                                        {|output| options(
                                            output,
                                            &["active", "recent", "name", "metric"],
                                            if query.get("sort").is_empty() {
                                                "recent"
                                            } else {
                                                query.get("sort")
                                            },
                                        )}
                                    </select>
                                </label>
                                <label>
                                    "Descending "
                                    <input
                                        type="checkbox"
                                        name="descending"
                                        value="1"
                                        checked=(query.get("descending") == "1")
                                    />
                                </label>
                            </fieldset>
                        </div>
                    </details>
                    <div class="actions">
                        <button>"Filter"</button>
                        <a href="/">"Reset"</a>
                    </div>
                </form>
            }?;
            if capped {
                html! { output, <p class="warn">"The combined run index reached its 4096-run limit."</p> }?;
            }
            if runs.is_empty() {
                html! { output,
                    <p class="empty">
                        {if no_reports {
                            "No run reports discovered. Check the configured directories under Inputs."
                        } else {
                            "No runs match these filters. Clear a filter or reset to see all runs."
                        }}
                    </p>
                }?;
            }
            html! { output,
                <p>
                    {runs.len()}" matching runs · page "{page + 1}" of "{page_max + 1}
                    " · select up to eight."
                </p>
                <form id="run-selection" action="/compare">
                    <div class="actions selection-toolbar">
                        <span id="selection-count" role="status">
                            "Select runs to compare"
                        </span>
                        <button>"Compare selected"</button>
                        <button formaction="/export">"CSV"</button>
                        <button formaction="/report">"Static report"</button>
                        <button id="selection-clear" type="button" hidden>
                            "Clear selection"
                        </button>
                    </div>
                    <div class="table-scroll" role="region" aria-label="Runs table" tabindex="0">
                        <table>
                            <tr>
                                <th>"Select"</th>
                                <th>"Project / run"</th>
                                <th>"State"</th>
                                <th>"Step"</th>
                                <th>"Latest metrics"</th>
                                <th>"Last report"</th>
                            </tr>
                            {|output| {
                                for entry in runs.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
                                    let id = entry.id();
                                    let mut values = entry
                                        .last
                                        .as_ref()
                                        .map(|record| {
                                            record
                                                .0
                                                .iter()
                                                .filter(|(key, _)| {
                                                    !matches!(key.as_str(), "step" | "time" | "seconds")
                                                })
                                                .take(4)
                                                .map(|(key, value)| format!("{key}: {}", number(*value)))
                                                .collect::<Vec<_>>()
                                        })
                                        .unwrap_or_default();
                                    if !metric.is_empty() {
                                        values = entry
                                            .metric(metric)
                                            .map(|value| vec![format!("{metric}: {}", number(value))])
                                            .unwrap_or_default();
                                    } else if values.is_empty() {
                                        if let Ok(summary) = &entry.summary {
                                            values = summary
                                                .iter()
                                                .take(4)
                                                .map(|metric| {
                                                    format!(
                                                        "{}: {} {}",
                                                        metric.name,
                                                        number(metric.value),
                                                        metric.unit,
                                                    )
                                                })
                                                .collect();
                                        }
                                    }
                                    html! { output,
                                        <tr>
                                            <td>
                                                <input
                                                    aria-label=format!("Select {id}")
                                                    type="checkbox"
                                                    name="run"
                                                    value=&id
                                                />
                                            </td>
                                            <td class="run-name">
                                                <a href=format!("/run?run={}", encode(&id))>{id}</a>
                                                {|output| {
                                                    if let Some(tasks) = task_links.get(&id) {
                                                        for (source, task_id) in tasks {
                                                            html! { output,
                                                                <div class="queue-task">
                                                                    <a href=format!(
                                                                        "/jobs?completed=1#task-{}-{}",
                                                                        encode(source),
                                                                        encode(task_id),
                                                                    )>"Pueue #"{task_id}</a>
                                                                </div>
                                                            }?;
                                                        }
                                                    } else if let Some(task_id) = entry.task_id() {
                                                        html! { output,
                                                            <div class="queue-task">"Pueue #"{task_id}</div>
                                                        }?;
                                                    }
                                                    notice(output, &entry.warnings)
                                                }}
                                            </td>
                                            <td>
                                                <span class="state" data-state=entry.state().as_str()>
                                                    {if entry.state() == RunState::Slow {
                                                        "No recent reports"
                                                    } else {
                                                        entry.state().as_str()
                                                    }}
                                                </span>
                                            </td>
                                            <td>
                                                {entry
                                                    .last
                                                    .as_ref()
                                                    .and_then(|record| record.get("step"))
                                                    .map(number)
                                                    .unwrap_or_default()}
                                            </td>
                                            <td>{|output| metric_lines(output, &values)}</td>
                                            <td>{|output| html::age(output, entry.stage.modified)}</td>

                                        </tr>
                                    }?;
                                }
                                Ok(())
                            }}
                        </table>
                    </div>
                </form>
            }?;
            if page > 0 {
                html! { output,
                    <a href=query.link("/", "page", &(page - 1).to_string())>"Previous"</a>
                    " "
                }?;
            }
            if page < page_max {
                html! { output, <a href=query.link("/", "page", &(page + 1).to_string())>"Next"</a> }?;
            }
            Ok(())
        },
        false,
    )
}

fn fields(output: &mut Output, query: &Query, fields: &[(&str, &str)]) -> RenderResult {
    output.each(fields, |output, (key, label)| {
        html! { output, <label>{label}<input name=key value=query.get(key) /></label> }
    })
}

fn options(output: &mut Output, values: &[&str], current: &str) -> RenderResult {
    output.each(values, |output, value| {
        html! { output,
            <option value=value selected=(current == *value)>
                {if value.is_empty() { "All" } else { value }}
            </option>
        }
    })
}

fn controls(
    output: &mut Output,
    selected: &[Entry<'_>],
    query: &Query,
    metrics: &[String],
    aggregation: Option<MetricAggregation>,
) -> RenderResult {
    html! { output,
        <form id="chart-filters" class="filters">
            {|output| {
                output
                    .each(
                        selected,
                        |output, entry| html! { output, <input type="hidden" name="run" value=entry.id() /> },
                    )
            }}
            <fieldset>
                <legend>"Metrics and charts"</legend>
                <label>
                    "Filter metrics "
                    <input
                        name="metric"
                        list="metrics"
                        autocomplete="off"
                        placeholder="All metrics (e.g. loss)"
                        value=query.get("metric")
                    />
                </label>
                <datalist id="metrics">
                    {|output| {
                        output
                            .each(metrics, |output, metric| html! { output, <option value=metric></option> })
                    }}
                </datalist>
                <label>
                    "Axis "
                    <select name="axis">
                        {|output| options(
                            output,
                            &["auto", "tokens", "flops", "step", "seconds", "time"],
                            if query.get("axis").is_empty() { "auto" } else { query.get("axis") },
                        )}
                    </select>
                </label>
                {|output| {
                    if let Some(aggregation) = aggregation {
                        html! { output,
                            <label>
                                "Value summary "
                                <select name="aggregate">
                                    {|output| {
                                        for (kind, label) in [
                                            (MetricAggregation::Latest, "Latest"),
                                            (MetricAggregation::Mean, "Mean"),
                                            (MetricAggregation::Median, "Median"),
                                            (MetricAggregation::Min, "Min"),
                                            (MetricAggregation::Max, "Max"),
                                        ] {
                                            html! { output,
                                                <option value=kind.as_str() selected=(kind == aggregation)>
                                                    {label}
                                                </option>
                                            }?;
                                        }
                                        Ok(())
                                    }}
                                </select>
                            </label>
                        }
                    } else {
                        Ok(())
                    }
                }}
                <button>"Apply"</button>
            </fieldset>
            <details id="chart-options">
                <summary>"Smoothing, range and seed groups"</summary>
                <fieldset>
                    <label>
                        "Smoothing method " <select name="smoothing">
                            <option
                                value="auto"
                                selected=((query.get("smoothing").is_empty()
                                    && query.get("smooth").is_empty()) || query.get("smoothing") == "auto")
                            >
                                "Auto (loss Gaussian)"
                            </option>
                            <option value="gaussian" selected=(query.get("smoothing") == "gaussian")>
                                "Gaussian"
                            </option>
                            <option value="raw" selected=(query.get("smoothing") == "raw")>
                                "Raw"
                            </option>
                            <option
                                value="mean"
                                selected=(query.get("smoothing") == "mean"
                                    || (query.get("smoothing").is_empty() && !query.get("smooth").is_empty()))
                            >
                                "Trailing mean"
                            </option>
                            <option value="ema" selected=(query.get("smoothing") == "ema")>
                                "EMA"
                            </option>
                        </select>
                    </label>
                    <label>
                        "Mean / EMA span "
                        <input
                            type="number"
                            name="smooth"
                            min="1"
                            max="100"
                            value={query.get("smooth").parse::<u32>().unwrap_or(20).clamp(1, 100)}
                        />
                    </label>
                    <label>
                        "Seed groups "
                        <input type="checkbox" name="group" value="1" checked=(query.get("group") == "1") />
                    </label>
                    <label>
                        "Gaussian bandwidth (% of shared axis range) "
                        <input
                            type="number"
                            name="bandwidth"
                            min="0.5"
                            max="10"
                            step="0.1"
                            value={query.get("bandwidth").parse::<f64>().unwrap_or(2.0)}
                        />
                    </label>
                    <label>
                        "From "<input type="number" step="any" name="start" value=query.get("start") />
                    </label>
                    <label>"To "<input type="number" step="any" name="end" value=query.get("end") /></label>
                </fieldset>
                <p class="dim">
                    "Auto shows loss against tokens seen when all runs have token history, with raw loss under a centered Gaussian overlay; other metrics use steps and raw values. Gaussian bandwidth is its standard deviation in axis distance (default 2% of the selected runs' shared full retained axis range), not observation count. Both sides contribute; new data can revise the curve, and its relative bandwidth grows with the shared axis range. EMA uses alpha = 2 / (span + 1), seeded from the first observation; span 1 is raw. Tokens sums reported train/real_tokens windows only with complete history, or uses cumulative train/tokens_seen. FLOPs requires cumulative train/flops_total; it is not estimated. Table summaries are unchanged."
                </p>
            </details>
            <p class="dim">
                "Search any reported metric by substring, ignoring case. Each page shows up to 32 metrics; Next metrics shows the rest. Loss, step timing and throughput appear first, then other metrics alphabetically within each group."
            </p>
        </form>
    }
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

#[derive(Clone, Copy)]
enum Axis {
    Step,
    Seconds,
    Time,
    Tokens,
    FloatingPointOperations,
}

impl Axis {
    fn field(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Seconds => "seconds",
            Self::Time => "time",
            Self::Tokens => data::TOKENS_AXIS,
            Self::FloatingPointOperations => "train/flops_total",
        }
    }
    fn is_budget(self) -> bool {
        matches!(self, Self::Tokens | Self::FloatingPointOperations)
    }
    fn label(self) -> &'static str {
        match self {
            Self::Tokens => "tokens seen",
            Self::FloatingPointOperations => "FLOPs",
            _ => self.field(),
        }
    }
}

fn axis(query: &Query, metric: &str, logs: &[Vec<Record>]) -> Axis {
    match query.get("axis") {
        "seconds" => Axis::Seconds,
        "time" => Axis::Time,
        "step" => Axis::Step,
        "tokens" => Axis::Tokens,
        "flops" => Axis::FloatingPointOperations,
        _ => {
            if data::metric_order(metric).0 != data::MetricKind::Loss {
                return Axis::Step;
            }
            if logs
                .iter()
                .all(|log| log.iter().any(|record| record.get(data::TOKENS_AXIS).is_some()))
            {
                return Axis::Tokens;
            }
            Axis::Step
        }
    }
}

fn smoothing(query: &Query, metric: &str) -> Smoothing {
    let value = if query.get("smoothing").is_empty() && !query.get("smooth").is_empty() {
        "mean"
    } else {
        query.get("smoothing")
    };
    Smoothing::parse(value, data::metric_order(metric).0).expect("validated smoothing method")
}

fn training_range(logs: &[Vec<Record>], metric: &str, axis: Axis) -> Option<(f64, f64)> {
    logs.iter()
        .flat_map(|log| curve(log, metric, &Query(Vec::new()), axis, Smoothing::Raw))
        .filter(|(x, _)| x.is_finite())
        .fold(None, |range, (x, _)| {
            Some(range.map_or((x, x), |(low, high): (f64, f64)| (low.min(x), high.max(x))))
        })
}

fn training_series(
    label: String,
    log: &[Record],
    metric: &str,
    query: &Query,
    axis: Axis,
    range: Option<(f64, f64)>,
) -> Series {
    let method = match smoothing(query, metric) {
        Smoothing::Gaussian(_) => Smoothing::Gaussian(range),
        method => method,
    };
    let overlay = match method {
        Smoothing::Gaussian(_) => Some("Gaussian"),
        Smoothing::ExponentialMovingAverage if query.get("smooth").parse::<u32>().unwrap_or(20) > 1 => Some("EMA"),
        _ => None,
    };
    let raw = overlay.map(|name| (name, curve(log, metric, query, axis, Smoothing::Raw)));
    Series {
        label,
        points: curve(log, metric, query, axis, method),
        raw,
    }
}

fn training_chart(output: &mut Output, metric: &str, axis: Axis, series: &[Series]) -> RenderResult {
    if axis.is_budget() {
        for line in series {
            if line.points.is_empty() {
                html! { output,
                    <p class="dim">
                        "No "{metric}" observations paired with recorded "{axis.label()}" for "{line.label}
                        ". Budget counters are not estimated or interpolated."
                    </p>
                }?;
            }
        }
    }
    html::chart(output, metric, axis.label(), series)
}

// Linear binning bounds the work independently of reporting density. The kernel
// uses axis distance, is centered, and truncates at four standard deviations.
fn gaussian_curve(points: &[(f64, f64)], bandwidth: f64, range: Option<(f64, f64)>) -> Vec<(f64, f64)> {
    const BINS: usize = 1024;
    debug_assert!(bandwidth.is_finite() && (0.005..=0.1).contains(&bandwidth));
    if points.is_empty() {
        return Vec::new();
    }
    let (low, high) = range.unwrap_or_else(|| {
        (
            points.iter().map(|point| point.0).fold(f64::INFINITY, f64::min),
            points.iter().map(|point| point.0).fold(f64::NEG_INFINITY, f64::max),
        )
    });
    debug_assert!(low.is_finite() && high.is_finite() && low <= high);
    let span = high / 2.0 - low / 2.0;
    let scale = points.iter().map(|point| point.1.abs()).fold(0.0, f64::max);
    let low_y = points.iter().map(|point| point.1).fold(f64::INFINITY, f64::min);
    let high_y = points.iter().map(|point| point.1).fold(f64::NEG_INFINITY, f64::max);
    if span <= 0.0 || scale == 0.0 {
        return points.to_vec();
    }
    let position = |x: f64| ((x / 2.0 - low / 2.0) / span * (BINS - 1) as f64).clamp(0.0, (BINS - 1) as f64);
    let mut counts = [0.0; BINS];
    let mut sums = [0.0; BINS];
    for &(x, y) in points {
        let offset = position(x);
        let index = offset.floor() as usize;
        let fraction = offset - index as f64;
        for (index, weight) in [(index, 1.0 - fraction), ((index + 1).min(BINS - 1), fraction)] {
            counts[index] += weight;
            sums[index] += weight * (y / scale);
        }
    }
    let sigma = bandwidth * (BINS - 1) as f64;
    let radius = ((4.0 * sigma).ceil() as usize).min(BINS - 1);
    let kernel: Vec<f64> = (0..=radius)
        .map(|distance| (-0.5 * (distance as f64 / sigma).powi(2)).exp())
        .collect();
    let mut weights = [0.0; BINS];
    let mut values = [0.0; BINS];
    for index in 0..BINS {
        for neighbor in index.saturating_sub(radius)..=(index + radius).min(BINS - 1) {
            let weight = kernel[index.abs_diff(neighbor)];
            weights[index] += weight * counts[neighbor];
            values[index] += weight * sums[neighbor];
        }
    }
    points
        .iter()
        .map(|&(x, y)| {
            let offset = position(x);
            let index = offset.floor() as usize;
            let next = (index + 1).min(BINS - 1);
            let fraction = offset - index as f64;
            let weight = weights[index] * (1.0 - fraction) + weights[next] * fraction;
            debug_assert!(weight > 0.0);
            let value = (values[index] * (1.0 - fraction) + values[next] * fraction) / weight;
            let smoothed = (value.clamp(-1.0, 1.0) * scale).clamp(low_y, high_y);
            (x, if smoothed.is_finite() { smoothed } else { y })
        })
        .collect()
}

fn curve(records: &[Record], metric: &str, query: &Query, axis: Axis, smoothing: Smoothing) -> Vec<(f64, f64)> {
    let window = query.get("smooth").parse::<usize>().unwrap_or(20).clamp(1, 100);
    let start = query.get("start").parse::<f64>().unwrap_or(f64::NEG_INFINITY);
    let end = query.get("end").parse::<f64>().unwrap_or(f64::INFINITY);
    let alpha = 2.0 / (window as f64 + 1.0);
    let mut ema: Option<f64> = None;
    let mut history = std::collections::VecDeque::new();
    let mut points = Vec::new();
    let mut previous_x = None;
    for record in records {
        let x = record.get(axis.field());
        if let Some(value) = x.filter(|_| axis.is_budget()) {
            if !value.is_finite() {
                break;
            }
            if value < previous_x.unwrap_or(0.0) {
                break;
            }
            previous_x = Some(value);
        }
        let Some(y) = record.get(metric) else { continue };
        let y = match smoothing {
            Smoothing::Raw | Smoothing::Gaussian(_) => y,
            Smoothing::Mean => {
                history.push_back(y);
                if history.len() > window {
                    history.pop_front();
                }
                history.iter().map(|value| value / history.len() as f64).sum::<f64>()
            }
            Smoothing::ExponentialMovingAverage => {
                ema = Some(ema.map(|previous| previous * (1.0 - alpha) + y * alpha).unwrap_or(y))
                    .filter(|value| value.is_finite());
                let Some(value) = ema else { continue };
                value
            }
        };
        let Some(x) = x else { continue };
        if y.is_finite() {
            points.push((x, y));
        }
    }
    if let Smoothing::Gaussian(range) = smoothing {
        let bandwidth = query.get("bandwidth").parse::<f64>().unwrap_or(2.0) / 100.0;
        points = gaussian_curve(&points, bandwidth, range);
    }
    points.retain(|(x, _)| *x >= start && *x <= end);
    points
}

#[derive(Clone, Copy)]
enum Smoothing {
    Gaussian(Option<(f64, f64)>),
    Raw,
    Mean,
    ExponentialMovingAverage,
}

impl Smoothing {
    fn parse(value: &str, kind: data::MetricKind) -> Option<Self> {
        match value {
            "" | "auto" => Some(if kind == data::MetricKind::Loss {
                Self::Gaussian(None)
            } else {
                Self::Raw
            }),
            "gaussian" => Some(Self::Gaussian(None)),
            "raw" => Some(Self::Raw),
            "mean" => Some(Self::Mean),
            "ema" => Some(Self::ExponentialMovingAverage),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricAggregation {
    Latest,
    Mean,
    Median,
    Min,
    Max,
}

impl MetricAggregation {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "" | "latest" => Some(Self::Latest),
            "mean" => Some(Self::Mean),
            "median" => Some(Self::Median),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Mean => "mean",
            Self::Median => "median",
            Self::Min => "min",
            Self::Max => "max",
        }
    }
}

fn metric_value(log: &[Record], key: &str, aggregation: MetricAggregation) -> Option<f64> {
    let values = log.iter().filter_map(|record| record.get(key));
    match aggregation {
        MetricAggregation::Latest => log.iter().rev().find_map(|record| record.get(key)),
        MetricAggregation::Min => values.reduce(f64::min),
        MetricAggregation::Max => values.reduce(f64::max),
        MetricAggregation::Median => median(values),
        MetricAggregation::Mean => {
            let count = values.clone().count();
            if count == 0 {
                return None;
            }
            let mean = values.map(|value| value / count as f64).sum::<f64>();
            mean.is_finite().then_some(mean)
        }
    }
}

fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<f64> = values.filter(|value| value.is_finite()).collect();
    if values.is_empty() {
        return None;
    }
    let count = values.len();
    debug_assert!(count <= data::RECORDS_MAX);
    let (lower, middle, _) = values.select_nth_unstable_by(count / 2, f64::total_cmp);
    if count % 2 == 1 {
        return Some(*middle);
    }
    let lower = lower
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .expect("even nonempty sample has a lower half");
    Some(lower.midpoint(*middle))
}

struct MetricPage {
    names: Vec<String>,
    next: Option<String>,
}

fn metric_page(logs: &[Vec<Record>], selected: &str, after: &str) -> MetricPage {
    let selected = selected.to_lowercase();
    let mut names = BTreeSet::new();
    for log in logs {
        for record in log.iter().rev() {
            for (key, _) in &record.0 {
                let matches_filter = selected.is_empty() || key.to_lowercase().contains(&selected);
                if !matches_filter {
                    continue;
                }
                if !matches!(key.as_str(), "step" | "time" | "seconds" | data::TOKENS_AXIS) {
                    if !after.is_empty() && data::metric_order(key) <= data::metric_order(after) {
                        continue;
                    }
                    names.insert(data::metric_order(key));
                    if names.len() > 33 {
                        names.pop_last();
                    }
                }
            }
        }
    }
    let has_next = names.len() > 32;
    if has_next {
        names.pop_last();
    }
    let names: Vec<String> = names.into_iter().map(|(_, name)| name.to_owned()).collect();
    let next = if has_next { names.last().cloned() } else { None };
    MetricPage { names, next }
}

fn metric_navigation(output: &mut Output, path: &str, query: &Query, page: &MetricPage) -> RenderResult {
    html! { output,
        <div class="actions metric-navigation">
            <span>{page.names.len()}" matching metrics shown"</span>
            {|output| {
                if !query.get("metric_after").is_empty() {
                    html! { output, <a href=query.link(path, "metric_after", "")>"First metrics"</a> }?;
                }
                if let Some(next) = &page.next {
                    html! { output, <a href=query.link(path, "metric_after", next)>"Next metrics"</a> }?;
                }
                Ok(())
            }}
        </div>
    }
}

fn metadata_table(output: &mut Output, selected: &[Entry<'_>]) -> RenderResult {
    let mut keys = BTreeSet::new();
    for entry in selected {
        for (key, _) in &entry.metadata {
            if keys.len() < 128 {
                keys.insert(key.as_str());
            }
        }
    }
    if keys.is_empty() {
        return html! { output, <p>"No run metadata."</p> };
    }
    html! { output,
        <table>
            <tr>
                <th>"Metadata"</th>
                {|output| output.each(selected, |output, entry| html! { output, <th>{entry.id()}</th> })}
            </tr>
            {|output| {
                for key in keys {
                    let values: Vec<Option<&Scalar>> = selected
                        .iter()
                        .map(|entry| entry.value(key))
                        .collect();
                    let differs = values.iter().any(|value| *value != values[0]);
                    html! { output,
                        <tr>
                            <td>{key}{if differs { " (differs)" } else { "" }}</td>
                            {|output| {
                                output
                                    .each(
                                        &values,
                                        |output, value| {
                                            let value = match value {
                                                None => "(missing)".into(),
                                                Some(Scalar::Text(value)) => format!("{value:?}"),
                                                Some(value) => inputs::scalar(value),
                                            };
                                            html! { output, <td>{value}</td> }
                                        },
                                    )
                            }}
                        </tr>
                    }?;
                }
                Ok(())
            }}
        </table>
    }
}

fn summary_table(output: &mut Output, selected: &[Entry<'_>], summaries: &[Vec<Summary>]) -> RenderResult {
    let names: BTreeSet<&str> = summaries
        .iter()
        .flat_map(|metrics| metrics.iter().map(|metric| metric.name.as_str()))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    html! { output,
        <h2 id="benchmarks">"Benchmark summary"</h2>
        <table>
            <tr>
                <th>"Metric / unit"</th>
                {|output| output.each(selected, |output, entry| html! { output, <th>{entry.id()}</th> })}
            </tr>
            {|output| {
                for name in names {
                    let values: Vec<Option<&Summary>> = summaries
                        .iter()
                        .map(|metrics| metrics.iter().find(|metric| metric.name == name))
                        .collect();
                    let first = *values.iter().flatten().next().expect("metric name came from a summary");
                    let baseline = values[0];
                    html! { output,
                        <tr>
                            <td>{name}" ("{first.unit}", "{first.direction.as_str()}")"</td>
                            {|output| {
                                output
                                    .each(
                                        &values,
                                        |output, metric| {
                                            if let Some(metric) = metric {
                                                let compatible = metric.unit == first.unit
                                                    && metric.direction == first.direction;
                                                let delta = baseline
                                                    .filter(|base| {
                                                        compatible && base.unit == metric.unit
                                                            && base.direction == metric.direction
                                                    })
                                                    .map(|base| metric.value - base.value);
                                                let class = match delta {
                                                    Some(
                                                        value,
                                                    ) if metric.direction == inputs::Direction::Higher
                                                        && value > 0.0
                                                        || metric.direction == inputs::Direction::Lower
                                                            && value < 0.0 => "good",
                                                    Some(
                                                        value,
                                                    ) if metric.direction == inputs::Direction::Higher
                                                        && value < 0.0
                                                        || metric.direction == inputs::Direction::Lower
                                                            && value > 0.0 => "warn",
                                                    _ => "",
                                                };
                                                let delta = delta
                                                    .map(|value| format!(" (Δ {})", number(value)))
                                                    .unwrap_or_default();
                                                html! { output,
                                                    <td class=class>
                                                        {number(metric.value)}{delta}" "
                                                        {if compatible {
                                                            ""
                                                        } else {
                                                            "incompatible unit/direction"
                                                        }}
                                                    </td>
                                                }
                                            } else {
                                                html! { output, <td>"–"</td> }
                                            }
                                        },
                                    )
                            }}
                        </tr>
                    }?;
                }
                Ok(())
            }}
        </table>
    }
}

fn summaries(selected: &[Entry<'_>]) -> (Vec<Vec<Summary>>, Vec<String>) {
    let mut warnings = Vec::new();
    let metrics = selected
        .iter()
        .map(|entry| match benchmark_metrics(entry) {
            Ok(metrics) => metrics,
            Err(error) => {
                warnings.push(format!("{} summary: {error}", entry.id()));
                Vec::new()
            }
        })
        .collect();
    (metrics, warnings)
}

fn benchmark_metrics(entry: &Entry<'_>) -> Result<Vec<Summary>, String> {
    let mut metrics = entry.summary.as_ref().map_err(Clone::clone)?.clone();
    for suffix in data::VARIANTS {
        let system = format!("{}{suffix}", entry.stage.name);
        let variant = if suffix.is_empty() { "base" } else { &suffix[1..] };
        for score in data::panel_scores(&entry.project.root, &system) {
            for (name, value, unit, direction) in [
                ("accuracy", score.system.accuracy, "fraction", inputs::Direction::Higher),
                ("nll", score.system.nll, "loss", inputs::Direction::Lower),
                ("brier", score.system.brier, "score", inputs::Direction::Lower),
                (
                    "reference/accuracy",
                    score.jev.accuracy,
                    "fraction",
                    inputs::Direction::Higher,
                ),
                ("reference/nll", score.jev.nll, "loss", inputs::Direction::Lower),
                ("reference/brier", score.jev.brier, "score", inputs::Direction::Lower),
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
                    direction,
                });
            }
        }
        for score in data::jevbench_scores(&entry.project.root, &system) {
            for (name, value, unit, direction) in [
                ("accuracy", score.accuracy, "fraction", inputs::Direction::Higher),
                ("ece", score.ece, "fraction", inputs::Direction::Lower),
                ("brier", score.brier, "score", inputs::Direction::Lower),
                ("coverage", score.coverage, "fraction", inputs::Direction::Higher),
                (
                    "complete",
                    if score.complete { 1.0 } else { 0.0 },
                    "boolean",
                    inputs::Direction::None,
                ),
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
                    direction,
                });
            }
        }
    }
    Ok(metrics)
}

fn comparison(selected: &[Entry<'_>], query: &Query, offline: bool) -> RenderResult<String> {
    let logs: Vec<Vec<Record>> = selected.iter().map(records).collect();
    let aggregation = MetricAggregation::parse(query.get("aggregate")).expect("validated metric aggregation");
    let (benchmarks, warnings) = summaries(selected);
    let has_benchmarks = benchmarks.iter().any(|metrics| !metrics.is_empty()) || !warnings.is_empty();
    html::page(
        if offline { "Run report" } else { "Compare runs" },
        |output| {
            if offline {
                html! { output,
                    <p>
                        "Selected run metrics and summaries. Metadata tables, queue data, device logs and artifacts are excluded."
                    </p>
                }?;
            } else {
                for entry in selected {
                    notice(output, &entry.warnings)?;
                }
            }
            html! { output,
                <h2>"Training performance"</h2>
                <p>
                    "Value summary: "{aggregation.as_str()}
                    " for step durations. Throughput always shows the median of retained valid nonnegative samples, not a time-weighted or whole-run rate. For an even sample count, median averages the two middle values. Step durations are converted from seconds to milliseconds. Not reported means no valid value was found in retained history."
                </p>
                <div class="table-scroll" role="region" aria-label="Training performance" tabindex="0">
                    <table id="training-performance">
                        <tr>
                            <th>"Run"</th>
                            <th>"Mean ms/step"</th>
                            <th>"Max ms/step"</th>
                            <th>"Median tokens/sec"</th>
                        </tr>
                        {|output| {
                            for (entry, log) in selected.iter().zip(&logs) {
                                html! { output,
                                    <tr>
                                        <td class="run-name">{entry.id()}</td>
                                        {|output| {
                                            for (value, scale) in [
                                                (
                                                    metric_value(log, "train/step_seconds_mean", aggregation),
                                                    1000.0,
                                                ),
                                                (
                                                    metric_value(log, "train/step_seconds_max", aggregation),
                                                    1000.0,
                                                ),
                                                (
                                                    median(
                                                        log
                                                            .iter()
                                                            .filter_map(|record| record.get("train/tokens_per_second"))
                                                            .filter(|value| *value >= 0.0),
                                                    ),
                                                    1.0,
                                                ),
                                            ] {
                                                let value = value
                                                    .filter(|value| *value >= 0.0)
                                                    .map(|value| value * scale)
                                                    .filter(|value| value.is_finite())
                                                    .map(number)
                                                    .unwrap_or_else(|| "Not reported".into());
                                                html! { output, <td>{value}</td> }?;
                                            }
                                            Ok(())
                                        }}
                                    </tr>
                                }?;
                            }
                            Ok(())
                        }}
                    </table>
                </div>
                <table>
                    <tr>
                        <th>"Run"</th>
                        <th>"State"</th>
                        <th>"Step"</th>
                        <th>"Latest metrics"</th>
                        <th>"Benchmarks"</th>
                        <th>"Trainer time"</th>
                    </tr>
                    {|output| {
                        for (entry, log) in selected.iter().zip(&logs) {
                            let latest: Vec<String> = data::latest_values(log)
                                .as_ref()
                                .map(|record| {
                                    record
                                        .0
                                        .iter()
                                        .filter(|(key, _)| {
                                            !matches!(key.as_str(), "step" | "time" | "seconds")
                                                && (query.get("metric").is_empty()
                                                    || key == query.get("metric"))
                                        })
                                        .take(2)
                                        .map(|(name, value)| format!("{name}: {}", number(*value)))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let final_metrics: Vec<String> = entry
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
                                                metric.name,
                                                number(metric.value),
                                                metric.unit,
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            html! { output,
                                <tr>
                                    <td>{entry.id()}</td>
                                    <td>{entry.state().as_str()}</td>
                                    <td>
                                        {log
                                            .last()
                                            .and_then(|record| record.get("step"))
                                            .map(number)
                                            .unwrap_or_else(|| "–".into())}
                                    </td>
                                    <td>{|output| metric_lines(output, &latest)}</td>
                                    <td>{|output| metric_lines(output, &final_metrics)}</td>
                                    <td>{training_time(entry, log.last())}</td>
                                </tr>
                            }?;
                        }
                        Ok(())
                    }}
                </table>
            }?;
            if !offline {
                html! { output,
                    <div class="actions">
                        {|output| {
                            if has_benchmarks {
                                html! { output, <a href="#benchmarks">"Benchmarks"</a> }
                            } else {
                                Ok(())
                            }
                        }} <a href="#comparison-metadata">"Metadata differences"</a>
                    </div>
                }?;
                controls(
                    output,
                    selected,
                    query,
                    &metric_page(&logs, "", "").names,
                    Some(aggregation),
                )?;
            }
            let page = metric_page(&logs, query.get("metric"), query.get("metric_after"));
            if !offline {
                metric_navigation(output, "/compare", query, &page)?;
            }
            html! { output,
                <h2>"Metric values"</h2>
                <p class="dim">
                    "Value summary: "{aggregation.as_str()}
                    " in each metric's logged units. Search above or use Next metrics to browse every reported numeric metric in retained history. Missing samples are not filled or counted as zero."
                </p>
                <div class="table-scroll" role="region" aria-label="Metric values" tabindex="0">
                    <table id="metric-values">
                        <tr>
                            <th>"Metric"</th>
                            {|output| {
                                output.each(selected, |output, entry| html! { output, <th>{entry.id()}</th> })
                            }}
                        </tr>
                        {|output| {
                            for name in &page.names {
                                html! { output,
                                    <tr>
                                        <th scope="row" class="metric-name">
                                            {name}
                                        </th>
                                        {|output| {
                                            for log in &logs {
                                                let value = metric_value(log, name, aggregation)
                                                    .map(number)
                                                    .unwrap_or_else(|| "Not reported".into());
                                                html! { output, <td>{value}</td> }?;
                                            }
                                            Ok(())
                                        }}
                                    </tr>
                                }?;
                            }
                            Ok(())
                        }}
                    </table>
                </div>
            }?;
            let no_matching_metric = page.names.is_empty() && !query.get("metric").is_empty();
            if no_matching_metric {
                html! { output,
                    <p class="empty">
                        "No matching metric observations. Clear the metric filter or choose a logged metric."
                    </p>
                }?;
            }
            for metric in page.names {
                let axis = axis(query, &metric, &logs);
                let range = match smoothing(query, &metric) {
                    Smoothing::Gaussian(_) => training_range(&logs, &metric, axis),
                    _ => None,
                };
                let series: Vec<Series> = selected
                    .iter()
                    .zip(&logs)
                    .map(|(entry, log)| training_series(entry.id(), log, &metric, query, axis, range))
                    .collect();
                training_chart(output, &metric, axis, &series)?;
                if query.get("group") == "1" {
                    seed_groups(output, selected, &series, axis.label())?;
                }
            }
            if !offline {
                html! { output,
                    <details id="comparison-metadata">
                        <summary>"Metadata differences"</summary>
                        {|output| metadata_table(output, selected)}
                    </details>
                }?;
            }
            notice(output, &warnings)?;
            summary_table(output, selected, &benchmarks)?;
            if !offline {
                let selection = query
                    .0
                    .iter()
                    .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
                    .collect::<Vec<_>>()
                    .join("&");
                html! { output,
                    <div class="actions">
                        <a href=format!("/export?{selection}")>"Download CSV"</a>
                        <a href=format!("/report?{selection}")>"Download static report"</a>
                    </div>
                }?;
            }
            Ok(())
        },
        offline,
    )
}

fn seed_groups(output: &mut Output, selected: &[Entry<'_>], series: &[Series], axis: &str) -> RenderResult {
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
    let mut curves = Vec::new();
    html! { output,
        <table>
            <tr>
                <th>"Seed group"</th>
                <th>"Distinct seeds"</th>
                <th>"Last shared x"</th>
                <th>"Mean"</th>
                <th>"Sample standard deviation"</th>
                <th>"Standard error"</th>
            </tr>
            {|output| {
                for (group, indices) in groups {
                    let mut seeds = BTreeSet::new();
                    if indices.iter().any(|index| !seeds.insert(selected[*index].field("seed"))) {
                        html! { output,
                            <tr>
                                <td>{group}</td>
                                <td colspan="5">"Duplicate seed; aggregation withheld"</td>
                            </tr>
                        }?;
                        continue;
                    }
                    if indices.len() < 2 {
                        continue;
                    }
                    let baseline = &selected[indices[0]];
                    let comparable: BTreeSet<&str> = indices
                        .iter()
                        .flat_map(|index| selected[*index].metadata.iter().map(|(key, _)| key.as_str()))
                        .filter(|key| {
                            matches!(*key, "revision" | "dataset") || key.starts_with("parameters.")
                        })
                        .collect();
                    if comparable
                        .iter()
                        .any(|key| {
                            indices.iter().any(|index| selected[*index].value(key) != baseline.value(key))
                        })
                    {
                        html! { output,
                            <tr>
                                <td>{group}</td>
                                <td colspan="5">
                                    "Different parameters, revision or dataset; aggregation withheld"
                                </td>
                            </tr>
                        }?;
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
                            a.parse::<f64>().unwrap_or(0.0).total_cmp(&b.parse::<f64>().unwrap_or(0.0))
                        }) else {
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
                    curves
                        .push(Series {
                            label: format!("{group} mean"),
                            points: average,
                            raw: None,
                        });
                    curves
                        .push(Series {
                            label: format!("{group} + sample SD"),
                            points: upper,
                            raw: None,
                        });
                    curves
                        .push(Series {
                            label: format!("{group} − sample SD"),
                            points: lower,
                            raw: None,
                        });
                    html! { output,
                        <tr>
                            <td>{group}</td>
                            <td>{indices.len()}</td>
                            <td>{x}</td>
                            <td>{number(mean)}</td>
                            <td>{number(deviation)}</td>
                            <td>{number(deviation / count.sqrt())}</td>
                        </tr>
                    }?;
                }
                Ok(())
            }}
        </table>
        <p>
            "Aggregation uses distinct declared seeds at exactly shared x values; standard error assumes independent seeds."
        </p>
    }?;
    if !curves.is_empty() {
        html::chart(output, "Seed group mean ± sample standard deviation", axis, &curves)?;
    }
    Ok(())
}

fn run_page(settings: &Settings, entry: &Entry<'_>, query: &Query) -> RenderResult<String> {
    let selected = std::slice::from_ref(entry);
    let log = records(entry);
    let page = metric_page(
        std::slice::from_ref(&log),
        query.get("metric"),
        query.get("metric_after"),
    );
    let names = &page.names;
    let (benchmarks, warnings) = summaries(selected);
    let has_benchmarks = benchmarks.iter().any(|metrics| !metrics.is_empty()) || !warnings.is_empty();
    html::page(
        &entry.id(),
        |output| {
            html! { output,
                <p>
                    <strong>{entry.state().as_str()}</strong>
                    " · step "
                    {log
                        .last()
                        .and_then(|record| record.get("step"))
                        .map(number)
                        .unwrap_or_else(|| "–".into())}
                    " · updated "
                    {|output| html::age(output, entry.stage.modified)}
                    " · trainer time "
                    {training_time(entry, log.last())}
                    "."
                </p>
                <p class="dim">{entry.stage.trainer}" · "{log.len()}" records"</p>
            }?;
            notice(output, &entry.warnings)?;
            if let Some(progress) = &entry.stage.progress {
                html! { output,
                    <p>
                        "Producer PID "{progress.pid}" · "{progress.phase.as_str()}" at step "{progress.step}
                        " · heartbeat "{|output| html::age(output, progress.modified)}
                    </p>
                }?;
            }
            html! { output,
                <dl class="metrics">
                    {|output| {
                        for metric in names.iter().take(6) {
                            let value = log.iter().rev().find_map(|record| record.get(metric));
                            html! { output,
                                <div>
                                    <dt>{metric}</dt>
                                    <dd>{value.map(number).unwrap_or_else(|| "–".into())}</dd>
                                </div>
                            }?;
                        }
                        if let Ok(metrics) = &entry.summary {
                            for metric in metrics.iter().take(2) {
                                html! { output,
                                    <div>
                                        <dt>"Summary: "{metric.name}</dt>
                                        <dd>{number(metric.value)}" "{metric.unit}</dd>
                                    </div>
                                }?;
                            }
                        }
                        Ok(())
                    }}
                </dl>
            }?;
            if names.len() > 6 {
                html! { output, <p class="dim">"Showing six latest metrics; all metrics are available in the charts."</p> }?;
            }
            html! { output,
                <div class="actions">
                    <a href="#metadata">"Metadata"</a>
                    {|output| {
                        if has_benchmarks {
                            html! { output, <a href="#benchmarks">"Benchmarks"</a> }
                        } else {
                            Ok(())
                        }
                    }}
                </div>
            }?;
            if log.is_empty() {
                notice(output, &warnings)?;
                summary_table(output, selected, &benchmarks)?;
            }
            controls(
                output,
                selected,
                query,
                &metric_page(std::slice::from_ref(&log), "", "").names,
                None,
            )?;
            metric_navigation(output, "/run", query, &page)?;
            if names.is_empty() {
                html! { output,
                    <p class="empty">
                        {if query.get("metric").is_empty() {
                            "No chartable metric observations yet."
                        } else {
                            "No matching metric observations. Clear the metric filter or choose a logged metric."
                        }}
                    </p>
                }?;
            }
            for metric in names {
                let axis = axis(query, metric, std::slice::from_ref(&log));
                training_chart(
                    output,
                    metric,
                    axis,
                    &[training_series(entry.id(), &log, metric, query, axis, None)],
                )?;
            }
            if !log.is_empty() {
                notice(output, &warnings)?;
                summary_table(output, selected, &benchmarks)?;
            }
            html! { output,
                <details id="metadata">
                    <summary>"Metadata"</summary>
                    {|output| metadata_table(output, selected)}
                </details>
            }?;
            let events = data::monitor_events(&entry.stage);
            if !events.is_empty() {
                html! { output,
                    <h2>"Monitor events"</h2>
                    <table>
                        <tr>
                            <th>"Event"</th>
                            <th>"Phase"</th>
                            <th>"Step"</th>
                            <th>"Idle seconds"</th>
                            <th>"Exit"</th>
                            <th>"Time"</th>
                        </tr>
                        {|output| {
                            output
                                .each(
                                    &events,
                                    |output, event| {
                                        html! { output,
                                            <tr>
                                                <td>{event.kind}</td>
                                                <td>{event.phase.as_deref().unwrap_or("")}</td>
                                                <td>
                                                    {event
                                                        .step
                                                        .map(|value| value.to_string())
                                                        .unwrap_or_default()}
                                                </td>
                                                <td>
                                                    {event
                                                        .idle_seconds
                                                        .map(|value| value.to_string())
                                                        .unwrap_or_default()}
                                                </td>
                                                <td>
                                                    {event
                                                        .exit_code
                                                        .map(|value| value.to_string())
                                                        .unwrap_or_default()}
                                                </td>
                                                <td>{|output| html::age(output, event.time)}</td>
                                            </tr>
                                        }
                                    },
                                )
                        }}
                    </table>
                }?;
            }
            if settings.artifacts {
                artifact_list(output, entry)?;
            }
            html! { output,
                <p>
                    <a href=format!("/compare?run={}", encode(&entry.id()))>"Compare / export this run"</a>
                </p>
            }
        },
        false,
    )
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
                if matches!(key.as_str(), "step" | "seconds" | "time" | data::TOKENS_AXIS) {
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
                csv_field(metric.direction.as_str())
            );
            if output.len() > RESPONSE_BYTES_MAX {
                return Err((413, "CSV exceeds 32 MiB".into()));
            }
        }
    }
    Ok(output)
}

fn artifact_list(output: &mut Output, entry: &Entry<'_>) -> RenderResult {
    let files = match inputs::artifacts(&entry.project.root, &entry.stage.name) {
        Ok(files) => files,
        Err(error) => return notice(output, &[format!("artifacts: {error}")]),
    };
    html! { output,
        <h2>"Artifacts"</h2>
        <ul>
            {|output| {
                output
                    .each(
                        &files,
                        |output, (name, size)| {
                            let url = format!("/artifact?run={}&file={}", encode(&entry.id()), encode(name));
                            html! { output,
                                <li>
                                    <a href=&url>{name}</a>
                                    " ("
                                    {size}
                                    " bytes)"
                                    {|output| {
                                        if matches!(
                                            name.rsplit('.').next(),
                                            Some("png" | "webp" | "jpg" | "jpeg")
                                        ) && *size <= 4 << 20
                                        {
                                            html! { output,
                                                <details>
                                                    <summary>"Preview"</summary>
                                                    <img class="preview" loading="lazy" src=&url alt=name />
                                                </details>
                                            }?;
                                        } else if matches!(
                                            name.rsplit('.').next(),
                                            Some("txt" | "json" | "csv" | "toml" | "md")
                                        ) && *size <= 64 << 10
                                        {
                                            if let Ok(bytes) = inputs::artifact(
                                                &entry.project.root,
                                                &entry.stage.name,
                                                name,
                                            ) {
                                                html! { output,
                                                    <details>
                                                        <summary>"Preview"</summary>
                                                        <pre>{String::from_utf8_lossy(&bytes).as_ref()}</pre>
                                                    </details>
                                                }?;
                                            }
                                        }
                                        Ok(())
                                    }}
                                </li>
                            }
                        },
                    )
            }}
        </ul>
    }
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

fn queue_snapshots(settings: &Settings, warnings: &mut Vec<String>) -> Vec<(String, String)> {
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
                        warnings.push(format!("{}: queue snapshot unreadable", project.name));
                    }
                }
            }
        }
        Err(error) => warnings.push(error),
    }
    snapshots
}

fn queue_run<'a>(
    settings: &Settings,
    source: &str,
    task: &queue::Task,
    runs: &'a [Entry<'a>],
) -> Option<&'a Entry<'a>> {
    let global = settings.queue.is_some() || settings.pueue.is_some();
    let mut matches = runs.iter().filter(|entry| {
        let local = source == entry.project.name
            || settings.projects.len() == 1
            || Path::new(&task.directory).starts_with(&entry.project.root);
        task.label == entry.id()
            || ((global || source == entry.project.name) && entry.task_id().as_deref() == Some(task.id.as_str()))
            || (local && task.label == entry.stage.name)
    });
    let first = matches.next();
    if matches.next().is_none() {
        first
    } else {
        None
    }
}

struct Completion {
    source: String,
    id: String,
    job: data::Job,
    ended: Option<time::OffsetDateTime>,
}

fn completion_time(value: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
}

fn completion_history(
    settings: &Settings,
    snapshots: &[(String, String)],
    sources: &[(String, PathBuf)],
    group: &str,
) -> Vec<Completion> {
    let shared = settings.pueue.is_some() || settings.queue.is_some();
    let mut history: BTreeMap<(String, String), data::Job> = BTreeMap::new();
    for (source, path) in sources {
        for job in data::jobs(path, 100) {
            let source = if shared { "shared" } else { source };
            let key = (source.to_string(), format!("{:.0}", job.id));
            if let Some(previous) = history.get(&key) {
                if completion_time(&previous.end) >= completion_time(&job.end) {
                    continue;
                }
            }
            history.insert(key, job);
        }
    }
    for (source, snapshot) in snapshots {
        let Ok(tasks) = queue::tasks(snapshot) else { continue };
        for task in tasks {
            let key = (source.clone(), task.id.clone());
            let previous = history.remove(&key);
            if task.state != "Done" {
                continue;
            }
            let mut finished = task.finished.unwrap_or_default();
            let start = completion_time(&finished.start);
            let end = completion_time(&finished.end);
            let faults = previous
                .filter(|job| {
                    if start.is_none() || end.is_none() {
                        return false;
                    }
                    start == completion_time(&job.start) && end == completion_time(&job.end)
                })
                .map(|job| {
                    if let ("Success", "finished-with-faults") = (finished.outcome.as_str(), job.outcome.as_str()) {
                        finished.outcome = job.outcome;
                    }
                    job.faults
                })
                .unwrap_or_default();
            history.insert(
                key,
                data::Job {
                    id: task.id.parse().expect("bounded decimal task id"),
                    label: task.label,
                    group: task.group,
                    outcome: finished.outcome,
                    start: finished.start,
                    end: finished.end,
                    faults,
                },
            );
        }
    }
    let mut history: Vec<Completion> = history
        .into_iter()
        .filter(|(_, job)| group.is_empty() || job.group == group)
        .map(|((source, id), job)| Completion {
            ended: completion_time(&job.end),
            source,
            id,
            job,
        })
        .collect();
    history.sort_by(|a, b| {
        b.ended
            .cmp(&a.ended)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| b.id.parse::<u64>().ok().cmp(&a.id.parse::<u64>().ok()))
            .then_with(|| a.id.cmp(&b.id))
    });
    history.truncate(100);
    history
}

fn jobs(settings: &Settings, query: &Query) -> RenderResult<String> {
    let runs = entries(settings);
    let mut warnings = Vec::new();
    let snapshots = queue_snapshots(settings, &mut warnings);
    let sources: Vec<(String, PathBuf)> = match &settings.jobs {
        Some(path) => vec![("shared".into(), path.clone())],
        None => settings
            .projects
            .iter()
            .map(|project| (project.name.clone(), project.root.join("reports/jobs.jsonl")))
            .collect(),
    };
    html::page(
        "Queue",
        |output| {
            html! { output,
                <details>
                    <summary>
                        "Queue filters · "
                        {if query.get("group").is_empty() { "all groups" } else { query.get("group") }}
                        {if query.get("completed") == "1" { " · includes completed" } else { "" }}
                    </summary>
                    <form class="filters">
                        <fieldset>
                            <legend>"Queue filters"</legend>
                            <label>"Group "<input name="group" value=query.get("group") /></label>
                            <label>
                                "Show completed "
                                <input
                                    type="checkbox"
                                    name="completed"
                                    value="1"
                                    checked=(query.get("completed") == "1")
                                />
                            </label>
                            <button>"Filter"</button>
                        </fieldset>
                    </form>
                </details>
            }?;
            notice(output, &warnings)?;
            if snapshots.is_empty() {
                html! { output,
                    <p>
                        "No live queue source. Use --pueue <config>, --queue <snapshot> or reports/queue.json."
                    </p>
                }?;
            }
            if let Some(path) = &settings.queue {
                if let Ok(time) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) {
                    html! { output, <p>"Shared snapshot updated "{|output| html::age(output, time)}"."</p> }?;
                }
            }
            for project in &settings.projects {
                if settings.queue.is_none() && settings.pueue.is_none() {
                    if let Ok(time) = std::fs::metadata(project.root.join("reports/queue.json"))
                        .and_then(|metadata| metadata.modified())
                    {
                        html! { output, <p>{project.name}" snapshot updated "{|output| html::age(output, time)}"."</p> }?;
                    }
                }
            }
            let mut queue_errors = Vec::new();
            html! { output,
                <h2>"Live tasks"</h2>
                <div class="table-scroll" role="region" aria-label="Live tasks" tabindex="0">
                    <table>
                        <tr>
                            <th>"Queue / task"</th>
                            <th>"Group"</th>
                            <th>"State"</th>
                            <th>"Label / run"</th>
                            <th>"Dependencies"</th>
                        </tr>
                        {|output| {
                            for (source, snapshot) in &snapshots {
                                let tasks = match queue::tasks(snapshot) {
                                    Ok(tasks) => tasks,
                                    Err(error) => {
                                        queue_errors.push(format!("{source}: {error}"));
                                        continue;
                                    }
                                };
                                for task in tasks {
                                    if task.state == "Done" && query.get("completed") != "1" {
                                        continue;
                                    }
                                    if !query.get("group").is_empty() && task.group != query.get("group") {
                                        continue;
                                    }
                                    let associated = queue_run(settings, source, &task, &runs);
                                    html! { output,
                                        <tr id=format!("task-{}-{}", encode(source), encode(&task.id))>
                                            <td>{source}"/"{task.id}</td>
                                            <td>{task.group}</td>
                                            <td>{task.state}</td>
                                            <td>
                                                {|output| {
                                                    if let Some(entry) = associated {
                                                        html! { output,
                                                            <a href=format!(
                                                                "/run?run={}",
                                                                encode(&entry.id()),
                                                            )>{entry.id()}</a>
                                                        }
                                                    } else {
                                                        output.text(&task.label)
                                                    }
                                                }}
                                            </td>
                                            <td>
                                                {|output| {
                                                    for (index, id) in task.dependencies.iter().enumerate() {
                                                        if index > 0 {
                                                            output.text(", ")?;
                                                        }
                                                        html! { output,
                                                            <a href=format!(
                                                                "/jobs?completed=1#task-{}-{}",
                                                                encode(source),
                                                                encode(id),
                                                            )>{id}</a>
                                                        }?;
                                                    }
                                                    Ok(())
                                                }}
                                            </td>
                                        </tr>
                                    }?;
                                }
                            }
                            Ok(())
                        }}
                    </table>
                </div>
            }?;
            notice(output, &queue_errors)?;
            html! { output,
                <h2>"Completion history"</h2>
                <p class="dim">
                    "Newest 100 retained completions by end time. Live queue state takes precedence over archived callbacks, including restarted tasks. Fault details require a matching archived attempt. Live history retains up to 100 recently finished tasks plus the newest 64 task IDs; group filtering applies within retained history."
                </p>
                <div class="table-scroll" role="region" aria-label="Completion history" tabindex="0">
                    <table id="completion-history">
                        <tr>
                            <th>"Queue / task"</th>
                            <th>"Label"</th>
                            <th>"Group"</th>
                            <th>"Outcome"</th>
                            <th>"Start"</th>
                            <th>"End"</th>
                            <th>"Faults"</th>
                        </tr>
                        {|output| {
                            for completion in completion_history(
                                settings,
                                &snapshots,
                                &sources,
                                query.get("group"),
                            ) {
                                let job = completion.job;
                                let faults = job
                                    .faults
                                    .iter()
                                    .map(|(kind, count)| format!("{kind} × {}", number(*count)))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                html! { output,
                                    <tr>
                                        <td>{completion.source}"/"{completion.id}</td>
                                        <td>
                                            {if job.label.is_empty() {
                                                "Unlabelled task"
                                            } else {
                                                &job.label
                                            }}
                                        </td>
                                        <td>{job.group}</td>
                                        <td class={if job.outcome == "Success" {
                                            "good"
                                        } else {
                                            "warn"
                                        }}>{job.outcome}</td>
                                        <td>{job.start}</td>
                                        <td>{job.end}</td>
                                        <td>{faults}</td>
                                    </tr>
                                }?;
                            }
                            Ok(())
                        }}
                    </table>
                </div>
            }
        },
        false,
    )
}

fn devices(settings: &Settings, query: &Query) -> RenderResult<String> {
    html::page(
        "Devices",
        |output| {
            html! { output,
                <details>
                    <summary>
                        "Device filters · "
                        {["project", "host", "device"]
                            .iter()
                            .filter(|key| !query.get(key).is_empty())
                            .count()} " active"
                    </summary>
                    <form class="filters">
                        <fieldset>
                            <legend>"Device filters"</legend>
                            {|output| fields(
                                output,
                                query,
                                &[("project", "Project"), ("host", "Host"), ("device", "Device")],
                            )}
                            <button>"Filter"</button>
                        </fieldset>
                    </form>
                </details>
            }?;
            for project in &settings.projects {
                if !query.get("project").is_empty() && project.name != query.get("project") {
                    continue;
                }
                let streams = match inputs::streams(&project.root) {
                    Ok(streams) => streams,
                    Err(error) => {
                        notice(output, &[format!("{}: {error}", project.name)])?;
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
                    let names = metric_page(std::slice::from_ref(&stream.records), "", "").names;
                    html! { output,
                        <h2>{title}</h2>
                        <p class={if age >= 300.0 {
                            "warn"
                        } else {
                            ""
                        }}>
                            "Sample age: "{number(age)}" seconds"
                            {if age >= 300.0 { " · stale; charts show historical readings" } else { "" }}
                        </p>
                        <dl class="metrics">
                            {|output| {
                                if let Some(latest) = stream.records.last() {
                                    for metric in names.iter().take(6) {
                                        html! { output,
                                            <div>
                                                <dt>{metric}</dt>
                                                <dd>
                                                    {latest
                                                        .get(metric)
                                                        .map(number)
                                                        .unwrap_or_else(|| "–".into())}
                                                </dd>
                                            </div>
                                        }?;
                                    }
                                }
                                Ok(())
                            }}
                        </dl>
                    }?;
                    for metric in names {
                        let points = stream
                            .records
                            .iter()
                            .filter_map(|record| {
                                Some(((record.get("time")? - html::now()) / 3600.0, record.get(&metric)?))
                            })
                            .collect();
                        html::chart(
                            output,
                            &format!("{metric} · hours before now"),
                            "hours before now",
                            &[Series {
                                label: title.clone(),
                                points,
                                raw: None,
                            }],
                        )?;
                    }
                }
            }
            Ok(())
        },
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn metric_order_and_filtering_do_not_depend_on_observation_order() {
        let mut log = (0..40)
            .map(|index| Record(vec![(format!("metric_{index:02}"), f64::from(index))]))
            .collect::<Vec<_>>();
        let expected = (0..32).map(|index| format!("metric_{index:02}")).collect::<Vec<_>>();
        for _ in 0..40 {
            assert_eq!(metric_page(&[log.clone()], "", "").names, expected);
            assert_eq!(metric_page(&[log.clone()], "METRIC_39", "").names, ["metric_39"]);
            assert!(metric_page(&[log.clone()], "missing", "").names.is_empty());
            let first = metric_page(&[log.clone()], "", "");
            let second = metric_page(&[log.clone()], "", first.next.as_deref().unwrap());
            assert_eq!(
                second.names,
                (32..40).map(|index| format!("metric_{index:02}")).collect::<Vec<_>>()
            );
            assert!(second.next.is_none());
            let latest = data::latest_values(&log).unwrap();
            assert!(latest.0.windows(2).all(|pair| pair[0].0 < pair[1].0));
            log.rotate_left(1);
        }
        let mut log = (0..40)
            .map(|index| Record(vec![(format!("device/temperature_{index:02}"), f64::from(index))]))
            .collect::<Vec<_>>();
        log.push(Record(vec![
            ("loss".into(), 0.5),
            ("accuracy".into(), 0.9),
            ("train/step_seconds_mean".into(), 0.025),
            ("train/step_seconds_max".into(), 0.04),
            ("train/tokens_per_second".into(), 4000.0),
        ]));
        let expected = [
            "loss",
            "train/step_seconds_max",
            "train/step_seconds_mean",
            "train/tokens_per_second",
            "accuracy",
        ];
        for _ in 0..log.len() {
            let names = metric_page(&[log.clone()], "", "").names;
            assert_eq!(&names[..expected.len()], expected);
            let latest = data::latest_values(&log).unwrap();
            assert_eq!(
                latest
                    .0
                    .iter()
                    .take(expected.len())
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            log.rotate_left(1);
        }
    }

    #[test]
    fn rendering_errors_are_http_failures_without_partial_markup() {
        let limit = Response::html(Err(rstml_ssr::RenderError::OutputLimit));
        assert_eq!(limit.status, 413);
        assert!(String::from_utf8(limit.body)
            .unwrap()
            .contains("HTML output limit exceeded"));
        let invalid = Response::html(Err(rstml_ssr::RenderError::NullCharacter));
        assert_eq!(invalid.status, 500);
        let fallback = Response::error(500, "bad\0text");
        assert_eq!(fallback.status, 500);
        assert_eq!(fallback.content_type, "text/plain; charset=utf-8");
        assert!(!fallback.body.contains(&0));
    }

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
    fn python_reports_are_consumable_and_sparse_filters_use_latest_observations() {
        let mut fixture = Fixture::new();
        let python = std::env::var_os("UV_PYTHON").unwrap_or_else(|| "python3".into());
        let output = std::process::Command::new(python)
            .current_dir(&fixture.root)
            .env("PYTHONPATH", Path::new(env!("CARGO_MANIFEST_DIR")).join("python"))
            .args([
                "-c",
                r#"
import runboard
values = [None, True, False, "", "λ" * 512, 0, -1, 10000, 1e-300, 1e300]
for index, value in enumerate(values):
    with runboard.init(project="sdk", name=f"case-{index}", config={"value": value}) as run:
        run.log({"train/loss": 1.0, "eval/accuracy": 0.9}, step=1)
        run.log({"train/loss": 0.5}, step=2)
with runboard.init(project="sdk", name="path-limit", config={"x" * 189: 1}):
    pass
with runboard.init(project="resume", name="branch") as run:
    run.log({"loss": 1, "eval": 0.9}, step=1)
    run.log({"loss": 4, "abandoned": 99}, step=4)
with runboard.init(project="resume", name="branch", resume_from=1) as run:
    run.log({"loss": 0.5}, step=8)
"#,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let root = fixture.root.join(".runboard/sdk");
        for stage in data::stages(&root) {
            assert!(inputs::metadata(&root, &stage.name).is_ok(), "{}", stage.name);
        }
        fixture.settings.projects = vec![Project {
            name: "sdk".into(),
            root,
        }];
        let resume_root = fixture.root.join(".runboard/resume");
        let resumed = data::stage(&resume_root, "branch").unwrap();
        let history = data::training_records(&resumed);
        assert_eq!(
            history
                .iter()
                .map(|record| record.get("step").unwrap())
                .collect::<Vec<_>>(),
            vec![1.0, 8.0]
        );
        assert!(data::latest_values(&history).unwrap().get("abandoned").is_none());
        fixture.settings.projects.push(Project {
            name: "resume".into(),
            root: resume_root,
        });
        assert!(!fixture.text("/report?run=resume/branch").contains("abandoned"));
        let filtered = fixture.text("/?metric=eval/accuracy&minimum=0.8");
        assert!(filtered.contains("10 matching runs"));
        let compare = fixture.text("/compare?run=sdk/case-0&metric=eval/accuracy");
        assert!(compare.contains("eval/accuracy: 0.9"));
        for path in ["/", "/run?run=sdk/case-0", "/report?run=sdk/case-0", "/diagnostics"] {
            fixture.text(path);
        }
        fs::write(
            fixture.settings.projects[0].root.join("reports/bad_train.log"),
            "{\"step\":1,\"bad\\u0000metric\":1}\n",
        )
        .unwrap();
        fixture.text("/");
        assert!(fixture.text("/diagnostics").contains("invalid or partial JSON"));
    }

    #[test]
    fn dashboard_route_snapshots() {
        let mut fixture = Fixture::new();
        fixture.settings.artifacts = true;
        fs::create_dir_all(fixture.root.join("a/artifacts/same")).unwrap();
        fs::write(fixture.root.join("a/artifacts/same/note.txt"), "<producer>& text").unwrap();
        fs::write(
            fixture.root.join("a/reports/queue.json"),
            r#"{"tasks":{"7":{"label":"same","group":"gpu","status":"Queued","dependencies":[8]}}}"#,
        )
        .unwrap();
        fs::write(
            fixture.root.join("a/reports/jobs.jsonl"),
            "{\"id\":7,\"label\":\"same\",\"outcome\":\"Success\",\"start\":\"start\",\"end\":\"end\",\"faults\":{}}\n",
        )
        .unwrap();
        for (name, path) in [
            ("runs", "/?sort=name"),
            ("run", "/run?run=a/same"),
            ("compare", "/compare?run=a/same&run=b/same&metric=loss&group=1"),
            ("report", "/report?run=a/same&metric=loss"),
            ("queue", "/jobs?completed=1"),
            ("devices", "/device?host=atlas"),
            ("inputs", "/diagnostics"),
        ] {
            let page = fixture.text(path);
            let main = page.split_once("<main>").unwrap().1.split_once("</main>").unwrap().0;
            insta::with_settings!({filters => vec![
                (r#"data-time="[0-9]+""#, "data-time=\"[timestamp]\""),
                (r">[0-9]+[smhd] ago</time>", ">[age]</time>"),
            ]}, { insta::assert_snapshot!(name, main); });
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
        let escaped = fixture.text("/?name=%22%3E%3Cscript%3E");
        assert!(escaped.contains("value=\"&quot;&gt;&lt;script&gt;\""));
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
    fn recent_runs_are_default_and_active_sort_is_explicit() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("b/reports/same_TRAINED"), "train exit 0").unwrap();
        let completed = fs::File::open(fixture.root.join("b/reports/same_train.log")).unwrap();
        completed
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now() + Duration::from_secs(60)))
            .unwrap();
        let page = fixture.text("/");
        assert!(page.find("b/same").unwrap() < page.find("a/same").unwrap());
        let page = fixture.text("/?sort=active");
        assert!(page.find("a/same").unwrap() < page.find("b/same").unwrap());
        let page = fixture.text("/?sort=recent");
        assert!(page.find("b/same").unwrap() < page.find("a/same").unwrap());
    }

    #[test]
    fn metric_aggregation_counts_only_reported_samples() {
        let log = vec![
            Record(vec![("step".into(), 1.0), ("train/tokens_per_second".into(), 1000.0)]),
            Record(vec![("step".into(), 99.0), ("other".into(), 1.0)]),
            Record(vec![("step".into(), 100.0), ("train/tokens_per_second".into(), 5000.0)]),
        ];
        for (aggregation, expected) in [
            (MetricAggregation::Latest, 5000.0),
            (MetricAggregation::Mean, 3000.0),
            (MetricAggregation::Median, 3000.0),
            (MetricAggregation::Min, 1000.0),
            (MetricAggregation::Max, 5000.0),
        ] {
            assert_eq!(
                metric_value(&log, "train/tokens_per_second", aggregation),
                Some(expected)
            );
            assert_eq!(metric_value(&log, "missing", aggregation), None);
        }
        assert_eq!(median([1000.0, 2000.0, 3000.0, 1e12].into_iter()), Some(2500.0));
        assert_eq!(median([1000.0, 2000.0, 1e12].into_iter()), Some(2000.0));
        assert_eq!(median([f64::MAX, f64::MAX].into_iter()), Some(f64::MAX));
        assert_eq!(median([-f64::MAX, f64::MAX].into_iter()), Some(0.0));
        assert_eq!(median([f64::NAN, f64::INFINITY].into_iter()), None);
        let fixture = Fixture::new();
        assert_eq!(
            route(&fixture.settings, "/compare?run=a/same&aggregate=unknown").status,
            400
        );
        assert_eq!(
            route(&fixture.settings, "/compare?run=a/same&aggregate=median").status,
            200
        );
    }

    #[test]
    fn comparison_performance_uses_latest_sparse_values_beyond_preview_limits() {
        let fixture = Fixture::new();
        let mut log = String::from("{\"step\":1,\"loss\":1,\"train/step_seconds_mean\":0.01,\"train/step_seconds_max\":0.04,\"train/tokens_per_second\":4000}\n");
        for index in 0..140 {
            log.push_str(&format!("{{\"step\":{},\"profile/block_{index}\":1}}\n", index + 2));
        }
        log.push_str("{\"step\":142,\"train/step_seconds_mean\":0.025}\n");
        fs::write(fixture.root.join("a/reports/same_train.log"), log).unwrap();
        for path in [
            "/compare?run=a/same&run=b/same&metric=loss",
            "/report?run=a/same&run=b/same",
        ] {
            let page = fixture.text(path);
            let table = page
                .split_once("<table id=\"training-performance\">")
                .unwrap()
                .1
                .split_once("</table>")
                .unwrap()
                .0;
            assert!(table.contains("<td>25</td><td>40</td><td>4000</td>"));
            assert_eq!(table.matches("Not reported").count(), 3);
        }
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
    fn completion_history_merges_authoritative_state_and_sorts_instants() {
        let mut fixture = Fixture::new();
        let a = fixture.root.join("a/reports/jobs.jsonl");
        let b = fixture.root.join("b/reports/jobs.jsonl");
        fs::write(&a, concat!(
            "{\"id\":2,\"group\":\"gpu\",\"outcome\":\"Success\",\"end\":\"2026-11-25T18:30:00Z\"}\n",
            "{\"id\":1,\"group\":\"gpu\",\"outcome\":\"Success\",\"end\":\"2026-11-25T20:00:00+02:00\"}\n",
            "{\"id\":3,\"group\":\"gpu\",\"outcome\":\"Success\",\"start\":\"old\",\"end\":\"2026-11-25T17:00:00Z\",\"faults\":{\"old\":9}}\n",
            "{\"id\":7,\"group\":\"gpu\",\"outcome\":\"Success\",\"end\":\"2027-01-01T00:00:00Z\"}\n"
        )).unwrap();
        fs::write(&b, "{\"id\":3,\"label\":\"archive\",\"group\":\"gpu\",\"outcome\":\"finished-with-faults\",\"start\":\"2026-11-25T20:00:00+01:00\",\"end\":\"2026-11-25T22:00:00+02:00\",\"faults\":{\"retry\":2}}\n").unwrap();
        let live = r#"{"tasks":{"3":{"label":"live","group":"cpu","status":{"Done":{"result":"Killed","start":"2026-11-25T18:40:00Z","end":"2026-11-25T19:00:00Z"}}},"7":{"status":"Queued"}}}"#;
        let other = r#"{"tasks":{"3":{"label":"other","group":"gpu","status":{"Done":{"result":"Success","start":"2026-11-25T19:00:00Z","end":"2026-11-25T20:00:00Z"}}}}}"#;
        let sources = vec![("a".into(), a), ("b".into(), b)];
        let snapshots = vec![("a".into(), live.into()), ("b".into(), other.into())];
        let history = completion_history(&fixture.settings, &snapshots, &sources, "");
        assert_eq!(
            history
                .iter()
                .map(|row| (row.source.as_str(), row.id.as_str()))
                .collect::<Vec<_>>(),
            [("b", "3"), ("a", "3"), ("a", "2"), ("a", "1")]
        );
        assert_eq!(history[0].job.faults, [("retry".into(), 2.0)]);
        assert_eq!(history[0].job.outcome, "finished-with-faults");
        assert!(history[1].job.faults.is_empty());
        assert_eq!(history[1].job.outcome, "Killed");
        assert_eq!(
            completion_history(&fixture.settings, &snapshots, &sources, "gpu").len(),
            3
        );
        fixture.settings.queue = Some(fixture.root.join("shared.json"));
        let shared = vec![("shared".into(), live.into())];
        let history = completion_history(&fixture.settings, &shared, &sources, "");
        assert_eq!(history.len(), 3);
        assert!(history.iter().all(|row| row.source == "shared"));
        assert_eq!(history[0].id, "3");
        assert_eq!(history[0].job.outcome, "Killed");
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
        assert!(fixture.text("/").contains("/jobs?completed=1#task-a-7"));
        assert!(fixture.text("/").contains("Pueue #7"));
        assert!(fixture.text("/?project=b").contains("Pueue #7"));
        fixture.settings.queue = Some(fixture.root.join("a/reports/queue.json"));
        assert!(!fixture.text("/jobs").contains("/run?run=a%2Fsame"));
        assert!(!fixture.text("/?project=a").contains("/jobs?completed=1#task-shared-7"));
        fs::write(fixture.root.join("b/reports/same_meta.json"), r#"{"task_id":8}"#).unwrap();
        assert!(fixture.text("/jobs").contains("/run?run=a%2Fsame"));
        assert!(fixture.text("/").contains("/jobs?completed=1#task-shared-7"));
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
        let page = fixture.text("/compare?run=a/same&run=a/next&metric=loss&group=1&smoothing=raw");
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
        assert_eq!(
            curve(&records, "loss", &query, Axis::Seconds, smoothing(&query, "loss")),
            [(20.0, 3.0)]
        );
        let query = Query::parse("axis=time").unwrap();
        assert!(curve(&records, "loss", &query, Axis::Time, smoothing(&query, "loss")).is_empty());
        let query = Query::parse("smoothing=ema&smooth=3").unwrap();
        assert_eq!(
            curve(&records, "loss", &query, Axis::Step, smoothing(&query, "loss")),
            [(1.0, 4.0), (2.0, 3.0)]
        );
        let query = Query::parse("smoothing=ema&smooth=3&start=2").unwrap();
        assert_eq!(
            curve(&records, "loss", &query, Axis::Step, smoothing(&query, "loss")),
            [(2.0, 3.0)]
        );
        let query = Query::parse("smoothing=ema&smooth=1").unwrap();
        assert_eq!(
            curve(&records, "loss", &query, Axis::Step, smoothing(&query, "loss")),
            [(1.0, 4.0), (2.0, 2.0)]
        );
        let mut budgets = records;
        for (record, (tokens, flops)) in budgets.iter_mut().zip([(1000.0, 1e12), (3000.0, 3e12)]) {
            record
                .0
                .extend([(data::TOKENS_AXIS.into(), tokens), ("train/flops_total".into(), flops)]);
        }
        let query = Query::parse("smoothing=ema&smooth=3").unwrap();
        let series = training_series("run".into(), &budgets, "loss", &query, Axis::Tokens, None);
        assert_eq!(series.raw.unwrap().1, [(1000.0, 4.0), (3000.0, 2.0)]);
        assert_eq!(series.points, [(1000.0, 4.0), (3000.0, 3.0)]);
        let series = training_series(
            "run".into(),
            &budgets,
            "loss",
            &query,
            Axis::FloatingPointOperations,
            None,
        );
        assert_eq!(series.points, [(1e12, 4.0), (3e12, 3.0)]);
        budgets[0].0.retain(|(key, _)| key != data::TOKENS_AXIS);
        let series = training_series("run".into(), &budgets, "loss", &query, Axis::Tokens, None);
        assert_eq!(series.points, [(3000.0, 3.0)]);
        budgets[1]
            .0
            .iter_mut()
            .find(|(key, _)| key == "train/flops_total")
            .unwrap()
            .1 = -1.0;
        assert_eq!(
            curve(&budgets, "loss", &query, Axis::FloatingPointOperations, Smoothing::Raw),
            [(1e12, 4.0)]
        );
    }

    #[test]
    fn gaussian_is_centered_and_agrees_with_distance_weighted_reference() {
        let points: Vec<(f64, f64)> = (0..=100)
            .map(|index| {
                let distance = f64::from(index.min(100 - index));
                let x = if index <= 50 {
                    distance * distance
                } else {
                    5000.0 - distance * distance
                };
                (x, if index == 50 { 10.0 } else { 1.0 })
            })
            .collect();
        let smoothed = gaussian_curve(&points, 0.05, None);
        for (index, &(x, y)) in smoothed.iter().enumerate() {
            let mut numerator = 0.0;
            let mut denominator = 0.0;
            for &(neighbor, value) in &points {
                let weight = (-0.5 * ((neighbor - x) / 250.0).powi(2)).exp();
                numerator += weight * value;
                denominator += weight;
            }
            assert!((y - numerator / denominator).abs() < 0.002);
            assert!((1.0..=10.0).contains(&y));
            assert!((y - smoothed[100 - index].1).abs() < 1e-10);
        }
        assert!(smoothed[50].1 > smoothed[49].1);
        assert!(smoothed[50].1 > smoothed[51].1);
        for value in [0.0, 1.0, -1.0, f64::MAX, -f64::MAX] {
            assert_eq!(
                gaussian_curve(&[(0.0, value), (1.0, value)], 0.01, None),
                [(0.0, value), (1.0, value)]
            );
        }
        let records: Vec<Record> = points
            .iter()
            .map(|&(x, y)| Record(vec![("step".into(), x), ("loss".into(), y)]))
            .collect();
        let full = training_series(
            "run".into(),
            &records,
            "loss",
            &Query::parse("smoothing=gaussian&bandwidth=5").unwrap(),
            Axis::Step,
            None,
        );
        let cropped = training_series(
            "run".into(),
            &records,
            "loss",
            &Query::parse("smoothing=gaussian&bandwidth=5&start=1600&end=3400").unwrap(),
            Axis::Step,
            None,
        );
        assert_eq!(cropped.points, full.points[40..=60]);
        let logs = vec![
            vec![
                Record(vec![("step".into(), 0.0), ("loss".into(), 1.0)]),
                Record(vec![("step".into(), 100.0), ("loss".into(), 5.0)]),
                Record(vec![("step".into(), 200.0), ("loss".into(), 1.0)]),
            ],
            vec![Record(vec![("step".into(), 10000.0), ("loss".into(), 1.0)])],
        ];
        let range = training_range(&logs, "loss", Axis::Step);
        assert_eq!(range, Some((0.0, 10000.0)));
        let shared = training_series("short".into(), &logs[0], "loss", &Query(Vec::new()), Axis::Step, range);
        let weight = (-0.5_f64 * (100.0_f64 / 200.0).powi(2)).exp();
        let expected = (5.0 + 2.0 * weight) / (1.0 + 2.0 * weight);
        assert!((shared.points[1].1 - expected).abs() < 0.002);
        assert!(shared.points[1].1 < 3.0);
        let cropped = training_series(
            "short".into(),
            &logs[0],
            "loss",
            &Query::parse("start=100").unwrap(),
            Axis::Step,
            range,
        );
        assert_eq!(cropped.points, shared.points[1..]);
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
        for query in [
            "smooth=0",
            "smooth=101",
            "smoothing=bogus",
            "bandwidth=0",
            "bandwidth=11",
            "bandwidth=NaN",
            "minimum=NaN",
            "axis=bogus",
            "start=Infinity",
        ] {
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
        let invalid = fixture.root.join("invalid.html");
        fixture.settings.export = Some(invalid.clone());
        fs::write(
            fixture.root.join("a/reports/same_train.log"),
            "{\"step\":1,\"bad\\u0000metric\":2}\n",
        )
        .unwrap();
        export(&fixture.settings).unwrap();
        assert!(invalid.exists());
        let response = route(&fixture.settings, "/report?run=a/same");
        assert_eq!(response.status, 200);
        assert!(fixture.text("/diagnostics").contains("invalid or partial JSON"));
        assert!(!response.body.contains(&0));
    }
}
