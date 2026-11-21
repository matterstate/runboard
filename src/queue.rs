use crate::json::{self, Scalar};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OUTPUT_BYTES_MAX: usize = 4 << 20;

pub struct Task {
    pub id: String,
    pub label: String,
    pub group: String,
    pub state: String,
    pub directory: String,
    pub dependencies: Vec<String>,
}

pub fn snapshot(config: &Path) -> Result<String, String> {
    let active = snapshot_query(config, "status!=success status!=failed")?;
    let recent = snapshot_query(config, "order_by id desc first 64")?;
    merge_snapshots(&active, &recent)
}

fn merge_snapshots(active: &str, recent: &str) -> Result<String, String> {
    let active: serde_json::Value = serde_json::from_str(active).map_err(|error| format!("Pueue: {error}"))?;
    let mut recent: serde_json::Value = serde_json::from_str(recent).map_err(|error| format!("Pueue: {error}"))?;
    let active = active["tasks"].as_object().ok_or("Pueue tasks must be an object")?;
    let recent_tasks = recent["tasks"].as_object_mut().ok_or("Pueue tasks must be an object")?;
    for (id, task) in active {
        if recent_tasks.len() >= 1024 && !recent_tasks.contains_key(id) {
            return Err("Queue exceeds 1024 tasks".into());
        }
        recent_tasks.entry(id.clone()).or_insert_with(|| task.clone());
    }
    let body = serde_json::to_string(&recent).map_err(|error| format!("Pueue: {error}"))?;
    if body.len() > OUTPUT_BYTES_MAX {
        return Err("Pueue snapshot exceeds 4 MiB".into());
    }
    Ok(body)
}

fn snapshot_query(config: &Path, query: &str) -> Result<String, String> {
    let mut command = Command::new(option_env!("RUNBOARD_PUEUE").unwrap_or("pueue"));
    if config != Path::new("-") {
        command.arg("-c").arg(config);
    }
    let mut child = command
        .args(["status", "--json", query])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Pueue: {error}"))?;
    let stdout = child.stdout.take().ok_or("Pueue stdout unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take(OUTPUT_BYTES_MAX as u64 + 1)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err("Pueue status failed".to_string())
                }
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => break Err("Pueue status timed out".to_string()),
            Err(error) => break Err(format!("Pueue: {error}")),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = reader
        .join()
        .map_err(|_| "Pueue reader failed")?
        .map_err(|error| error.to_string())?;
    if output.len() > OUTPUT_BYTES_MAX {
        return Err("Pueue snapshot exceeds 4 MiB".into());
    }
    result?;
    String::from_utf8(output).map_err(|error| error.to_string())
}

pub fn tasks(body: &str) -> Result<Vec<Task>, String> {
    let pairs = json::flatten(body)?;
    let mut tasks = BTreeMap::new();
    for (path, value) in &pairs {
        let Some(path) = path.strip_prefix("tasks.") else {
            continue;
        };
        let Some((id, field)) = path.split_once('.') else {
            continue;
        };
        if !id.bytes().all(|byte| byte.is_ascii_digit()) || id.len() > 16 {
            continue;
        }
        if tasks.len() == 1024 && !tasks.contains_key(id) {
            return Err("Queue exceeds 1024 tasks".into());
        }
        let task = tasks.entry(id.to_string()).or_insert_with(|| Task {
            id: id.into(),
            label: String::new(),
            group: String::new(),
            state: "unknown".into(),
            directory: String::new(),
            dependencies: Vec::new(),
        });
        match (field, value) {
            ("label", Scalar::Text(value)) => task.label = value.clone(),
            ("group", Scalar::Text(value)) => task.group = value.clone(),
            ("path", Scalar::Text(value)) => task.directory = value.clone(),
            ("status", Scalar::Text(value)) => task.state = value.clone(),
            (_, _) if field.starts_with("status.") => task.state = field.split('.').nth(1).unwrap_or("unknown").into(),
            (_, Scalar::Number(value)) if field.starts_with("dependencies.") => {
                task.dependencies.push(format!("{value:.0}"))
            }
            _ => {}
        }
    }
    Ok(tasks.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_history_preserves_old_active_tasks_and_newer_statuses() {
        let body = merge_snapshots(
            r#"{"tasks":{"1":{"status":"Running"},"99":{"status":"Running"}}}"#,
            r#"{"tasks":{"99":{"status":"Done"},"100":{"status":"Queued"}}}"#,
        )
        .unwrap();
        let tasks = tasks(&body).unwrap();
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().any(|task| task.id == "1" && task.state == "Running"));
        assert!(tasks.iter().any(|task| task.id == "99" && task.state == "Done"));
        assert!(merge_snapshots("{}", "{}").is_err());
    }

    #[test]
    fn reads_pueue_states_and_dependencies() {
        let tasks = tasks(r#"{"tasks":{"7":{"label":"train","group":"gpu","path":"/project","status":{"Running":{"start":"now"}},"dependencies":[3]},"8":{"status":"Queued"}}}"#).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].state, "Running");
        assert_eq!(tasks[0].dependencies, ["3"]);
        assert_eq!(tasks[1].state, "Queued");
        assert!(super::tasks("broken").is_err());
    }
}
