use crate::data::{self, Record, Stage};
use crate::json::{self, Scalar};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub type Fields = Vec<(String, Scalar)>;
pub const ARTIFACT_BYTES_MAX: u64 = 16 << 20;

#[derive(Clone)]
pub struct Summary {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub direction: String,
}

pub struct Stream {
    pub host: String,
    pub device: String,
    pub records: Vec<Record>,
}

pub fn scalar(value: &Scalar) -> String {
    match value {
        Scalar::Number(value) => crate::html::number(*value),
        Scalar::Text(value) => value.clone(),
        Scalar::Bool(value) => value.to_string(),
        Scalar::Null => "null".into(),
    }
}

pub fn read_json(path: &Path) -> Result<Fields, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    if metadata.len() > 4 << 20 {
        return Err("exceeds 4 MiB".into());
    }
    let body = data::read_text(path, 4 << 20).ok_or("cannot read bounded UTF-8 input")?;
    if !body.trim_start().starts_with('{') {
        return Err("expected a JSON object".into());
    }
    json::flatten(&body)
}

pub fn metadata(root: &Path, name: &str) -> Result<Fields, String> {
    assert!(data::name_is_safe(name));
    let path = root.join("reports").join(format!("{name}_meta.json"));
    match fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        _ => {
            let fields = read_json(&path)?;
            if fields.len() > 128 {
                return Err("metadata exceeds 128 fields".into());
            }
            let mut bytes = 0usize;
            for (name, value) in &fields {
                if matches!(value, Scalar::Number(value) if !value.is_finite()) {
                    return Err(format!("{name}: non-finite metadata value"));
                }
                let value = scalar(value);
                if name.len() > 200 || value.len() > 1024 {
                    return Err("metadata name/value exceeds 200/1024 bytes".into());
                }
                bytes += name.len() + value.len();
            }
            if bytes > 16 << 10 {
                return Err("metadata exceeds 16 KiB of scalar data".into());
            }
            Ok(fields)
        }
    }
}

pub fn summaries(root: &Path, name: &str) -> Result<Vec<Summary>, String> {
    assert!(data::name_is_safe(name));
    let path = root.join("reports").join(format!("{name}_summary.json"));
    if !path.try_exists().map_err(|error| error.to_string())? {
        return Ok(Vec::new());
    }
    let pairs = read_json(&path)?;
    let mut summaries = Vec::new();
    for (path, scalar) in &pairs {
        let Some(name) = path
            .strip_prefix("metrics.")
            .and_then(|path| path.strip_suffix(".value"))
        else {
            continue;
        };
        let Scalar::Number(value) = scalar else {
            return Err(format!("{name}: value must be numeric"));
        };
        if !value.is_finite() {
            return Err(format!("{name}: non-finite value"));
        }
        let direction = json::text(&pairs, &format!("metrics.{name}.direction")).unwrap_or("none");
        let unit = json::text(&pairs, &format!("metrics.{name}.unit")).unwrap_or("");
        if unit.len() > 64 {
            return Err(format!("{name}: unit exceeds 64 bytes"));
        }
        if !matches!(direction, "higher" | "lower" | "none") {
            return Err(format!("{name}: direction must be higher, lower or none"));
        }
        if summaries.len() == 128 {
            return Err("summary exceeds 128 metrics".into());
        }
        if name.len() > 200 {
            return Err("summary metric name exceeds 200 bytes".into());
        }
        summaries.push(Summary {
            name: name.into(),
            value: *value,
            unit: unit.into(),
            direction: direction.into(),
        });
    }
    if summaries.is_empty() {
        return Err("summary has no metrics.<name>.value fields".into());
    }
    Ok(summaries)
}

