//! Bounded readers for producer-owned output files.

use crate::json::{flatten, number, text, Scalar};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const STAGES_MAX: usize = 1024;
pub const RECORDS_MAX: usize = 100_000;
pub const DIRECTORY_ENTRIES_MAX: usize = 200_000;
pub const LOG_BYTES_MAX: u64 = 16 << 20;
const METADATA_BYTES_MAX: u64 = 1 << 20;
pub const TIERS: [&str; 3] = ["original", "hard", "easy"];
/// Suffixes a stage's scored variants carry: order-averaged, with fitted temperatures, and both.
pub const VARIANTS: [&str; 4] = ["", "-oa", "-t", "-t-oa"];
const TRAIN_LOG_SUFFIX: &str = "_train.log";
const PAIRED_SUFFIX: &str = "_paired.json";

pub struct Stage {
    pub name: String,
    pub trainer: String,
    pub log_path: Option<PathBuf>,
    pub modified: SystemTime,
    pub trained: Option<String>,
    pub progress: Option<Progress>,
}

pub struct Progress {
    pub phase: String,
    pub step: u64,
    pub pid: u32,
    pub modified: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrainingTime {
    Final(u64),
    Recorded(u64),
}

pub struct MonitorEvent {
    pub kind: String,
    pub time: SystemTime,
    pub phase: Option<String>,
    pub step: Option<u64>,
    pub idle_seconds: Option<u64>,
    pub exit_code: Option<i32>,
}

#[derive(Clone)]
pub struct Record(pub Vec<(String, f64)>);

impl Record {
    pub fn get(&self, key: &str) -> Option<f64> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, value)| *value)
    }
}

#[derive(Clone, Copy)]
pub struct Metrics {
    pub accuracy: f64,
    pub nll: f64,
    pub brier: f64,
}

pub struct PanelScore {
    pub panel: String,
    pub system: Metrics,
    pub jev: Metrics,
}

pub struct TierScore {
    pub tier: &'static str,
    pub accuracy: f64,
    pub ece: f64,
    pub brier: f64,
    pub coverage: f64,
    pub complete: bool,
}

pub struct Job {
    pub id: f64,
    pub label: String,
    pub outcome: String,
    pub start: String,
    pub end: String,
    pub faults: Vec<(String, f64)>,
}

/// Stage and system names come from URLs and are joined into paths: only these characters are allowed.
pub fn name_is_safe(name: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    !name.is_empty() && name.len() <= 200 && !name.starts_with('.') && name.chars().all(allowed)
}

fn file_names(directory: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .take(DIRECTORY_ENTRIES_MAX)
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect()
}

pub fn stages(root: &Path) -> Vec<Stage> {
    let reports = root.join("reports");
    let mut found: Vec<Stage> = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for file in file_names(&reports) {
        let Some(name) = file
            .strip_suffix(TRAIN_LOG_SUFFIX)
            .or_else(|| file.strip_suffix("_summary.json"))
            .or_else(|| file.strip_suffix("_meta.json"))
        else {
            continue;
        };
        if !name_is_safe(name) || found.len() == STAGES_MAX {
            continue;
        }
        if !names.insert(name.to_string()) {
            continue;
        }
        let log_path = reports.join(format!("{name}{TRAIN_LOG_SUFFIX}"));
        let log_path = log_path.is_file().then_some(log_path);
        let source = log_path.clone().unwrap_or_else(|| reports.join(&file));
        let Ok(metadata) = fs::metadata(source) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let trained = read_text(&reports.join(format!("{name}_TRAINED")), 4096).map(|s| s.trim().to_string());
        let trainer = stage_trainer(root, name);
        let progress = if trained.is_none() {
            progress(&reports.join(format!("{name}_progress.json")))
        } else {
            None
        };
        found.push(Stage {
            name: name.to_string(),
            trainer,
            log_path,
            modified,
            trained,
            progress,
        });
    }
    found.sort_by_key(|stage| std::cmp::Reverse(stage.modified));
    found
}

