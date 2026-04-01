use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{eyre, Result};
use serde_json::{json, Value};
use uuid::Uuid;

const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

pub async fn handle_git_request(method: &str, params: &Value) -> Result<Option<Value>> {
    if !method.starts_with("git/") {
        return Ok(None);
    }

    let cwd = resolve_git_cwd(params)?;
    let result = match method {
        "git/status" => git_status(&cwd)?,
        "git/diff" => git_diff(&cwd)?,
        "git/commit" => git_commit(&cwd, params)?,
        "git/push" => git_push(&cwd)?,
        "git/pull" => git_pull(&cwd)?,
        "git/branches" => git_branches(&cwd)?,
        "git/checkout" => git_checkout(&cwd, params)?,
        "git/createBranch" => git_create_branch(&cwd, params)?,
        "git/createWorktree" => git_create_worktree(&cwd, params)?,
        "git/removeWorktree" => git_remove_worktree(&cwd, params)?,
        "git/resetToRemote" => git_reset_to_remote(&cwd, params)?,
        "git/remoteUrl" => git_remote_url(&cwd)?,
        "git/branchesWithStatus" => git_branches_with_status(&cwd)?,
        other => return Err(eyre!("Unknown git method: {other}")),
    };

    Ok(Some(result))
}

pub fn git_status(cwd: &Path) -> Result<Value> {
    let porcelain = git(cwd, ["status", "--porcelain=v1", "-b"])?;
    let lines: Vec<&str> = porcelain
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let branch_line = lines.first().copied().unwrap_or("");
    let file_lines = lines.iter().skip(1).copied().collect::<Vec<_>>();
    let branch = parse_branch_from_status(branch_line);
    let tracking = parse_tracking_from_status(branch_line);
    let files = file_lines
        .iter()
        .map(|line| {
            json!({
                "path": line.chars().skip(3).collect::<String>().trim(),
                "status": line.chars().take(2).collect::<String>().trim(),
            })
        })
        .collect::<Vec<_>>();
    let dirty = !files.is_empty();
    let branch_info = rev_list_counts(cwd).unwrap_or((0, 0));
    let detached = branch_line.contains("HEAD detached") || branch_line.contains("no branch");
    let no_upstream = tracking.is_none() && !detached;
    let published_to_remote = !detached
        && branch
            .as_deref()
            .map(|branch_name| remote_branch_exists(cwd, branch_name))
            .unwrap_or(false);
    let local_only_commit_count = if detached {
        0
    } else {
        count_local_only_commits(cwd).unwrap_or(0)
    };
    let diff = repo_diff_totals(cwd, tracking.as_deref(), &file_lines)
        .unwrap_or_else(|_| json!({"additions": 0, "deletions": 0, "binaryFiles": 0}));

    Ok(json!({
        "repoRoot": resolve_repo_root(cwd).ok().map(|path| path.display().to_string()),
        "branch": branch,
        "tracking": tracking,
        "dirty": dirty,
        "ahead": branch_info.0,
        "behind": branch_info.1,
        "localOnlyCommitCount": local_only_commit_count,
        "state": compute_state(dirty, branch_info.0, branch_info.1, detached, no_upstream),
        "canPush": (branch_info.0 > 0 || no_upstream) && !detached,
        "publishedToRemote": published_to_remote,
        "files": files,
        "diff": diff,
    }))
}

