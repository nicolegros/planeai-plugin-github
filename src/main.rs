use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::process::Command;

const PLUGIN_ID: &str = "github";
const PLUGIN_NAME: &str = "GitHub";
const PLUGIN_VERSION: &str = "0.1.0";
const HOST_API_VERSION: &str = "planeai.plugin-host.v1";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("{PLUGIN_ID} plugin starting");
    let stdin = io::stdin();
    let mut input = stdin.lock().lines();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    while let Some(line) = input.next() {
        let line = line?;
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{PLUGIN_ID} ignored malformed JSON-RPC frame: {error}");
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let response = match dispatch(method, params, &mut input, &mut output) {
            Ok(result) => success(id, result),
            Err(message) => failure(id, -32000, &message),
        };
        if request.get("id").is_some() {
            write_frame(&mut output, &response)?;
        }
        if method == "plugin.shutdown" {
            eprintln!("{PLUGIN_ID} plugin stopping");
            break;
        }
    }
    Ok(())
}

fn dispatch<R: BufRead, W: Write>(
    method: &str,
    params: Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    match method {
        "plugin.handshake" => handshake(&params),
        "plugin.shutdown" => Ok(json!({ "stopping": true })),
        "github.status" => status(&params, input, output),
        "github.defaults" => defaults(&params, input, output),
        "github.create" => create_pull_request(&params, input, output),
        "github.link" => link_pull_request(&params, input, output),
        "github.merge" => merge_pull_request(&params, input, output),
        "github.markReady" => mark_ready(&params, input, output),
        "github.failureLogs" => failure_logs(&params, input, output),
        "github.reconcile" => reconcile(input, output),
        _ => Err("method not found".to_string()),
    }
}

fn handshake(params: &Value) -> Result<Value, String> {
    if params
        .get("host_api_version")
        .and_then(Value::as_str)
        != Some(HOST_API_VERSION)
    {
        return Err("unsupported PlaneAI plugin host API".to_string());
    }
    Ok(json!({
        "plugin_id": PLUGIN_ID,
        "plugin_name": PLUGIN_NAME,
        "plugin_version": PLUGIN_VERSION,
        "host_api_version": HOST_API_VERSION,
        "lifecycle_event_subscriptions": [],
    }))
}

fn repository_context<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let session_id = required_string(params, "session_id")?;
    host_call(
        input,
        output,
        &format!("github-context-{session_id}"),
        "host.sessions.repositoryContext",
        json!({ "session_id": session_id }),
    )
}

fn status<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let remote = git_output(&cwd, ["remote", "get-url", "origin"])?;
    if !is_github_remote(&remote) {
        return Ok(json!({ "applicable": false, "reason": "The origin remote is not github.com." }));
    }
    let branch = context_string(&context, "branch")?;
    let raw = gh_output(
        &cwd,
        [
            "pr",
            "view",
            &branch,
            "--json",
            "url,state,isDraft,statusCheckRollup,mergeable,mergeStateStatus,reviewDecision",
        ],
    );
    match raw {
        Ok(raw) => {
            let pr: Value = serde_json::from_str(&raw)
                .map_err(|error| format!("failed to parse gh pr view output: {error}"))?;
            Ok(json!({
                "applicable": true,
                "remote": remote.trim(),
                "pr": project_pr_status(&pr),
            }))
        }
        Err(error) if is_no_pull_request(&error) => Ok(json!({
            "applicable": true,
            "remote": remote.trim(),
            "pr": Value::Null,
        })),
        Err(error) => Err(error),
    }
}

fn defaults<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let branch = context_string(&context, "branch")?;
    let base_branch = context
        .get("base_branch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "main".to_string());
    let diff = git_output(&cwd, ["diff", "--stat", &format!("{base_branch}...HEAD")])
        .unwrap_or_default();
    let body = if diff.trim().is_empty() {
        String::new()
    } else {
        format!("## Changes\n\n```\n{}\n```", diff.trim())
    };
    Ok(json!({ "title": branch, "body": body, "base_branch": base_branch }))
}

fn create_pull_request<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let branch = context_string(&context, "branch")?;
    let title = required_string(params, "title")?;
    let body = params.get("body").and_then(Value::as_str).unwrap_or_default();
    let base_branch = required_string(params, "base_branch")?;
    let draft = params.get("draft").and_then(Value::as_bool).unwrap_or(false);

    git_output(&cwd, ["push", "-u", "origin", &branch])?;
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
        "--base".to_string(),
        base_branch.to_string(),
    ];
    if draft {
        args.push("--draft".to_string());
    }
    let url = command_output(&cwd, "gh", args.iter().map(String::as_str))?;
    Ok(json!({ "url": url.trim() }))
}

fn link_pull_request<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let url = required_string(params, "url")?;
    let raw = gh_output(&cwd, ["pr", "view", url, "--json", "url,state,isDraft"])?;
    let pr: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("failed to parse linked pull request: {error}"))?;
    Ok(json!({ "pr": project_pr_status(&pr) }))
}

fn merge_pull_request<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let branch = context_string(&context, "branch")?;
    let strategy = match params.get("strategy").and_then(Value::as_str).unwrap_or("squash") {
        "squash" => "--squash",
        "merge" => "--merge",
        "rebase" => "--rebase",
        _ => return Err("merge strategy must be squash, merge, or rebase".to_string()),
    };
    gh_output(&cwd, ["pr", "merge", &branch, strategy, "--delete-branch"])?;
    Ok(json!({ "merged": true }))
}

fn mark_ready<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let branch = context_string(&context, "branch")?;
    gh_output(&cwd, ["pr", "ready", &branch])?;
    Ok(json!({ "ready": true }))
}