fn stage_trainer(root: &Path, name: &str) -> String {
    assert!(name_is_safe(name));
    if let Some(body) = read_text(&root.join("reports").join(format!("{name}_meta.json")), 64 << 10) {
        if let Ok(pairs) = flatten(&body) {
            if let Some(trainer) = text(&pairs, "trainer").filter(|trainer| !trainer.is_empty() && trainer.len() <= 200)
            {
                return trainer.into();
            }
        }
    }
    for candidate in [name.to_string(), name.replace('_', "-")] {
        for suffix in ["", ".work"] {
            for metadata_name in ["training.json", "plan.json"] {
                let path = root
                    .join("checkpoints")
                    .join(format!("{candidate}{suffix}"))
                    .join(metadata_name);
                let Some(body) = read_text(&path, METADATA_BYTES_MAX) else {
                    continue;
                };
                let Ok(pairs) = flatten(&body) else { continue };
                match text(&pairs, "trainer") {
                    Some(
                        "tinygrad" | "readout.train_stage" | "readout.pretrain_engram" | "tinygrad_readout.train_stage",
                    ) => {
                        return "tinygrad".into();
                    }
                    Some("torch" | "pytorch") => return "torch".into(),
                    Some(trainer) if !trainer.is_empty() && trainer.len() <= 200 => return trainer.to_string(),
                    _ => {}
                }
            }
        }
    }
    "unknown".into()
}

pub fn read_text(path: &Path, bytes_max: u64) -> Option<String> {
    assert!(bytes_max <= LOG_BYTES_MAX);
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > bytes_max {
        return None;
    }
    let mut body = String::new();
    file.take(bytes_max + 1).read_to_string(&mut body).ok()?;
    (body.len() as u64 <= bytes_max).then_some(body)
}

pub fn progress(path: &Path) -> Option<Progress> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > 4096 {
        return None;
    }
    let pairs = flatten(&read_text(path, 4096)?).ok()?;
    let phase = text(&pairs, "phase")?;
    if !matches!(phase, "load" | "step" | "checkpoint" | "finalize" | "evaluate") {
        return None;
    }
    let step = number(&pairs, "step")?;
    let pid = number(&pairs, "pid")?;
    if !step.is_finite() || step < 0.0 || step.fract() != 0.0 || step > u32::MAX as f64 {
        return None;
    }
    if !pid.is_finite() || pid < 1.0 || pid.fract() != 0.0 || pid > u32::MAX as f64 {
        return None;
    }
    Some(Progress {
        phase: phase.to_string(),
        step: step as u64,
        pid: pid as u32,
        modified: metadata.modified().ok()?,
    })
}

pub fn stage(root: &Path, name: &str) -> Option<Stage> {
    assert!(name_is_safe(name));
    stages(root).into_iter().find(|stage| stage.name == name)
}

fn numeric_record(line: &str) -> Option<Record> {
    if !line.trim_start().starts_with('{') || line.len() > 64 << 10 {
        return None;
    }
    let pairs = flatten(line).ok()?;
    if text(&pairs, "monitor").is_some() {
        return None;
    }
    let numbers = pairs.into_iter().filter_map(|(path, scalar)| match scalar {
        Scalar::Number(value) if value.is_finite() => Some((path, value)),
        _ => None,
    });
    Some(Record(numbers.collect()))
}

/// The last `bytes_max` bytes of a file as text, starting at a line boundary.
pub fn tail(path: &Path, bytes_max: u64) -> Option<String> {
    assert!(bytes_max <= LOG_BYTES_MAX);
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let size = metadata.len();
    let start = size.saturating_sub(bytes_max);
    let mut boundary = true;
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1)).ok()?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous).ok()?;
        boundary = previous[0] == b'\n';
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(bytes_max).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if boundary {
        return Some(text);
    }
    let first_newline = text.find('\n')?;
    Some(text[first_newline + 1..].to_string())
}