fn git_diff(cwd: &Path) -> Result<Value> {
    let porcelain = git(cwd, ["status", "--porcelain=v1", "-b"])?;
    let lines: Vec<&str> = porcelain
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let branch_line = lines.first().copied().unwrap_or("");
    let file_lines = lines.iter().skip(1).copied().collect::<Vec<_>>();
    let tracking = parse_tracking_from_status(branch_line);
    let base_ref = resolve_repo_diff_base(cwd, tracking.as_deref())?;
    let tracked_patch = git(cwd, ["diff", "--binary", "--find-renames", &base_ref])?;
    let untracked_paths = file_lines
        .iter()
        .filter(|line| line.starts_with("?? "))
        .map(|line| line[3..].trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let untracked_patch = diff_patch_for_untracked_files(cwd, &untracked_paths)?;
    let patch = [tracked_patch.trim(), untracked_patch.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(json!({ "patch": patch }))
}

fn git_commit(cwd: &Path, params: &Value) -> Result<Value> {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Changes from Codex");

    let status_check = git(cwd, ["status", "--porcelain"])?;
    if status_check.trim().is_empty() {
        return Err(eyre!("Nothing to commit."));
    }

    let _ = git(cwd, ["add", "-A"])?;
    let output = git(cwd, ["commit", "-m", message])?;
    let parts = output.lines().next().unwrap_or("");
    let captures = regex_like_commit_header(parts);
    let summary = output
        .lines()
        .find(|line| line.contains(" files changed") || line.contains(" file changed"))
        .map(ToOwned::to_owned)
        .or_else(|| output.lines().last().map(ToOwned::to_owned))
        .unwrap_or_default();

    Ok(json!({
        "hash": captures.1,
        "branch": captures.0,
        "summary": summary.trim(),
    }))
}

fn git_push(cwd: &Path) -> Result<Value> {
    let branch = git(cwd, ["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_owned();
    let push_result = git(cwd, ["push"]);
    if let Err(error) = push_result {
        let error_message = error.to_string();
        if error_message.contains("no upstream") || error_message.contains("has no upstream branch")
        {
            let _ = git(cwd, ["push", "--set-upstream", "origin", &branch])?;
        } else if error_message.contains("rejected") {
            return Err(eyre!("Push rejected. Pull changes first."));
        } else {
            return Err(eyre!("{error_message}"));
        }
    }
    Ok(json!({
        "branch": branch,
        "remote": "origin",
        "status": git_status(cwd)?,
    }))
}

fn git_pull(cwd: &Path) -> Result<Value> {
    if let Err(error) = git(cwd, ["pull", "--rebase"]) {
        let _ = git(cwd, ["rebase", "--abort"]);
        return Err(eyre!(
            "{}",
            if error.to_string().is_empty() {
                "Pull failed due to conflicts. Rebase aborted.".to_owned()
            } else {
                error.to_string()
            }
        ));
    }
    Ok(json!({
        "success": true,
        "status": git_status(cwd)?,
    }))
}

fn git_branches(cwd: &Path) -> Result<Value> {
    let output = git(cwd, ["branch", "--no-color"])?;
    let repo_root = resolve_repo_root(cwd).ok();
    let local_checkout_root = resolve_local_checkout_root(cwd).ok();
    let project_relative_path = repo_root
        .as_ref()
        .map(|root| resolve_project_relative_path(cwd, root))
        .unwrap_or_default();
    let worktree_path_by_branch =
        git_worktree_path_by_branch(cwd, &project_relative_path).unwrap_or_default();
    let local_checkout_path = local_checkout_root
        .as_ref()
        .and_then(|root| scoped_local_checkout_path(root, &project_relative_path));

    let mut current = String::new();
    let mut branches = Vec::new();
    let mut branches_checked_out_elsewhere = Vec::new();

    for line in output.lines() {
        let Some((is_current, is_checked_out_elsewhere, name)) = normalize_branch_list_entry(line)
        else {
            continue;
        };

        if name.contains("HEAD detached") || name == "(no branch)" {
            if is_current {
                current = "HEAD".to_owned();
            }
            continue;
        }

        if !branches.contains(&name) {
            branches.push(name.clone());
        }
        if is_checked_out_elsewhere && !branches_checked_out_elsewhere.contains(&name) {
            branches_checked_out_elsewhere.push(name.clone());
        }
        if is_current {
            current = name;
        }
    }

    branches.sort();
    branches_checked_out_elsewhere.sort();
    let default_branch = detect_default_branch(cwd, &branches);

    Ok(json!({
        "branches": branches,
        "branchesCheckedOutElsewhere": branches_checked_out_elsewhere,
        "worktreePathByBranch": worktree_path_by_branch,
        "localCheckoutPath": local_checkout_path.map(|path| path.display().to_string()),
        "current": current,
        "default": default_branch,
    }))
}

fn git_checkout(cwd: &Path, params: &Value) -> Result<Value> {
    let branch = params
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| eyre!("Branch name is required."))?;

    if let Err(error) = git(cwd, ["checkout", branch]) {
        let message = error.to_string();
        if message.contains("would be overwritten") {
            return Err(eyre!(
                "Cannot switch branches: you have uncommitted changes."
            ));
        }
        if message.contains("already used by worktree") {
            return Err(eyre!(
                "Cannot switch branches: this branch is already open in another worktree."
            ));
        }
        return Err(eyre!("{message}"));
    }

    let status = git_status(cwd)?;
    Ok(json!({
        "current": status.get("branch").cloned().unwrap_or_else(|| json!(branch)),
        "tracking": status.get("tracking").cloned().unwrap_or(Value::Null),
        "status": status,
    }))
}

fn git_create_branch(cwd: &Path, params: &Value) -> Result<Value> {
    let branch = normalize_created_branch_name(
        params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if branch.is_empty() {
        return Err(eyre!("Branch name is required."));
    }
    assert_valid_created_branch_name(cwd, &branch)?;
    if !local_branch_exists(cwd, &branch) && remote_branch_exists(cwd, &branch) {
        return Err(eyre!(
            "Branch '{branch}' already exists on origin. Check it out locally instead of creating a new branch."
        ));
    }
    if let Err(error) = git(cwd, ["checkout", "-b", &branch]) {
        let message = error.to_string();
        if message.contains("already exists") {
            return Err(eyre!("Branch '{branch}' already exists."));
        }
        return Err(eyre!("{message}"));
    }
    Ok(json!({
        "branch": branch,
        "status": git_status(cwd)?,
    }))
}

fn git_create_worktree(cwd: &Path, params: &Value) -> Result<Value> {
    let branch = normalize_created_branch_name(
        params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if branch.is_empty() {
        return Err(eyre!("Branch name is required."));
    }
    assert_valid_created_branch_name(cwd, &branch)?;

    let branch_result = git_branches(cwd)?;
    let repo_root = resolve_repo_root(cwd)?;
    let status = git_status(cwd)?;
    let project_relative_path = resolve_project_relative_path(cwd, &repo_root);
    let default_branch = branch_result
        .get("default")
        .and_then(Value::as_str)
        .unwrap_or("");
    let base_branch = params
        .get("baseBranch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default_branch);
    if base_branch.is_empty() {
        return Err(eyre!("Base branch is required."));
    }
    if !local_branch_exists(cwd, base_branch) {
        return Err(eyre!(
            "Base branch '{base_branch}' is not available locally. Create or check out that branch first."
        ));
    }

    let current_branch = status
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let dirty = status
        .get("dirty")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let change_transfer = if params.get("changeTransfer").and_then(Value::as_str) == Some("copy") {
        "copy"
    } else {
        "move"
    };
    let can_carry_local_changes =
        dirty && !current_branch.is_empty() && current_branch == base_branch;
    if dirty && !can_carry_local_changes {
        return Err(eyre!(
            "Uncommitted changes can {} into a new worktree only from {}. Switch the base branch to match or clean up local changes first.",
            if change_transfer == "copy" { "copy" } else { "move" },
            if current_branch.is_empty() { "the current branch" } else { &current_branch }
        ));
    }

    if let Some(existing_worktree_path) = branch_result
        .get("worktreePathByBranch")
        .and_then(|value| value.get(&branch))
        .and_then(Value::as_str)
    {
        if same_file_path(Path::new(existing_worktree_path), cwd) {
            return Err(eyre!("Branch '{branch}' is already open in this project."));
        }
        return Ok(json!({
            "branch": branch,
            "worktreePath": existing_worktree_path,
            "alreadyExisted": true,
        }));
    }

    if local_branch_exists(cwd, &branch) {
        return Err(eyre!(
            "Branch '{branch}' already exists locally. Choose another name or open that branch instead."
        ));
    }

    let worktree_root_path = allocate_managed_worktree_path(&repo_root)?;
    let mut copied_local_changes_patch = String::new();
    if can_carry_local_changes && change_transfer == "copy" {
        copied_local_changes_patch = capture_local_changes_patch(&repo_root)?;
    }

    if let Err(error) = git(
        &repo_root,
        [
            "worktree",
            "add",
            "-b",
            &branch,
            worktree_root_path.to_str().unwrap_or_default(),
            base_branch,
        ],
    ) {
        let _ = fs::remove_dir_all(worktree_root_path.parent().unwrap_or(&worktree_root_path));
        let message = error.to_string();
        if message.contains("invalid reference") {
            return Err(eyre!("Base branch '{base_branch}' does not exist."));
        }
        if message.contains("already exists") {
            return Err(eyre!("Branch '{branch}' already exists."));
        }
        if message.contains("already used by worktree")
            || message.contains("already checked out at")
        {
            return Err(eyre!(
                "Branch '{branch}' is already open in another worktree."
            ));
        }
        return Err(eyre!("{message}"));
    }

    if can_carry_local_changes && change_transfer == "move" {
        let stash_label = format!("remodex-worktree-handoff-{}", Uuid::new_v4().simple());
        let stash_output = git(
            &repo_root,
            [
                "stash",
                "push",
                "--include-untracked",
                "--message",
                &stash_label,
            ],
        )?;
        if !stash_output.contains("No local changes") {
            let stash_ref = find_stash_ref_by_label(&repo_root, &stash_label)?;
            if let Some(stash_ref) = stash_ref {
                let _ = git(&worktree_root_path, ["stash", "pop", &stash_ref]);
            }
        }
    }

    if !copied_local_changes_patch.trim().is_empty() {
        apply_copied_local_changes_to_worktree(&worktree_root_path, &copied_local_changes_patch)?;
    }

    Ok(json!({
        "branch": branch,
        "worktreePath": scoped_worktree_path(&worktree_root_path, &project_relative_path)
            .display()
            .to_string(),
        "alreadyExisted": false,
    }))
}

fn git_remove_worktree(cwd: &Path, params: &Value) -> Result<Value> {
    let worktree_root_path = resolve_repo_root(cwd)?;
    let local_checkout_root = resolve_local_checkout_root(cwd)?;
    let branch = params
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    if same_file_path(&worktree_root_path, &local_checkout_root) {
        return Err(eyre!("Cannot remove the main local checkout."));
    }
    if !is_managed_worktree_path(&worktree_root_path) {
        return Err(eyre!(
            "Only managed worktrees can be removed automatically."
        ));
    }

    cleanup_managed_worktree(&local_checkout_root, &worktree_root_path, branch.as_deref())?;
    if let Some(branch) = branch {
        if local_branch_exists(&local_checkout_root, &branch) {
            return Err(eyre!(
                "The temporary worktree was removed, but branch '{branch}' could not be deleted automatically."
            ));
        }
    }

    Ok(json!({ "success": true }))
}

fn git_reset_to_remote(cwd: &Path, params: &Value) -> Result<Value> {
    if params.get("confirm").and_then(Value::as_str) != Some("discard_runtime_changes") {
        return Err(eyre!(
            "This action requires params.confirm === \"discard_runtime_changes\"."
        ));
    }

    let has_upstream = git(
        cwd,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_ok();
    if has_upstream {
        let _ = git(cwd, ["fetch"])?;
        let _ = git(cwd, ["reset", "--hard", "@{u}"])?;
    } else {
        let _ = git(cwd, ["checkout", "--", "."])?;
    }
    let _ = git(cwd, ["clean", "-fd"])?;

    Ok(json!({
        "success": true,
        "status": git_status(cwd)?,
    }))
}

fn git_remote_url(cwd: &Path) -> Result<Value> {
    let url = git(cwd, ["config", "--get", "remote.origin.url"])?
        .trim()
        .to_owned();
    Ok(json!({
        "url": url,
        "ownerRepo": parse_owner_repo(&url),
    }))
}

fn git_branches_with_status(cwd: &Path) -> Result<Value> {
    let mut branches = git_branches(cwd)?;
    let status = git_status(cwd)?;
    if let Some(object) = branches.as_object_mut() {
        object.insert("status".to_owned(), status);
    }
    Ok(branches)
}

fn git_worktree_path_by_branch(
    cwd: &Path,
    project_relative_path: &str,
) -> Result<serde_json::Map<String, Value>> {
    let output = git(cwd, ["worktree", "list", "--porcelain"])?;
    let mut paths = serde_json::Map::new();

    for record in output.split("\n\n") {
        let mut worktree_path = None::<String>;
        let mut branch_name = None::<String>;

        for line in record
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktree_path = Some(path.trim().to_owned());
            }
            if let Some(branch) = line.strip_prefix("branch ") {
                branch_name = normalize_worktree_branch_ref(branch.trim());
            }
        }

        if let (Some(worktree_path), Some(branch_name)) = (worktree_path, branch_name) {
            paths.insert(
                branch_name,
                Value::String(
                    scoped_worktree_path(Path::new(&worktree_path), project_relative_path)
                        .display()
                        .to_string(),
                ),
            );
        }
    }

    Ok(paths)
}

fn capture_local_changes_patch(cwd: &Path) -> Result<String> {
    let tracked_patch = git(cwd, ["diff", "--binary", "--find-renames", "HEAD"])?;
    let porcelain = git(cwd, ["status", "--porcelain=v1"])?;
    let untracked_paths = porcelain
        .lines()
        .filter(|line| line.starts_with("?? "))
        .map(|line| line[3..].trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let untracked_patch = diff_patch_for_untracked_files(cwd, &untracked_paths)?;
    Ok([tracked_patch, untracked_patch]
        .into_iter()
        .filter(|patch| !patch.trim().is_empty())
        .map(ensure_trailing_newline)
        .collect::<Vec<_>>()
        .join("\n"))
}

fn diff_patch_for_untracked_files(cwd: &Path, file_paths: &[String]) -> Result<String> {
    if file_paths.is_empty() {
        return Ok(String::new());
    }

    let mut patches = Vec::new();
    for file_path in file_paths {
        patches.push(git_diff_no_index(cwd, file_path, true)?);
    }
    Ok(patches
        .into_iter()
        .filter(|patch| !patch.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn apply_copied_local_changes_to_worktree(cwd: &Path, patch: &str) -> Result<()> {
    let patch_path = std::env::temp_dir().join(format!(
        "remodex-worktree-copy-{}.patch",
        Uuid::new_v4().simple()
    ));
    fs::write(&patch_path, ensure_trailing_newline(patch.to_owned()))?;
    let result = git(
        cwd,
        [
            "apply",
            "--binary",
            "--whitespace=nowarn",
            patch_path.to_str().unwrap_or_default(),
        ],
    );
    let _ = fs::remove_file(&patch_path);
    result.map(|_| ())
}

fn find_stash_ref_by_label(cwd: &Path, stash_label: &str) -> Result<Option<String>> {
    let output = git(cwd, ["stash", "list", "--format=%gd%x00%s"])?;
    for record in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = record.split('\0');
        let reference = parts.next().unwrap_or("").trim();
        let summary = parts.next().unwrap_or("");
        if !reference.is_empty() && summary.contains(stash_label) {
            return Ok(Some(reference.to_owned()));
        }
    }
    Ok(None)
}

fn cleanup_managed_worktree(
    repo_root: &Path,
    worktree_root_path: &Path,
    branch_name: Option<&str>,
) -> Result<()> {
    let _ = git(
        repo_root,
        [
            "worktree",
            "remove",
            "--force",
            worktree_root_path.to_str().unwrap_or_default(),
        ],
    );
    if let Some(branch_name) = branch_name {
        let _ = git(repo_root, ["branch", "-D", branch_name]);
    }
    let _ = fs::remove_dir_all(worktree_root_path.parent().unwrap_or(worktree_root_path));
    Ok(())
}

fn resolve_git_cwd(params: &Value) -> Result<PathBuf> {
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
        .ok_or_else(|| eyre!("Git actions require a bound local working directory."))?;
    let path = PathBuf::from(requested_cwd);
    if !path.is_dir() {
        return Err(eyre!(
            "The requested local working directory does not exist on this Mac."
        ));
    }
    Ok(path)
}

fn resolve_repo_root(cwd: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git(cwd, ["rev-parse", "--show-toplevel"])?.trim(),
    ))
}

fn resolve_local_checkout_root(cwd: &Path) -> Result<PathBuf> {
    let common_dir = git(
        cwd,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common_dir = PathBuf::from(common_dir.trim());
    if common_dir.file_name().and_then(|value| value.to_str()) != Some(".git") {
        return resolve_repo_root(cwd);
    }
    Ok(common_dir.parent().unwrap_or(&common_dir).to_path_buf())
}

fn normalize_branch_list_entry(raw_line: &str) -> Option<(bool, bool, String)> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let is_current = trimmed.starts_with("* ");
    let is_checked_out_elsewhere = trimmed.starts_with("+ ");
    let name = trimmed
        .trim_start_matches('*')
        .trim_start_matches('+')
        .trim()
        .to_owned();
    if name.is_empty() {
        return None;
    }
    Some((is_current, is_checked_out_elsewhere, name))
}

fn normalize_created_branch_name(raw_name: &str) -> String {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let normalized = trimmed
        .split('/')
        .map(|segment| segment.split_whitespace().collect::<Vec<_>>().join("-"))
        .collect::<Vec<_>>()
        .join("/");
    if normalized.starts_with("remodex/") {
        normalized
    } else {
        format!("remodex/{normalized}")
    }
}

fn normalize_worktree_branch_ref(raw_ref: &str) -> Option<String> {
    raw_ref
        .trim()
        .strip_prefix("refs/heads/")
        .map(ToOwned::to_owned)
}

fn assert_valid_created_branch_name(cwd: &Path, branch_name: &str) -> Result<()> {
    git(cwd, ["check-ref-format", "--branch", branch_name])
        .map(|_| ())
        .map_err(|_| eyre!("Branch '{branch_name}' is not a valid Git branch name."))
}

fn local_branch_exists(cwd: &Path, branch_name: &str) -> bool {
    git(
        cwd,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch_name}"),
        ],
    )
    .is_ok()
}

fn remote_branch_exists(cwd: &Path, branch_name: &str) -> bool {
    git(
        cwd,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch_name}"),
        ],
    )
    .is_ok()
}

fn rev_list_counts(cwd: &Path) -> Result<(u64, u64)> {
    let output = git(cwd, ["rev-list", "--left-right", "--count", "HEAD...@{u}"])?;
    let parts = output.split_whitespace().collect::<Vec<_>>();
    Ok((
        parts
            .first()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        parts
            .get(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
    ))
}

fn parse_branch_from_status(line: &str) -> Option<String> {
    let line = line.strip_prefix("## ")?.trim();
    let branch = line.split("...").next()?.trim();
    if branch == "HEAD (no branch)" || branch.contains("HEAD detached") {
        None
    } else {
        Some(branch.to_owned())
    }
}

fn parse_tracking_from_status(line: &str) -> Option<String> {
    line.split("...")
        .nth(1)
        .map(|part| {
            part.split_whitespace()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}

fn compute_state(
    dirty: bool,
    ahead: u64,
    behind: u64,
    detached: bool,
    no_upstream: bool,
) -> &'static str {
    if detached {
        "detached_head"
    } else if no_upstream {
        "no_upstream"
    } else if dirty && behind > 0 {
        "dirty_and_behind"
    } else if dirty {
        "dirty"
    } else if ahead > 0 && behind > 0 {
        "diverged"
    } else if behind > 0 {
        "behind_only"
    } else if ahead > 0 {
        "ahead_only"
    } else {
        "up_to_date"
    }
}

fn detect_default_branch(cwd: &Path, branches: &[String]) -> Option<String> {
    if let Ok(reference) = git(cwd, ["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        let branch = reference
            .trim()
            .trim_start_matches("refs/remotes/origin/")
            .trim();
        if !branch.is_empty() {
            return Some(branch.to_owned());
        }
    }
    if remote_branch_exists(cwd, "main") {
        return Some("main".to_owned());
    }
    if remote_branch_exists(cwd, "master") {
        return Some("master".to_owned());
    }
    if branches.iter().any(|branch| branch == "main") {
        return Some("main".to_owned());
    }
    if branches.iter().any(|branch| branch == "master") {
        return Some("master".to_owned());
    }
    branches.first().cloned()
}

fn count_local_only_commits(cwd: &Path) -> Result<u64> {
    let remote_refs = git(cwd, ["for-each-ref", "--format=%(refname)", "refs/remotes"])?;
    if remote_refs.lines().all(|line| line.trim().is_empty()) {
        return Ok(0);
    }
    Ok(
        git(cwd, ["rev-list", "--count", "HEAD", "--not", "--remotes"])?
            .trim()
            .parse()
            .unwrap_or(0),
    )
}

fn repo_diff_totals(cwd: &Path, tracking: Option<&str>, file_lines: &[&str]) -> Result<Value> {
    let base_ref = resolve_repo_diff_base(cwd, tracking)?;
    let tracked_totals = parse_numstat_totals(&git(cwd, ["diff", "--numstat", &base_ref])?);
    let untracked_paths = file_lines
        .iter()
        .filter(|line| line.starts_with("?? "))
        .map(|line| line[3..].trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let mut untracked_additions = 0;
    let mut untracked_deletions = 0;
    let mut untracked_binary = 0;
    for path in untracked_paths {
        let output = git_diff_no_index(cwd, &path, false)?;
        let totals = parse_numstat_totals(&output);
        untracked_additions += totals.0;
        untracked_deletions += totals.1;
        untracked_binary += totals.2;
    }

    Ok(json!({
        "additions": tracked_totals.0 + untracked_additions,
        "deletions": tracked_totals.1 + untracked_deletions,
        "binaryFiles": tracked_totals.2 + untracked_binary,
    }))
}

fn resolve_repo_diff_base(cwd: &Path, tracking: Option<&str>) -> Result<String> {
    if tracking.is_some() {
        if let Ok(base) = git(cwd, ["merge-base", "HEAD", "@{u}"]) {
            return Ok(base.trim().to_owned());
        }
    }
    let first_local_only_commit = git(
        cwd,
        [
            "rev-list",
            "--reverse",
            "--topo-order",
            "HEAD",
            "--not",
            "--remotes",
        ],
    )?
    .lines()
    .find(|line| !line.trim().is_empty())
    .map(str::trim)
    .map(ToOwned::to_owned);

    let Some(first_local_only_commit) = first_local_only_commit else {
        return Ok("HEAD".to_owned());
    };

    if let Ok(base) = git(cwd, ["rev-parse", &format!("{first_local_only_commit}^")]) {
        return Ok(base.trim().to_owned());
    }
    Ok(EMPTY_TREE_HASH.to_owned())
}

fn parse_numstat_totals(output: &str) -> (u64, u64, u64) {
    let mut additions = 0;
    let mut deletions = 0;
    let mut binary_files = 0;

    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split('\t');
        let raw_additions = parts.next().unwrap_or_default();
        let raw_deletions = parts.next().unwrap_or_default();
        match (raw_additions.parse::<u64>(), raw_deletions.parse::<u64>()) {
            (Ok(parsed_additions), Ok(parsed_deletions)) => {
                additions += parsed_additions;
                deletions += parsed_deletions;
            }
            _ => {
                binary_files += 1;
            }
        }
    }

    (additions, deletions, binary_files)
}

fn git_diff_no_index(cwd: &Path, file_path: &str, binary_patch: bool) -> Result<String> {
    let mut args = vec!["diff", "--no-index"];
    if binary_patch {
        args.push("--binary");
    } else {
        args.push("--numstat");
    }
    args.extend(["--", "/dev/null", file_path]);
    git_allow_exit_code_one(cwd, &args)
}

fn git_allow_exit_code_one(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if output.status.success() || output.status.code() == Some(1) {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    Err(eyre!(
        "{}",
        String::from_utf8_lossy(&output.stderr).trim().to_owned()
    ))
}

fn ensure_trailing_newline(value: String) -> String {
    if value.ends_with('\n') {
        value
    } else {
        format!("{value}\n")
    }
}

fn parse_owner_repo(remote_url: &str) -> Option<String> {
    let remote_url = remote_url.trim_end_matches(".git");
    remote_url
        .rsplit_once(':')
        .or_else(|| remote_url.rsplit_once('/'))
        .map(|(_, owner_repo)| owner_repo.to_owned())
}

fn allocate_managed_worktree_path(repo_root: &Path) -> Result<PathBuf> {
    let codex_home = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap().join(".codex"));
    let worktrees_root = codex_home.join("worktrees");
    fs::create_dir_all(&worktrees_root)?;
    let repo_name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");

    for _ in 0..16 {
        let token = Uuid::new_v4().simple().to_string()[..4].to_owned();
        let token_directory = worktrees_root.join(&token);
        let worktree_path = token_directory.join(repo_name);
        if token_directory.exists() || worktree_path.exists() {
            continue;
        }
        fs::create_dir_all(&token_directory)?;
        return Ok(worktree_path);
    }

    Err(eyre!("Could not allocate a managed worktree path."))
}

fn same_file_path(left: &Path, right: &Path) -> bool {
    normalize_existing_path(left) == normalize_existing_path(right)
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn managed_worktrees_root() -> PathBuf {
    let codex_home = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap().join(".codex"));
    codex_home.join("worktrees")
}

fn is_managed_worktree_path(path: &Path) -> bool {
    let candidate = normalize_existing_path(path);
    let root = normalize_existing_path(&managed_worktrees_root());
    candidate.starts_with(&root) && candidate != root
}

fn resolve_project_relative_path(cwd: &Path, repo_root: &Path) -> String {
    let normalized_cwd = normalize_existing_path(cwd);
    let normalized_repo_root = normalize_existing_path(repo_root);
    let relative = normalized_cwd
        .strip_prefix(&normalized_repo_root)
        .ok()
        .map(|path| path.to_path_buf())
        .unwrap_or_default();
    if relative.as_os_str().is_empty() {
        String::new()
    } else {
        relative.display().to_string()
    }
}

fn scoped_worktree_path(worktree_root_path: &Path, project_relative_path: &str) -> PathBuf {
    let normalized_worktree_root_path = normalize_existing_path(worktree_root_path);
    if project_relative_path.is_empty() {
        return normalized_worktree_root_path;
    }
    let candidate_path = normalized_worktree_root_path.join(project_relative_path);
    if candidate_path.is_dir() {
        normalize_existing_path(&candidate_path)
    } else {
        normalized_worktree_root_path
    }
}

fn scoped_local_checkout_path(
    checkout_root_path: &Path,
    project_relative_path: &str,
) -> Option<PathBuf> {
    let normalized_checkout_root_path = normalize_existing_path(checkout_root_path);
    if project_relative_path.is_empty() {
        return Some(normalized_checkout_root_path);
    }
    let candidate_path = normalized_checkout_root_path.join(project_relative_path);
    candidate_path
        .is_dir()
        .then(|| normalize_existing_path(&candidate_path))
}

fn regex_like_commit_header(line: &str) -> (String, String) {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') {
        return (String::new(), String::new());
    }
    let trimmed = trimmed.trim_start_matches('[').trim_end_matches(']');
    let mut parts = trimmed.split_whitespace();
    let branch = parts.next().unwrap_or_default().to_owned();
    let hash = parts.next().unwrap_or_default().to_owned();
    (branch, hash)
}

fn git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
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
