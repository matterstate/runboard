use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("Deploy: {error}");
            ExitCode::from(255)
        }
    }
}

fn run() -> Result<bool, String> {
    let state = PathBuf::from(environment("STATE_DIRECTORY")?);
    let mode = std::env::args().nth(1).ok_or("expected check, build or activate")?;
    match mode.as_str() {
        "check" => {
            let repository = environment("DEPLOY_REPOSITORY")?;
            let parts: Vec<&str> = repository.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|part| !identifier(part)) {
                return Err("invalid repository".into());
            }
            let branch = environment("DEPLOY_BRANCH")?;
            if !identifier(&branch) {
                return Err("invalid branch".into());
            }
            let head = output(Command::new("git").args([
                "ls-remote",
                "--exit-code",
                &format!("https://github.com/{repository}.git"),
                &format!("refs/heads/{branch}"),
            ]))?;
            let revision = head.split_whitespace().next().ok_or("missing remote revision")?;
            validate_revision(revision)?;
            match read_state(&state.join("deployed")) {
                Ok(deployed) if deployed == revision => return Ok(false),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
            let response = output(Command::new("curl").args([
                "--fail",
                "--silent",
                "--show-error",
                "--max-time",
                "20",
                "--max-filesize",
                "1048576",
                "--header",
                "Accept: application/vnd.github+json",
                &format!("https://api.github.com/repos/{repository}/actions/runs?head_sha={revision}&branch={branch}&event=push&per_page=20"),
            ]))?;
            let runs: Value = serde_json::from_str(&response).map_err(|error| error.to_string())?;
            if !ci_passed(&runs, &repository, &branch, revision) {
                eprintln!("Deploy: waiting for CI on {revision}");
                return Ok(false);
            }
            write_state(&state.join("target"), revision.as_bytes())?;
            Ok(true)
        }
        "build" => {
            let revision = read_state(&state.join("target")).map_err(|error| error.to_string())?;
            validate_revision(&revision)?;
            let repository = environment("DEPLOY_REPOSITORY")?;
            let release = state.join(format!("release-{revision}"));
            command(Command::new("nix").args([
                "build",
                "--no-write-lock-file",
                "--out-link",
                release.to_str().ok_or("invalid state path")?,
                &format!("github:{repository}/{revision}#default"),
            ]))?;
            let candidate = store_release(&release)?;
            let current = store_release(&state.join("current")).ok();
            let previous = store_release(&state.join("previous")).ok();
            let mut count = 0_u32;
            for entry in fs::read_dir(&state).map_err(|error| error.to_string())? {
                count += 1;
                if count > 128 {
                    return Err("too many deployment state entries".into());
                }
                let entry = entry.map_err(|error| error.to_string())?;
                if entry.file_name().to_string_lossy().starts_with("release-") {
                    let target = store_release(&entry.path())?;
                    if target != candidate && Some(&target) != current.as_ref() && Some(&target) != previous.as_ref() {
                        fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
                    }
                }
            }
            if let Some(current) = current {
                replace_link(&state.join("previous"), &current)?;
            }
            replace_link(&state.join("current"), &candidate)?;
            Ok(true)
        }
        "activate" => {
            let revision = read_state(&state.join("target")).map_err(|error| error.to_string())?;
            validate_revision(&revision)?;
            store_release(&state.join("current"))?;
            let health_url = environment("DEPLOY_HEALTH_URL")?;
            let restart = command(Command::new("systemctl").args(["restart", "runboard.service"]));
            if restart.is_ok() && healthy(&health_url) {
                write_state(&state.join("deployed"), revision.as_bytes())?;
                eprintln!("Deploy: activated {revision}");
                return Ok(true);
            }
            let previous = store_release(&state.join("previous"))?;
            replace_link(&state.join("current"), &previous)?;
            command(Command::new("systemctl").args(["restart", "runboard.service"]))?;
            if !healthy(&health_url) {
                return Err("deployment and rollback health checks failed".into());
            }
            Err(format!("{revision} failed health checks; restored previous release"))
        }
        _ => Err("expected check, build or activate".into()),
    }
}

fn environment(name: &str) -> Result<String, String> {
    let value = std::env::var(name).map_err(|_| format!("missing {name}"))?;
    if value.is_empty() || value.len() > 4096 {
        return Err(format!("invalid {name}"));
    }
    Ok(value)
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn validate_revision(revision: &str) -> Result<(), String> {
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid revision".into());
    }
    Ok(())
}