pub fn diagnostics(root: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let entries = match fs::read_dir(root.join("reports")) {
        Ok(entries) => entries,
        Err(error) => return vec![format!("reports: {error}")],
    };
    let mut bytes_left = 16u64 << 20;
    for entry in entries.take(data::DIRECTORY_ENTRIES_MAX) {
        if warnings.len() >= 100 {
            warnings.push("More input errors omitted".into());
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == "queue.json"
            || ["_meta.json", "_summary.json", "_progress.json", "_paired.json"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
        {
            let size = fs::metadata(&path).map(|value| value.len()).unwrap_or(0);
            if size.min(4 << 20) > bytes_left {
                warnings.push("Diagnostic scan reached its 16 MiB budget".into());
                break;
            }
            bytes_left -= size.min(4 << 20);
            if let Err(error) = read_json(&path) {
                warnings.push(format!("{name}: {error}"));
            }
            if name.ends_with("_progress.json") && data::progress(&path).is_none() {
                warnings.push(format!("{name}: invalid phase, step or PID, or input exceeds 4 KiB"));
            }
            if name.ends_with("_paired.json") && size > 1 << 20 {
                warnings.push(format!("{name}: paired report exceeds 1 MiB"));
            }
            if let Some(run) = name.strip_suffix("_meta.json") {
                if data::name_is_safe(run) {
                    if let Err(error) = metadata(root, run) {
                        warnings.push(format!("{name}: {error}"));
                    }
                }
            }
            if let Some(run) = name.strip_suffix("_summary.json") {
                if data::name_is_safe(run) {
                    if let Err(error) = summaries(root, run) {
                        warnings.push(format!("{name}: {error}"));
                    }
                }
            }
        }
        if name.ends_with("_train.log") || matches!(name.as_str(), "jobs.jsonl" | "device_telemetry.jsonl") {
            if bytes_left == 0 {
                warnings.push("Diagnostic scan reached its 16 MiB budget".into());
                break;
            }
            let size = match fs::metadata(&path) {
                Ok(value) => value.len(),
                Err(error) => {
                    warnings.push(format!("{name}: {error}"));
                    continue;
                }
            };
            let limit = bytes_left.min(256 << 10);
            bytes_left = bytes_left.saturating_sub(size.min(limit));
            let Some(body) = data::tail(&path, limit) else {
                warnings.push(format!("{name}: unreadable or nonregular input"));
                continue;
            };
            let mut invalid = 0u32;
            let mut nonfinite = 0u32;
            for line in body.lines() {
                if !line.trim_start().starts_with('{') {
                    if !name.ends_with("_train.log") && !line.trim().is_empty() {
                        invalid += 1;
                    }
                    continue;
                }
                if line.len() > 64 << 10 {
                    invalid += 1;
                    continue;
                }
                let pairs = match json::flatten(line) {
                    Ok(pairs) => pairs,
                    Err(_) => {
                        invalid += 1;
                        continue;
                    }
                };
                nonfinite += pairs
                    .iter()
                    .filter(|(_, value)| matches!(value, Scalar::Number(value) if !value.is_finite()))
                    .count() as u32;
                let integer = |key: &str| {
                    json::number(&pairs, key)
                        .is_some_and(|value| value.is_finite() && value >= 0.0 && value.fract() == 0.0)
                };
                let time = || {
                    json::number(&pairs, "time").is_some_and(|value| value.is_finite() && (0.0..=4e9).contains(&value))
                };
                let valid = match name.as_str() {
                    "jobs.jsonl" => integer("id") && json::text(&pairs, "outcome").is_some(),
                    "device_telemetry.jsonl" => time(),
                    _ if json::text(&pairs, "monitor").is_some() => time(),
                    _ if pairs.iter().any(|(key, _)| key == "step") => integer("step"),
                    _ => true,
                };
                if !valid {
                    invalid += 1;
                }
            }
            if nonfinite > 0 {
                warnings.push(format!(
                    "{name}: {nonfinite} non-finite values omitted in the inspected tail"
                ));
            }
            if invalid > 0 {
                warnings.push(format!(
                    "{name}: {invalid} invalid or partial JSON records in the inspected tail"
                ));
            }
            if size > data::LOG_BYTES_MAX {
                warnings.push(format!("{name}: history limited to the last 16 MiB"));
            }
            if let Some(run) = name.strip_suffix("_train.log") {
                if !data::name_is_safe(run) {
                    warnings.push(format!("{name}: unsafe run name, skipped"));
                }
            }
        }
    }
    if data::stages(root).len() == data::STAGES_MAX {
        warnings.push("Run discovery reached its 1024-run limit".into());
    }
    warnings
}

pub fn streams(root: &Path) -> Result<Vec<Stream>, String> {
    let path = root.join("reports/device_telemetry.jsonl");
    if !path.try_exists().map_err(|error| error.to_string())? {
        return Ok(Vec::new());
    }
    let body = data::tail(&path, 6 << 20).ok_or("cannot read device telemetry")?;
    let mut streams: BTreeMap<(String, String), Vec<Record>> = BTreeMap::new();
    for line in body.lines().rev().take(data::RECORDS_MAX) {
        if line.len() > 64 << 10 {
            continue;
        }
        let Ok(pairs) = json::flatten(line) else { continue };
        if !json::number(&pairs, "time").is_some_and(|value| value.is_finite() && (0.0..=4e9).contains(&value)) {
            continue;
        }
        let host = json::text(&pairs, "host").unwrap_or("local");
        let device = json::text(&pairs, "device").unwrap_or("default");
        if host.len() > 128 || device.len() > 128 {
            continue;
        }
        let key = (host.to_string(), device.to_string());
        if streams.len() == 64 && !streams.contains_key(&key) {
            return Err("telemetry exceeds 64 host/device streams".into());
        }
        let numbers = pairs
            .into_iter()
            .filter_map(|(key, value)| match value {
                Scalar::Number(value) if value.is_finite() => Some((key, value)),
                _ => None,
            })
            .collect();
        streams.entry(key).or_default().push(Record(numbers));
    }
    Ok(streams
        .into_iter()
        .map(|((host, device), mut records)| {
            records.reverse();
            Stream { host, device, records }
        })
        .collect())
}

pub fn artifact_path(root: &Path, run: &str, relative: &str) -> Result<PathBuf, String> {
    if !data::name_is_safe(run) || relative.len() > 1024 || relative.is_empty() {
        return Err("invalid artifact path".into());
    }
    let mut current = root.to_path_buf();
    let path = PathBuf::from("artifacts").join(run).join(relative);
    let mut depth = 0u32;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err("artifact traversal rejected".into());
        };
        depth += 1;
        if depth > 10 {
            return Err("artifact nesting exceeds eight directories".into());
        }
        current.push(name);
        let metadata = fs::symlink_metadata(&current).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("artifact symlinks are not served".into());
        }
    }
    let canonical = current.canonicalize().map_err(|error| error.to_string())?;
    let base = root.canonicalize().map_err(|error| error.to_string())?;
    if !canonical.starts_with(base.join("artifacts").join(run)) {
        return Err("artifact outside run directory".into());
    }
    Ok(canonical)
}

