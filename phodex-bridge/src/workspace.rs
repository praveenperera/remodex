use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use color_eyre::eyre::{eyre, Result};
use serde_json::{json, Value};
use uuid::Uuid;

static REPO_MUTATION_LOCKS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

pub async fn handle_workspace_request(method: &str, params: &Value) -> Result<Option<Value>> {
    if !method.starts_with("workspace/") {
        return Ok(None);
    }

    let cwd = resolve_workspace_cwd(params)?;
    let repo_root = resolve_repo_root(&cwd)?;

    let result = match method {
        "workspace/revertPatchPreview" => workspace_revert_patch_preview(&repo_root, params)?,
        "workspace/revertPatchApply" => workspace_revert_patch_apply(&repo_root, params)?,
        other => return Err(eyre!("Unknown workspace method: {other}")),
    };

    Ok(Some(result))
}

fn workspace_revert_patch_preview(repo_root: &Path, params: &Value) -> Result<Value> {
    let forward_patch = resolve_forward_patch(params)?;
    let analysis = analyze_unified_patch(&forward_patch);
    let staged_files = find_staged_targeted_files(repo_root, &analysis.affected_files)?;

    if !analysis.unsupported_reasons.is_empty() || !staged_files.is_empty() {
        return Ok(json!({
            "canRevert": false,
            "affectedFiles": analysis.affected_files,
            "conflicts": [],
            "unsupportedReasons": analysis.unsupported_reasons,
            "stagedFiles": staged_files,
        }));
    }

    let apply_check = run_git_apply(
        repo_root,
        &["apply", "--reverse", "--check"],
        &forward_patch,
    )?;
    let conflicts = if apply_check.0 {
        Vec::new()
    } else {
        parse_apply_conflicts(&format!("{}{}", apply_check.1, apply_check.2))
    };

    Ok(json!({
        "canRevert": apply_check.0 && conflicts.is_empty(),
        "affectedFiles": analysis.affected_files,
        "conflicts": conflicts,
        "unsupportedReasons": [],
        "stagedFiles": staged_files,
    }))
}

fn workspace_revert_patch_apply(repo_root: &Path, params: &Value) -> Result<Value> {
    let _guard = acquire_repo_mutation_lock(repo_root)?;
    let preview = workspace_revert_patch_preview(repo_root, params)?;
    if !preview
        .get("canRevert")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(json!({
            "success": false,
            "revertedFiles": [],
            "conflicts": preview.get("conflicts").cloned().unwrap_or_else(|| json!([])),
            "unsupportedReasons": preview.get("unsupportedReasons").cloned().unwrap_or_else(|| json!([])),
            "stagedFiles": preview.get("stagedFiles").cloned().unwrap_or_else(|| json!([])),
        }));
    }

    let forward_patch = resolve_forward_patch(params)?;
    let apply_result = run_git_apply(repo_root, &["apply", "--reverse"], &forward_patch)?;
    if !apply_result.0 {
        return Ok(json!({
            "success": false,
            "revertedFiles": [],
            "conflicts": parse_apply_conflicts(&format!("{}{}", apply_result.1, apply_result.2)),
            "unsupportedReasons": [],
            "stagedFiles": [],
            "status": crate::git_handler::git_status(repo_root).ok(),
        }));
    }

    Ok(json!({
        "success": true,
        "revertedFiles": preview.get("affectedFiles").cloned().unwrap_or_else(|| json!([])),
        "conflicts": [],
        "unsupportedReasons": [],
        "stagedFiles": [],
        "status": crate::git_handler::git_status(repo_root).ok(),
    }))
}

fn resolve_forward_patch(params: &Value) -> Result<String> {
    let forward_patch = params
        .get("forwardPatch")
        .and_then(Value::as_str)
        .map(str::trim_end)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| eyre!("The request must include a non-empty forwardPatch."))?;
    Ok(format!("{forward_patch}\n"))
}

fn analyze_unified_patch(raw_patch: &str) -> PatchAnalysis {
    let patch = raw_patch.trim();
    if patch.is_empty() {
        return PatchAnalysis {
            affected_files: Vec::new(),
            unsupported_reasons: vec!["No exact patch was captured.".to_owned()],
        };
    }

    let mut chunks = Vec::<Vec<String>>::new();
    let mut current = Vec::<String>::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            chunks.push(current);
            current = Vec::new();
        }
        current.push(line.to_owned());
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let mut affected_files = HashSet::new();
    let mut unsupported_reasons = HashSet::new();
    for chunk in chunks {
        let chunk_analysis = analyze_patch_chunk(&chunk);
        if let Some(path) = chunk_analysis.path {
            affected_files.insert(path);
        }
        for reason in chunk_analysis.unsupported_reasons {
            unsupported_reasons.insert(reason);
        }
    }

    if affected_files.is_empty() {
        unsupported_reasons.insert("No exact patch was captured.".to_owned());
    }

    let mut affected_files = affected_files.into_iter().collect::<Vec<_>>();
    affected_files.sort();
    let mut unsupported_reasons = unsupported_reasons.into_iter().collect::<Vec<_>>();
    unsupported_reasons.sort();

    PatchAnalysis {
        affected_files,
        unsupported_reasons,
    }
}

