mod app;
mod data;
mod html;
mod inputs;
mod json;
mod live;
mod queue;
mod server;

use app::{Project, Settings};
use std::path::PathBuf;
use std::sync::Mutex;

const USAGE: &str = "runboard --root [name=]<directory> [--root ...] [--listen address:port]\n\
    [--jobs history.jsonl] [--queue snapshot.json | --pueue config.yml] [--artifacts]\n\
    [--state-dir directory] [--nats-port port | --nats-url nats://server:port]\n\
    [--export report.html --select project/run [--select ...]]";

#[tokio::main(worker_threads = 2)]
async fn main() {
    let values: Vec<String> = std::env::args().skip(1).collect();
    if values == ["--help"] {
        println!(
            "{USAGE}\n\nDefault listen: 127.0.0.1:5000. Maximum 16 projects, 8 selected runs.\n\
            A bundled NATS sidecar starts on 127.0.0.1:4223 unless --nats-url is supplied.\n\
            State defaults to $XDG_STATE_HOME/runboard/<project-scope> or ~/.local/state/runboard/<project-scope>.\n\
            --pueue - uses Pueue's default config. Artifacts require --artifacts.\n\
            --export writes selected runs to a new static HTML file without starting NATS."
        );
        return;
    }
    let settings = match arguments(values) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("runboard: {error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let result = if settings.export.is_some() {
        app::export(&settings)
    } else {
        server::run(settings).await
    };
    if let Err(error) = result {
        eprintln!("runboard: {error}");
        std::process::exit(1);
    }
}

fn arguments(arguments: Vec<String>) -> Result<Settings, String> {
    let mut projects = Vec::<Project>::new();
    let mut listen = None;
    let mut jobs = None;
    let mut queue = None;
    let mut pueue = None;
    let mut artifacts = false;
    let mut export = None;
    let mut selected = Vec::new();
    let mut state_dir = None;
    let mut nats_url = None;
    let mut nats_port = None;
    let mut values = arguments.into_iter();
    while let Some(flag) = values.next() {
        if flag == "--artifacts" {
            artifacts = true;
            continue;
        }
        let value = values.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--root" => {
                if projects.len() == 16 {
                    return Err("at most 16 project roots are allowed".into());
                }
                let (name, path) = value
                    .split_once('=')
                    .map(|(name, path)| (Some(name), path))
                    .unwrap_or((None, value.as_str()));
                let root = PathBuf::from(path)
                    .canonicalize()
                    .map_err(|error| format!("{path}: {error}"))?;
                if !root.join("reports").is_dir() {
                    return Err(format!("{} has no reports directory", root.display()));
                }
                let name = name
                    .or_else(|| root.file_name().and_then(|name| name.to_str()))
                    .ok_or("name this root with name=path")?;
                if !data::name_is_safe(name) {
                    return Err("invalid project name; use --root name=path".into());
                }
                if projects.iter().any(|project| project.name == name) {
                    return Err(format!("duplicate project name {name}"));
                }
                projects.push(Project {
                    name: name.into(),
                    root,
                });
            }
            "--listen" if listen.is_none() => listen = Some(value),
            "--jobs" if jobs.is_none() => jobs = Some(PathBuf::from(value)),
            "--queue" if queue.is_none() => queue = Some(PathBuf::from(value)),
            "--pueue" if pueue.is_none() => pueue = Some(PathBuf::from(value)),
            "--export" if export.is_none() => export = Some(PathBuf::from(value)),
            "--select" if selected.len() < 8 => selected.push(value),
            "--state-dir" if state_dir.is_none() => state_dir = Some(PathBuf::from(value)),
            "--nats-url" if nats_url.is_none() => {
                if value.len() > 2048
                    || !matches!(value.split_once("://").map(|(scheme, _)| scheme), Some("nats" | "tls"))
                {
                    return Err("--nats-url must be a nats:// or tls:// endpoint".into());
                }
                nats_url = Some(value);
            }
            "--nats-port" if nats_port.is_none() => {
                let port = value.parse::<u16>().map_err(|_| "invalid NATS port")?;
                if port == 0 {
                    return Err("NATS port must be between 1 and 65535".into());
                }
                nats_port = Some(port);
            }
            _ => return Err(format!("unknown, duplicate or excess argument {flag}")),
        }
    }
    if projects.is_empty() {
        return Err("--root is required".into());
    }
    if queue.is_some() && pueue.is_some() {
        return Err("choose --queue or --pueue".into());
    }
    if nats_url.is_some() && nats_port.is_some() {
        return Err("choose --nats-url or --nats-port".into());
    }
    if export.is_some() && selected.is_empty() {
        return Err("--export requires an explicit --select project/run".into());
    }
    if export.is_none() && !selected.is_empty() {
        return Err("--select requires --export".into());
    }
    Ok(Settings {
        live: live::Options {
            state_dir,
            url: nats_url,
            port: nats_port.unwrap_or(4223),
        },
        projects,
        listen: listen.unwrap_or_else(|| "127.0.0.1:5000".into()),
        jobs,
        queue,
        pueue,
        artifacts,
        export,
        selected,
        queue_cache: Mutex::new(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_named_roots_and_explicit_exports() {
        let root = std::env::temp_dir().join(format!("runboard-arguments-{}", std::process::id()));
        std::fs::create_dir_all(root.join("reports")).unwrap();
        let named = format!("a={}", root.display());
        let settings = arguments(vec![
            "--root".into(),
            named.clone(),
            "--root".into(),
            format!("b={}", root.display()),
            "--artifacts".into(),
        ])
        .unwrap();
        assert_eq!(settings.projects.len(), 2);
        assert!(settings.artifacts);
        assert_eq!(settings.listen, "127.0.0.1:5000");
        assert_eq!(settings.live.port, 4223);
        for extra in [
            vec!["--root", &named],
            vec!["--export", "report.html"],
            vec!["--queue", "queue.json", "--pueue", "-"],
            vec!["--select", "a/run"],
            vec!["--nats-port", "0"],
            vec!["--nats-port", "4223", "--nats-url", "nats://localhost:4222"],
            vec!["--nats-url", "http://localhost"],
        ] {
            let mut values = vec!["--root".to_string(), named.clone()];
            values.extend(extra.iter().map(|value| value.to_string()));
            assert!(arguments(values).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