fn ci_passed(runs: &Value, repository: &str, branch: &str, revision: &str) -> bool {
    let Some(runs) = runs["workflow_runs"].as_array() else {
        return false;
    };
    let latest = runs.iter().take(20).find(|run| {
        run["path"] == ".github/workflows/ci.yml"
            && run["head_sha"] == revision
            && run["head_branch"] == branch
            && run["event"] == "push"
            && run["head_repository"]["full_name"] == repository
    });
    latest.is_some_and(|run| run["status"] == "completed" && run["conclusion"] == "success")
}

fn read_state(path: &Path) -> std::io::Result<String> {
    let mut value = String::new();
    fs::File::open(path)?.take(4097).read_to_string(&mut value)?;
    if value.len() > 4096 {
        return Err(std::io::Error::other("state file too large"));
    }
    Ok(value)
}

fn write_state(path: &Path, value: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("next");
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(value).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn replace_link(path: &Path, target: &Path) -> Result<(), String> {
    let temporary = path.with_extension("next");
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    symlink(target, &temporary).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn store_release(path: &Path) -> Result<PathBuf, String> {
    let target = fs::read_link(path).map_err(|error| error.to_string())?;
    if target.parent() != Some(Path::new("/nix/store")) || !target.join("bin/runboard").is_file() {
        return Err("release must contain /nix/store/<package>/bin/runboard".into());
    }
    Ok(target)
}

fn output(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "{} failed: {}",
            command.get_program().to_string_lossy(),
            output.status
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn command(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("{} failed: {status}", command.get_program().to_string_lossy()));
    }
    Ok(())
}

fn healthy(url: &str) -> bool {
    for _ in 0..20 {
        let response = Command::new("curl")
            .args([
                "--noproxy",
                "*",
                "--fail",
                "--silent",
                "--max-time",
                "2",
                "--max-filesize",
                "1024",
                url,
            ])
            .output();
        if let Ok(response) = response {
            if response.status.success() {
                if let Ok(value) = serde_json::from_slice::<Value>(&response.stdout) {
                    if value["status"] == "live" {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_requires_latest_successful_main_push() {
        let revision = "a".repeat(40);
        let run = serde_json::json!({"path":".github/workflows/ci.yml","head_sha":revision,"head_branch":"main","event":"push","head_repository":{"full_name":"example/runboard"},"status":"completed","conclusion":"success"});
        let mut runs = serde_json::json!({"workflow_runs":[run.clone()]});
        assert!(ci_passed(&runs, "example/runboard", "main", &revision));
        for (field, value) in [
            ("path", "other.yml"),
            ("head_sha", "wrong"),
            ("head_branch", "topic"),
            ("event", "pull_request"),
            ("status", "in_progress"),
            ("conclusion", "failure"),
        ] {
            runs["workflow_runs"][0] = run.clone();
            runs["workflow_runs"][0][field] = value.into();
            assert!(!ci_passed(&runs, "example/runboard", "main", &revision));
        }
        runs["workflow_runs"][0] = run.clone();
        runs["workflow_runs"][0]["head_repository"]["full_name"] = "other/runboard".into();
        assert!(!ci_passed(&runs, "example/runboard", "main", &revision));
        let mut latest = run.clone();
        latest["conclusion"] = "failure".into();
        runs["workflow_runs"] = serde_json::json!([latest, run]);
        assert!(!ci_passed(&runs, "example/runboard", "main", &revision));
    }

    #[test]
    fn rejects_invalid_targets() {
        assert!(validate_revision(&"a".repeat(40)).is_ok());
        assert!(validate_revision("main").is_err());
        assert!(!identifier("main/other"));
        assert!(!identifier(""));
        assert!(!identifier("main?rev=other"));
        assert!(store_release(Path::new("/bin/sh")).is_err());
    }

    #[test]
    fn interrupted_state_writes_and_release_switches_recover() {
        let directory = std::env::temp_dir().join(format!("runboard-deploy-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let marker = directory.join("deployed");
        fs::write(marker.with_extension("next"), "interrupted").unwrap();
        write_state(&marker, b"good").unwrap();
        assert_eq!(read_state(&marker).unwrap(), "good");
        let current = directory.join("current");
        replace_link(&current, Path::new("first")).unwrap();
        symlink("interrupted", current.with_extension("next")).unwrap();
        replace_link(&current, Path::new("second")).unwrap();
        assert_eq!(fs::read_link(&current).unwrap(), Path::new("second"));
        replace_link(&current, Path::new("first")).unwrap();
        assert_eq!(fs::read_link(&current).unwrap(), Path::new("first"));
        assert!(!current.with_extension("next").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