/// Training records of a stage in step order. A resumed run appends, so a step can appear twice: the last wins.
pub fn training_records(stage: &Stage) -> Vec<Record> {
    let Some(log) = stage.log_path.as_deref().and_then(|path| tail(path, LOG_BYTES_MAX)) else {
        return Vec::new();
    };
    let mut records: Vec<Record> = Vec::new();
    for line in log.lines().rev() {
        if records.len() == RECORDS_MAX {
            break;
        }
        let Some(record) = numeric_record(line) else { continue };
        let Some(step) = record.get("step") else { continue };
        if step < 0.0 || step.fract() != 0.0 {
            continue;
        }
        if records
            .last()
            .and_then(|last| last.get("step"))
            .is_some_and(|last| last <= step)
        {
            continue;
        }
        records.push(record);
    }
    records.reverse();
    records
}

pub fn last_record(stage: &Stage) -> Option<Record> {
    let log = tail(stage.log_path.as_deref()?, 256 << 10)?;
    log.lines().rev().filter_map(numeric_record).find(|record| {
        record
            .get("step")
            .is_some_and(|step| step >= 0.0 && step.fract() == 0.0)
    })
}

pub fn training_time(root: &Path, stage: &Stage, last: Option<&Record>) -> Option<TrainingTime> {
    if stage.trained.as_deref() == Some("train exit 0") {
        let path = root.join("checkpoints").join(&stage.name).join("training.json");
        if let Some(body) = read_text(&path, 64 << 10) {
            if let Ok(pairs) = flatten(&body) {
                if let Some(seconds) = number(&pairs, "seconds") {
                    if seconds.is_finite() && (0.0..=4_000_000_000.0).contains(&seconds) {
                        return Some(TrainingTime::Final(seconds.round() as u64));
                    }
                }
            }
        }
    }
    let seconds = last?.get("seconds")?;
    if !seconds.is_finite() || !(0.0..=4_000_000_000.0).contains(&seconds) {
        return None;
    }
    Some(TrainingTime::Recorded(seconds.round() as u64))
}

pub fn monitor_events(stage: &Stage) -> Vec<MonitorEvent> {
    let Some(log) = stage.log_path.as_deref().and_then(|path| tail(path, 256 << 10)) else {
        return Vec::new();
    };
    log.lines()
        .rev()
        .filter_map(|line| {
            let pairs = flatten(line).ok()?;
            let kind = text(&pairs, "monitor")?.to_string();
            let seconds = number(&pairs, "time")?;
            if !seconds.is_finite() || !(0.0..=4_000_000_000.0).contains(&seconds) {
                return None;
            }
            Some(MonitorEvent {
                kind,
                time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64),
                phase: text(&pairs, "phase").map(str::to_string),
                step: number(&pairs, "step").map(|value| value as u64),
                idle_seconds: number(&pairs, "idle_seconds").map(|value| value as u64),
                exit_code: number(&pairs, "exit_code").map(|value| value as i32),
            })
        })
        .take(16)
        .collect()
}

fn metrics(pairs: &[(String, Scalar)], prefix: &str) -> Option<Metrics> {
    Some(Metrics {
        accuracy: number(pairs, &format!("{prefix}.accuracy"))?,
        nll: number(pairs, &format!("{prefix}.nll"))?,
        brier: number(pairs, &format!("{prefix}.brier"))?,
    })
}

pub fn panel_scores(root: &Path, system: &str) -> Vec<PanelScore> {
    assert!(name_is_safe(system));
    let reports = root.join("reports");
    let suffix = format!("_{system}{PAIRED_SUFFIX}");
    let mut scores = Vec::new();
    for file in file_names(&reports) {
        let Some(panel) = file.strip_suffix(&suffix) else {
            continue;
        };
        if scores.len() == 128 {
            break;
        }
        let Some(body) = read_text(&reports.join(&file), METADATA_BYTES_MAX) else {
            continue;
        };
        let Ok(pairs) = flatten(&body) else { continue };
        let (Some(system), Some(jev)) = (metrics(&pairs, "all.candidate"), metrics(&pairs, "all.reference")) else {
            continue;
        };
        scores.push(PanelScore {
            panel: panel.to_string(),
            system,
            jev,
        });
    }
    scores.sort_by(|a, b| a.panel.cmp(&b.panel));
    scores
}

