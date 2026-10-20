use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 1 {
        return Err("usage: cargo run --example demo -- <new directory>".into());
    }
    let root = PathBuf::from(&arguments[0]);
    if root.exists() {
        return Err("demo directory already exists; choose a new directory".into());
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    fs::create_dir_all(root.join("reports"))?;
    for (name, improvement, outcome) in [
        ("baseline", 0.0, Some("train exit 0")),
        ("candidate", 0.25, Some("train exit 0")),
        ("larger-batch", 0.1, None),
        ("memory-limit", 0.0, Some("train exit 1")),
    ] {
        let mut log = fs::File::create(root.join(format!("reports/{name}_train.log")))?;
        let steps = if outcome == Some("train exit 1") { 12u32 } else { 80 };
        for index in 1..=steps {
            let step = index * 25;
            let progress = f64::from(index) / 80.0;
            let loss = 2.4 * (-progress * 3.0).exp() + 0.45 - improvement * progress;
            let jitter = (f64::from(index) * 1.8).sin() * 0.035;
            writeln!(
                log,
                "{{\"step\":{step},\"loss\":{:.4},\"seconds\":{},\"lr\":0.0003,\"validation/f1\":{:.4},\
                 \"train/tokens_per_second\":{:.0},\"train/step_seconds_mean\":1.8,\"time\":{}}}",
                loss + jitter,
                index * 45,
                0.55 + progress * (0.22 + improvement * 0.2),
                12500.0 + jitter * 5000.0 + improvement * 2000.0,
                now.saturating_sub(u64::from(steps - index) * 45),
            )?;
        }
        if let Some(outcome) = outcome {
            fs::write(root.join(format!("reports/{name}_TRAINED")), outcome)?;
        } else {
            fs::write(
                root.join(format!("reports/{name}_progress.json")),
                format!("{{\"phase\":\"step\",\"step\":{},\"pid\":1234}}", steps * 25),
            )?;
        }
        fs::create_dir_all(root.join(format!("checkpoints/{name}")))?;
        fs::write(
            root.join(format!("checkpoints/{name}/training.json")),
            format!("{{\"trainer\":\"example-trainer\",\"seconds\":{}}}", steps * 45),
        )?;
        let group = if matches!(name, "baseline" | "candidate") {
            "matched-seeds"
        } else {
            name
        };
        let seed = if name == "candidate" { 2 } else { 1 };
        let task = match name {
            "baseline" => 101,
            "candidate" => 102,
            "memory-limit" => 103,
            _ => 105,
        };
        fs::write(
            root.join(format!("reports/{name}_meta.json")),
            format!(
                r#"{{"group":"{group}","seed":{seed},"revision":"demo","dataset":"synthetic","parameters":{{"lr":0.0003}},"tags":["example"],"task_id":{task}}}"#
            ),
        )?;
        fs::write(
            root.join(format!("reports/{name}_summary.json")),
            format!(
                r#"{{"metrics":{{"accuracy":{{"value":{},"unit":"fraction","direction":"higher"}},"latency":{{"value":12.5,"unit":"ms","direction":"lower"}}}}}}"#,
                0.77 + improvement * 0.1
            ),
        )?;
        fs::create_dir_all(root.join(format!("artifacts/{name}")))?;
        fs::write(
            root.join(format!("artifacts/{name}/evaluation.json")),
            r#"{"examples":1000,"source":"synthetic demo"}"#,
        )?;
    }
    let mut telemetry = fs::File::create(root.join("reports/device_telemetry.jsonl"))?;
    for index in 0..240u32 {
        let wave = (f64::from(index) / 14.0).sin();
        writeln!(
            telemetry,
            "{{\"time\":{},\"device/gpu_junction_celsius\":{:.1},\"device/gpu_mem_celsius\":72,\
             \"device/gpu_power_watts\":{:.1},\"device/gpu_busy_percent\":97,\"device/vram_used_gib\":18.5,\
             \"device/vram_total_gib\":24,\"host/memory_available_gib\":42,\"host/swap_used_gib\":0,\"host\":\"demo\",\"device\":\"gpu0\"}}",
            now.saturating_sub(u64::from(239 - index) * 15),
            79.0 + wave * 4.0,
            250.0 + wave * 25.0,
        )?;
    }
    let jobs = [
        r#"{"id":101,"label":"baseline","group":"gpu","outcome":"Success","start":"2026-01-01T09:00:00Z","end":"2026-01-01T10:00:00Z","faults":{}}"#,
        r#"{"id":102,"label":"candidate","group":"gpu","outcome":"Success","start":"2026-01-01T10:00:00Z","end":"2026-01-01T11:00:00Z","faults":{}}"#,
        r#"{"id":103,"label":"memory-limit","group":"gpu","outcome":"Failed","start":"2026-01-01T11:00:00Z","end":"2026-01-01T11:09:00Z","faults":{"out of memory":1}}"#,
        r#"{"id":104,"label":"evaluate","group":"gpu","outcome":"finished-with-faults","start":"2026-01-01T11:10:00Z","end":"2026-01-01T11:20:00Z","faults":{"Traceback":1}}"#,
    ];
    fs::write(root.join("reports/jobs.jsonl"), format!("{}\n", jobs.join("\n")))?;
    fs::write(
        root.join("reports/queue.json"),
        r#"{"tasks":{"104":{"label":"evaluate","group":"gpu","status":{"Done":{"result":"Success"}}},"105":{"label":"larger-batch","group":"gpu","status":{"Running":{"start":"2026-01-01T11:20:00Z"}},"dependencies":[104]},"106":{"label":"next-evaluation","group":"gpu","status":"Queued","dependencies":[105]}}}"#,
    )?;
    writeln!(io::stdout(), "Demo written to {}", root.display())?;
    Ok(())
}