fn failure_logs<R: BufRead, W: Write>(
    params: &Value,
    input: &mut io::Lines<R>,
    output: &mut W,
) -> Result<Value, String> {
    let context = repository_context(params, input, output)?;
    let cwd = context_path(&context)?;
    let branch = context_string(&context, "branch")?;
    let raw = gh_output(&cwd, ["pr", "view", &branch, "--json", "statusCheckRollup"])?;
    let status: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("failed to parse CI status: {error}"))?;
    let run_id = status
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|check| check.get("conclusion").and_then(Value::as_str).is_some_and(|value| value.eq_ignore_ascii_case("failure")))
        .filter_map(|check| check.get("detailsUrl").and_then(Value::as_str))
        .find_map(action_run_id)
        .ok_or("no failed GitHub Actions run found")?;
    let logs = gh_output(&cwd, ["run", "view", run_id, "--log-failed"])?;
    let tail = logs.lines().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    Ok(json!({ "message": format!("CI failed on branch `{branch}`. Here are the failure logs:\n\n{tail}") }))
}

fn reconcile<R: BufRead, W: Write>(input: &mut io::Lines<R>, output: &mut W) -> Result<Value, String> {
    let sessions = host_call(input, output, "github-sessions", "host.sessions.list", Value::Null)?;
    let mut checked = 0_u64;
    for session in sessions.get("sessions").and_then(Value::as_array).into_iter().flatten() {
        if session.get("status").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let Some(session_id) = session.get("id").and_then(Value::as_str) else { continue; };
        let _ = status(&json!({ "session_id": session_id }), input, output);
        checked += 1;
    }
    Ok(json!({ "checked": checked }))
}

fn project_pr_status(pr: &Value) -> Value {
    let state = if pr.get("isDraft").and_then(Value::as_bool) == Some(true) {
        "draft".to_string()
    } else {
        pr.get("state").and_then(Value::as_str).unwrap_or("open").to_ascii_lowercase()
    };
    let mergeable = pr.get("mergeable").and_then(Value::as_str).unwrap_or_default();
    let merge_state = pr.get("mergeStateStatus").and_then(Value::as_str).unwrap_or_default();
    json!({
        "url": pr.get("url").cloned().unwrap_or(Value::Null),
        "state": state,
        "checks": pr.get("statusCheckRollup").cloned().unwrap_or_else(|| json!([])),
        "conflicting": mergeable == "CONFLICTING" || merge_state == "DIRTY",
        "merge_blocked": merge_state == "BLOCKED",
        "review_decision": pr.get("reviewDecision").cloned().unwrap_or(Value::Null),
    })
}

fn host_call<R: BufRead, W: Write>(
    input: &mut io::Lines<R>,
    output: &mut W,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    write_frame(output, &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
        .map_err(|error| format!("failed to call host: {error}"))?;
    loop {
        let line = input
            .next()
            .ok_or("host closed stdin while handling callback")?
            .map_err(|error| format!("failed reading host callback response: {error}"))?;
        let response: Value = serde_json::from_str(&line)
            .map_err(|error| format!("host callback response was not JSON: {error}"))?;
        if response.get("id").and_then(Value::as_str) != Some(id) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(format!("host callback failed: {error}"));
        }
        return response
            .get("result")
            .cloned()
            .ok_or("host callback response did not contain a result".to_string());
    }
}

fn context_path(context: &Value) -> Result<String, String> {
    context_string(context, "working_tree_path")
}

fn context_string<'a>(context: &'a Value, field: &str) -> Result<String, String> {
    context
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("repository context did not include {field}"))
}

fn required_string<'a>(params: &'a Value, field: &str) -> Result<&'a str, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{field} is required"))
}

fn git_output<'a>(cwd: &str, args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    command_output(cwd, "git", args)
}

fn gh_output<'a>(cwd: &str, args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    command_output(cwd, "gh", args)
}

fn command_output<'a>(cwd: &str, program: &str, args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() { format!("{program} exited with {}", output.status) } else { stderr });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn is_github_remote(remote: &str) -> bool {
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    [
        "git@github.com:",
        "git@github.com/",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
    ]
    .iter()
    .any(|prefix| remote.starts_with(prefix))
}

fn is_no_pull_request(error: &str) -> bool {
    error.contains("no pull requests found") || error.contains("Could not resolve to a PullRequest")
}

fn action_run_id(url: &str) -> Option<&str> {
    let marker = "/actions/runs/";
    let rest = url.get(url.find(marker)? + marker.len()..)?;
    Some(rest.split('/').next()?)
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_frame(output: &mut impl Write, frame: &Value) -> io::Result<()> {
    output.write_all(serde_json::to_string(frame).expect("JSON value serializes").as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_existing_github_remote_forms_only() {
        assert!(is_github_remote("git@github.com:planeai/planeai.git"));
        assert!(is_github_remote("ssh://git@github.com/planeai/planeai.git"));
        assert!(is_github_remote("https://github.com/planeai/planeai.git"));
        assert!(!is_github_remote("git@gitlab.com:planeai/planeai.git"));
    }

    #[test]
    fn extracts_actions_run_id() {
        assert_eq!(action_run_id("https://github.com/o/r/actions/runs/123/job/45"), Some("123"));
        assert_eq!(action_run_id("https://github.com/o/r/pull/1"), None);
    }

    #[test]
    fn projects_draft_and_conflict_status() {
        let status = project_pr_status(&json!({
            "url": "https://github.com/o/r/pull/1",
            "state": "OPEN",
            "isDraft": true,
            "mergeable": "CONFLICTING",
            "mergeStateStatus": "DIRTY",
            "statusCheckRollup": []
        }));
        assert_eq!(status["state"], "draft");
        assert_eq!(status["conflicting"], true);
    }
}