pub fn jevbench_scores(root: &Path, system: &str) -> Vec<TierScore> {
    assert!(name_is_safe(system));
    let mut scores = Vec::new();
    for tier in TIERS {
        let path = root
            .join("reports/jevbench")
            .join(system)
            .join(tier)
            .join("summary.json");
        let Some(body) = read_text(&path, METADATA_BYTES_MAX) else {
            continue;
        };
        let Ok(pairs) = flatten(&body) else { continue };
        let Some(accuracy) = number(&pairs, "accuracy") else {
            continue;
        };
        scores.push(TierScore {
            tier,
            accuracy,
            ece: number(&pairs, "ece.ece").unwrap_or(f64::NAN),
            brier: number(&pairs, "brier_mean").unwrap_or(f64::NAN),
            coverage: number(&pairs, "coverage").unwrap_or(f64::NAN),
            complete: pairs
                .iter()
                .any(|(path, scalar)| path == "complete" && *scalar == Scalar::Bool(true)),
        });
    }
    scores
}

/// The most recent finished queue tasks, newest first.
pub fn jobs(path: &Path, count: usize) -> Vec<Job> {
    let Some(log) = tail(path, 4 << 20) else {
        return Vec::new();
    };
    let mut jobs = Vec::new();
    for line in log.lines().rev() {
        if jobs.len() == count.min(RECORDS_MAX) {
            break;
        }
        let Ok(pairs) = flatten(line) else { continue };
        let Some(id) = number(&pairs, "id") else { continue };
        if !id.is_finite() || id < 0.0 || id.fract() != 0.0 || text(&pairs, "outcome").is_none() {
            continue;
        }
        let field = |key: &str| text(&pairs, key).unwrap_or("").to_string();
        let faults = pairs
            .iter()
            .filter_map(|(path, scalar)| match (path.strip_prefix("faults."), scalar) {
                (Some(kind), Scalar::Number(count)) => Some((kind.to_string(), *count)),
                _ => None,
            });
        jobs.push(Job {
            id,
            label: field("label"),
            outcome: field("outcome"),
            start: field("start"),
            end: field("end"),
            faults: faults.collect(),
        });
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "runboard-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("reports/jevbench/stage-a/hard")).unwrap();
        fs::create_dir_all(root.join("checkpoints/stage-a.work")).unwrap();
        fs::write(
            root.join("checkpoints/stage-a.work/training.json"),
            r#"{"trainer":"readout.train_stage"}"#,
        )
        .unwrap();
        let log = "noise\n{\"step\": 20, \"loss\": 1.5, \"x/y\": 2}\n{\"step\": 40, \"loss\": 1.0}\nresumed\n\
                   {\"step\": 40, \"loss\": 0.9}\n{\"step\": 60, \"loss\": NaN}\n";
        fs::write(root.join("reports/stage-a_train.log"), log).unwrap();
        fs::write(root.join("reports/stage-a_TRAINED"), "train exit 0\n").unwrap();
        fs::write(root.join("reports/stage-b_train.log"), "").unwrap();
        let paired = r#"{"all": {"candidate": {"accuracy": 0.5, "nll": 1, "brier": 0.4},
                                 "reference": {"accuracy": 0.9, "nll": 0.5, "brier": 0.1}}}"#;
        fs::write(root.join("reports/blind_stage-a_paired.json"), paired).unwrap();
        fs::write(root.join("reports/blind_stage-a-oa_paired.json"), paired).unwrap();
        let summary = r#"{"accuracy": 0.6, "ece": {"ece": 0.2}, "brier_mean": 0.3, "coverage": 1.0, "complete": true}"#;
        fs::write(root.join("reports/jevbench/stage-a/hard/summary.json"), summary).unwrap();
        let job =
            r#"{"id": 7, "label": "x", "outcome": "Failed", "start": "s", "end": "e", "faults": {"MMU fault": 2}}"#;
        fs::write(root.join("reports/jobs.jsonl"), format!("{job}\n")).unwrap();
        root
    }

    #[test]
    fn reads_trainer_metadata_for_underscored_text_run_names() {
        let root = fixture();
        fs::create_dir_all(root.join("checkpoints/source-prose-pilot-engram.work")).unwrap();
        fs::write(root.join("reports/source_prose_pilot_engram_train.log"), "").unwrap();
        fs::write(
            root.join("checkpoints/source-prose-pilot-engram.work/training.json"),
            r#"{"trainer":"readout.pretrain_engram"}"#,
        )
        .unwrap();
        assert_eq!(stage(&root, "source_prose_pilot_engram").unwrap().trainer, "tinygrad");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_or_invalid_metadata_does_not_imply_torch() {
        let root = fixture();
        let path = root.join("checkpoints/stage-b.work");
        fs::create_dir_all(&path).unwrap();
        assert_eq!(stage(&root, "stage-b").unwrap().trainer, "unknown");
        for body in ["broken", r#"{"trainer":null}"#] {
            fs::write(path.join("training.json"), body).unwrap();
            assert_eq!(stage(&root, "stage-b").unwrap().trainer, "unknown");
        }
        fs::remove_file(path.join("training.json")).unwrap();
        fs::write(path.join("plan.json"), r#"{"trainer":"readout.pretrain_engram"}"#).unwrap();
        assert_eq!(stage(&root, "stage-b").unwrap().trainer, "tinygrad");
        fs::write(path.join("training.json"), r#"{"trainer":"pytorch"}"#).unwrap();
        assert_eq!(stage(&root, "stage-b").unwrap().trainer, "torch");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_what_jobs_leave() {
        let root = fixture();
        let found = stages(&root);
        assert_eq!(found.len(), 2);
        let a = stage(&root, "stage-a").unwrap();
        assert_eq!(a.trainer, "tinygrad");
        assert_eq!(a.trained.as_deref(), Some("train exit 0"));
        assert_eq!(stage(&root, "stage-b").unwrap().trainer, "unknown");
        let records = training_records(&a);
        let steps: Vec<f64> = records.iter().map(|record| record.get("step").unwrap()).collect();
        assert_eq!(steps, [20.0, 40.0, 60.0]);
        assert_eq!(records[1].get("loss"), Some(0.9));
        assert_eq!(records[2].get("loss"), None);
        assert_eq!(records[0].get("x/y"), Some(2.0));
        let panels = panel_scores(&root, "stage-a");
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].panel, "blind");
        assert_eq!(panels[0].jev.accuracy, 0.9);
        assert_eq!(panel_scores(&root, "stage-a-oa").len(), 1);
        let tiers = jevbench_scores(&root, "stage-a");
        assert_eq!(tiers.len(), 1);
        assert_eq!((tiers[0].tier, tiers[0].ece, tiers[0].complete), ("hard", 0.2, true));
        let jobs = jobs(&root.join("reports/jobs.jsonl"), 10);
        assert_eq!(jobs[0].faults, [("MMU fault".to_string(), 2.0)]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_unsafe_names() {
        for name in ["", "..", "a/b", ".hidden", "a b", "a%2f"] {
            assert!(!name_is_safe(name), "{name}");
        }
        assert!(name_is_safe("stage-q-qwen35-4b-lora-tinygrad"));
    }

    #[test]
    fn reads_bounded_training_progress() {
        let path = std::env::temp_dir().join(format!("runboard-progress-{}.json", std::process::id()));
        fs::write(
            &path,
            format!(r#"{{"pid":{},"phase":"step","step":41}}"#, std::process::id()),
        )
        .unwrap();
        let found = progress(&path).unwrap();
        assert_eq!(found.step, 41);
        assert_eq!(found.phase, "step");
        assert_eq!(found.pid, std::process::id());
        fs::write(&path, r#"{"pid":1,"phase":"unknown","step":41}"#).unwrap();
        assert!(progress(&path).is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_recent_monitor_events() {
        let root = fixture();
        let path = root.join("reports/stage-a_train.log");
        let mut log = fs::read_to_string(&path).unwrap();
        log.push_str(
            "{\"monitor\":\"slow\",\"time\":1700000000,\"phase\":\"step\",\"step\":41,\"idle_seconds\":300}\n",
        );
        log.push_str("{\"monitor\":\"exit\",\"time\":1700000001,\"exit_code\":-15}\n");
        fs::write(&path, log).unwrap();
        let found = monitor_events(&stage(&root, "stage-a").unwrap());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, "exit");
        assert_eq!(found[0].exit_code, Some(-15));
        assert_eq!(found[1].step, Some(41));
        assert_eq!(found[1].idle_seconds, Some(300));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefers_final_training_time_over_last_logged_time() {
        let root = fixture();
        let stage = stage(&root, "stage-a").unwrap();
        let logged = Record(vec![("seconds".into(), 61.0)]);
        assert_eq!(
            training_time(&root, &stage, Some(&logged)),
            Some(TrainingTime::Recorded(61))
        );
        let checkpoint = root.join("checkpoints/stage-a");
        fs::create_dir_all(&checkpoint).unwrap();
        fs::write(checkpoint.join("training.json"), r#"{"seconds":125.7}"#).unwrap();
        assert_eq!(
            training_time(&root, &stage, Some(&logged)),
            Some(TrainingTime::Final(126))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_custom_metrics_and_ignores_partial_appends() {
        let root = fixture();
        fs::write(
            root.join("reports/stage-b_train.log"),
            " {\"loss\":2,\"step\":10}\n{\"step\":20,\"validation\":{\"f1\":0.8}}\n{\"monitor\":\"stall\",\"time\":1,\"step\":20}\n{\"step\":",
        )
        .unwrap();
        let stage = stage(&root, "stage-b").unwrap();
        assert_eq!(last_record(&stage).unwrap().get("validation.f1"), Some(0.8));
        assert_eq!(training_records(&stage).len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resumed_runs_replace_abandoned_future_steps() {
        let root = fixture();
        fs::write(
            root.join("reports/stage-b_train.log"),
            "{\"step\":1}\n{\"step\":2}\n{\"step\":3}\n{\"step\":2,\"loss\":0.5}\n",
        )
        .unwrap();
        let records = training_records(&stage(&root, "stage-b").unwrap());
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].get("loss"), Some(0.5));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_reads_and_tails_handle_boundaries() {
        let root = fixture();
        let path = root.join("reports/example.log");
        fs::write(&path, "abc\ndef\n").unwrap();
        assert_eq!(tail(&path, 4).as_deref(), Some("def\n"));
        assert_eq!(tail(&path, 5).as_deref(), Some("def\n"));
        assert!(read_text(&path, 7).is_none());
        assert_eq!(read_text(&path, 8).as_deref(), Some("abc\ndef\n"));
        assert!(read_text(&root, 4096).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_callback_records_survive_partial_appends() {
        let root = fixture();
        let path = root.join("reports/jobs.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"id\":7,\"label\":\"run\",\"group\":\"gpu\",\"outcome\":\"finished-with-faults\",",
                "\"result\":\"Success\",\"faults\":{\"MMU fault\":2},\"command\":\"private\",\"path\":\"private\"}\n",
                "{\"id\":8"
            ),
        )
        .unwrap();
        let found = jobs(&path, 1);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].outcome, "finished-with-faults");
        assert_eq!(found[0].faults, [("MMU fault".into(), 2.0)]);
        fs::remove_dir_all(root).unwrap();
    }
}