fn analyze_patch_chunk(lines: &[String]) -> PatchChunkAnalysis {
    let path = extract_patch_path(lines);
    let is_binary = lines
        .iter()
        .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch");
    let is_rename_or_mode_only = lines.iter().any(|line| {
        line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("copy from ")
            || line.starts_with("copy to ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("similarity index ")
            || line.starts_with("new file mode 120")
            || line.starts_with("deleted file mode 120")
    });

    let additions = lines
        .iter()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count();
    let deletions = lines
        .iter()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .count();

    let mut unsupported_reasons = Vec::new();
    if is_binary {
        unsupported_reasons.push("Binary changes are not auto-revertable in v1.".to_owned());
    }
    if is_rename_or_mode_only {
        unsupported_reasons.push(
            "Rename, mode-only, or symlink changes are not auto-revertable in v1.".to_owned(),
        );
    }
    if path.is_none()
        || (additions == 0
            && deletions == 0
            && !lines.iter().any(|line| line == "--- /dev/null")
            && !lines.iter().any(|line| line == "+++ /dev/null"))
            && !is_binary
            && !is_rename_or_mode_only
    {
        unsupported_reasons.push("No exact patch was captured.".to_owned());
    }

    PatchChunkAnalysis {
        path,
        unsupported_reasons,
    }
}

fn extract_patch_path(lines: &[String]) -> Option<String> {
    for line in lines {
        if let Some(path) = line.strip_prefix("+++ ") {
            let normalized = normalize_diff_path(path.trim());
            if normalized.as_deref() != Some("/dev/null") {
                return normalized;
            }
        }
    }

    for line in lines {
        if line.starts_with("diff --git ") {
            let components = line.split_whitespace().collect::<Vec<_>>();
            if components.len() >= 4 {
                return normalize_diff_path(components[3]);
            }
        }
    }

    None
}

fn normalize_diff_path(raw_path: &str) -> Option<String> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("a/") || trimmed.starts_with("b/") {
        Some(trimmed[2..].to_owned())
    } else {
        Some(trimmed.to_owned())
    }
}

fn find_staged_targeted_files(cwd: &Path, affected_files: &[String]) -> Result<Vec<String>> {
    if affected_files.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["diff", "--name-only", "--cached", "--"];
    args.extend(affected_files.iter().map(String::as_str));
    let output = git(cwd, &args)?;
    let mut files = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn run_git_apply(cwd: &Path, args: &[&str], patch_text: &str) -> Result<(bool, String, String)> {
    let temp_patch_path =
        std::env::temp_dir().join(format!("remodex-revert-{}.patch", Uuid::new_v4().simple()));
    fs::write(&temp_patch_path, patch_text)?;
    let output = Command::new("git")
        .args(args)
        .arg(&temp_patch_path)
        .current_dir(cwd)
        .output()?;
    let _ = fs::remove_file(temp_patch_path);
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    ))
}

fn parse_apply_conflicts(stderr: &str) -> Vec<Value> {
    let mut conflicts = Vec::new();
    for line in stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = line
            .strip_prefix("error: patch failed: ")
            .and_then(|value| value.split(':').next())
            .or_else(|| {
                line.strip_prefix("error: ")
                    .and_then(|value| value.split(": patch does not apply").next())
            })
            .unwrap_or("unknown");
        if !conflicts
            .iter()
            .any(|conflict: &Value| conflict.get("path").and_then(Value::as_str) == Some(path))
        {
            conflicts.push(json!({
                "path": path,
                "message": line,
            }));
        }
    }
    if conflicts.is_empty() && !stderr.trim().is_empty() {
        conflicts.push(json!({
            "path": "unknown",
            "message": stderr.split_whitespace().collect::<Vec<_>>().join(" "),
        }));
    }
    conflicts
}

fn resolve_workspace_cwd(params: &Value) -> Result<PathBuf> {
    let requested_cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("currentWorkingDirectory")
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| eyre!("Workspace actions require a bound local working directory."))?;
    let path = PathBuf::from(requested_cwd);
    if !path.is_dir() {
        return Err(eyre!(
            "The requested local working directory does not exist on this Mac."
        ));
    }
    Ok(path)
}

fn resolve_repo_root(cwd: &Path) -> Result<PathBuf> {
    let repo_root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let repo_root = repo_root.trim();
    if repo_root.is_empty() {
        return Err(eyre!(
            "The selected local folder is not inside a Git repository."
        ));
    }
    Ok(PathBuf::from(repo_root))
}

fn acquire_repo_mutation_lock(repo_root: &Path) -> Result<RepoMutationGuard> {
    let locks = REPO_MUTATION_LOCKS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = locks.lock().map_err(|_| eyre!("Workspace lock poisoned"))?;
    if guard.contains(repo_root) {
        return Err(eyre!(
            "Another workspace mutation is already running for this repository."
        ));
    }
    guard.insert(repo_root.to_path_buf());
    Ok(RepoMutationGuard {
        repo_root: repo_root.to_path_buf(),
    })
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Err(eyre!(
        "{}",
        if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "git command failed".to_owned()
        }
    ))
}

struct RepoMutationGuard {
    repo_root: PathBuf,
}

impl Drop for RepoMutationGuard {
    fn drop(&mut self) {
        if let Some(locks) = REPO_MUTATION_LOCKS.get() {
            if let Ok(mut guard) = locks.lock() {
                guard.remove(&self.repo_root);
            }
        }
    }
}

struct PatchAnalysis {
    affected_files: Vec<String>,
    unsupported_reasons: Vec<String>,
}

struct PatchChunkAnalysis {
    path: Option<String>,
    unsupported_reasons: Vec<String>,
}