pub fn artifacts(root: &Path, run: &str) -> Result<Vec<(String, u64)>, String> {
    assert!(data::name_is_safe(run));
    let base = root.join("artifacts").join(run);
    if !base.try_exists().map_err(|error| error.to_string())? {
        return Ok(Vec::new());
    }
    let mut pending = VecDeque::from([String::new()]);
    let mut found = Vec::new();
    let mut inspected = 0u32;
    while let Some(directory) = pending.pop_front() {
        let path = if directory.is_empty() {
            artifact_path(root, run, ".")
        } else {
            artifact_path(root, run, &directory)
        }?;
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            inspected += 1;
            if inspected > 2048 {
                return Err("artifact listing exceeds 2048 entries".into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = if directory.is_empty() {
                name
            } else {
                format!("{directory}/{name}")
            };
            let metadata = entry.file_type().map_err(|error| error.to_string())?;
            if metadata.is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if relative.split('/').count() < 8 {
                    pending.push_back(relative);
                }
            } else if metadata.is_file() {
                let size = entry.metadata().map_err(|error| error.to_string())?.len();
                found.push((relative, size));
            }
        }
    }
    found.sort();
    Ok(found)
}

pub fn artifact(root: &Path, run: &str, name: &str) -> Result<Vec<u8>, String> {
    let path = artifact_path(root, run, name)?;
    let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > ARTIFACT_BYTES_MAX {
        return Err("artifact must be a regular file at most 16 MiB".into());
    }
    let file = fs::File::open(&path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > ARTIFACT_BYTES_MAX {
        return Err("artifact must be a regular file at most 16 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(ARTIFACT_BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > ARTIFACT_BYTES_MAX {
        return Err("artifact grew beyond 16 MiB".into());
    }
    if artifact_path(root, run, name)? != path {
        return Err("artifact changed while reading".into());
    }
    Ok(bytes)
}

pub fn log_warnings(stage: &Stage) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(path) = &stage.log_path else { return warnings };
    match fs::metadata(path) {
        Err(error) => warnings.push(error.to_string()),
        Ok(metadata) if metadata.len() > data::LOG_BYTES_MAX => {
            warnings.push("Showing the last 16 MiB of run history".into())
        }
        _ => {}
    }
    warnings
}
