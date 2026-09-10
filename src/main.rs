use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, RecvTimeoutError, Sender},
    Arc,
};
use std::thread;
use std::time::Duration;

const PLUGIN_ID: &str = "github";
const PLUGIN_NAME: &str = "GitHub";
const PLUGIN_VERSION: &str = "0.1.0";
const HOST_API_VERSION: &str = "planeai.plugin-host.v1";
const CANCELLATION_ERROR_CODE: i64 = -32800;
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone)]
struct CommandRunner {
    git: PathBuf,
    gh: PathBuf,
}

impl Default for CommandRunner {
    fn default() -> Self {
        Self {
            git: PathBuf::from("git"),
            gh: PathBuf::from("gh"),
        }
    }
}

impl CommandRunner {
    #[cfg(test)]
    fn new(git: impl Into<PathBuf>, gh: impl Into<PathBuf>) -> Self {
        Self {
            git: git.into(),
            gh: gh.into(),
        }
    }
}

#[derive(Clone)]
struct ExecutionContext {
    cancelled: Arc<AtomicBool>,
    events: Sender<ControllerEvent>,
    runner: CommandRunner,
    request_key: String,
    callback_sequence: Arc<AtomicU64>,
}

impl ExecutionContext {
    fn check_cancelled(&self) -> Result<(), RequestError> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(RequestError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
enum RequestError {
    Cancelled,
    Message(String),
}

impl From<String> for RequestError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

enum InputEvent {
    Frame(Value),
    Invalid(String),
    Closed,
}

enum ControllerEvent {
    HostCall {
        id: String,
        request_key: String,
        method: String,
        params: Value,
        response: Sender<Result<Value, RequestError>>,
    },
    Finished {
        request_key: Option<String>,
        id: Value,
        result: Result<Value, RequestError>,
        shutdown: bool,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("{PLUGIN_ID} plugin starting");
    run_with_runner(io::stdin(), io::stdout().lock(), CommandRunner::default())?;
    eprintln!("{PLUGIN_ID} plugin stopping");
    Ok(())
}

fn run_with_runner<R: Read + Send + 'static, W: Write>(
    input: R,
    mut output: W,
    runner: CommandRunner,
) -> io::Result<()> {
    let (input_tx, input_rx) = mpsc::channel();
    thread::spawn(move || read_input(input, input_tx));

    let (event_tx, event_rx) = mpsc::channel();
    let mut active_requests: HashMap<String, Arc<AtomicBool>> = HashMap::new();
    let mut callbacks: HashMap<String, (String, Sender<Result<Value, RequestError>>)> =
        HashMap::new();
    let callback_sequence = Arc::new(AtomicU64::new(1));
    let mut input_open = true;
    let mut stopping = false;

    while input_open || !active_requests.is_empty() {
        while let Ok(event) = event_rx.try_recv() {
            handle_controller_event(
                event,
                &mut output,
                &mut active_requests,
                &mut callbacks,
                &mut stopping,
            )?;
        }
        if stopping && active_requests.is_empty() {
            break;
        }
        match input_rx.recv_timeout(POLL_INTERVAL) {
            Ok(InputEvent::Frame(frame)) => handle_input_frame(
                frame,
                &event_tx,
                &runner,
                Arc::clone(&callback_sequence),
                &mut active_requests,
                &mut callbacks,
            ),
            Ok(InputEvent::Invalid(error)) => {
                eprintln!("{PLUGIN_ID} ignored malformed JSON-RPC frame: {error}")
            }
            Ok(InputEvent::Closed) | Err(RecvTimeoutError::Disconnected) => {
                input_open = false;
                for cancelled in active_requests.values() {
                    cancelled.store(true, Ordering::SeqCst);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}

fn read_input<R: Read>(input: R, sender: Sender<InputEvent>) {
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next() {
        match line {
            Ok(line) => match serde_json::from_str(&line) {
                Ok(frame) => {
                    if sender.send(InputEvent::Frame(frame)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if sender.send(InputEvent::Invalid(error.to_string())).is_err() {
                        return;
                    }
                }
            },
            Err(error) => {
                eprintln!("{PLUGIN_ID} failed reading stdin: {error}");
                break;
            }
        }
    }
    let _ = sender.send(InputEvent::Closed);
}

fn handle_input_frame(
    frame: Value,
    event_tx: &Sender<ControllerEvent>,
    runner: &CommandRunner,
    callback_sequence: Arc<AtomicU64>,
    active_requests: &mut HashMap<String, Arc<AtomicBool>>,
    callbacks: &mut HashMap<String, (String, Sender<Result<Value, RequestError>>)>,
) {
    if let Some(id) = frame.get("id").and_then(Value::as_str) {
        if let Some((_request_key, callback)) = callbacks.remove(id) {
            let result = if let Some(error) = frame.get("error") {
                Err(RequestError::Message(format!(
                    "host callback failed: {error}"
                )))
            } else {
                frame.get("result").cloned().ok_or_else(|| {
                    RequestError::Message(
                        "host callback response did not contain a result".to_string(),
                    )
                })
            };
            let _ = callback.send(result);
            return;
        }
    }

    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method == "$/cancelRequest" {
        if let Some(id) = frame.get("params").and_then(|params| params.get("id")) {
            if let Some(cancelled) = active_requests.get(&request_key(id)) {
                cancelled.store(true, Ordering::SeqCst);
            }
        }
        return;
    }

    let Some(id) = frame.get("id").cloned() else {
        return;
    };
    let key = request_key(&id);
    let cancelled = Arc::new(AtomicBool::new(false));
    active_requests.insert(key.clone(), Arc::clone(&cancelled));
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    let method = method.to_string();
    let runner = runner.clone();
    let events = event_tx.clone();
    thread::spawn(move || {
        let context = ExecutionContext {
            cancelled,
            events: events.clone(),
            runner,
            request_key: key.clone(),
            callback_sequence,
        };
        let shutdown = method == "plugin.shutdown";
        let result = dispatch(&method, params, &context);
        let _ = events.send(ControllerEvent::Finished {
            request_key: Some(key),
            id,
            result,
            shutdown,
        });
    });
}

fn handle_controller_event<W: Write>(
    event: ControllerEvent,
    output: &mut W,
    active_requests: &mut HashMap<String, Arc<AtomicBool>>,
    callbacks: &mut HashMap<String, (String, Sender<Result<Value, RequestError>>)>,
    stopping: &mut bool,
) -> io::Result<()> {
    match event {
        ControllerEvent::HostCall {
            id,
            request_key,
            method,
            params,
            response,
        } => {
            write_frame(
                output,
                &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            )?;
            callbacks.insert(id, (request_key, response));
        }
        ControllerEvent::Finished {
            request_key,
            id,
            result,
            shutdown,
        } => {
            if let Some(key) = request_key {
                active_requests.remove(&key);
                callbacks.retain(|_, (owner, _)| owner != &key);
            }
            let response = match result {
                Ok(result) => success(id, result),
                Err(RequestError::Cancelled) => {
                    failure(id, CANCELLATION_ERROR_CODE, "request cancelled")
                }
                Err(RequestError::Message(message)) => failure(id, -32000, &message),
            };
            write_frame(output, &response)?;
            if shutdown {
                *stopping = true;
            }
        }
    }
    Ok(())
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).expect("JSON-RPC ids serialize")
}

fn dispatch(
    method: &str,
    params: Value,
    context: &ExecutionContext,
) -> Result<Value, RequestError> {
    context.check_cancelled()?;
    match method {
        "plugin.handshake" => handshake(&params).map_err(Into::into),
        "plugin.shutdown" => Ok(json!({ "stopping": true })),
        "github.status" => status(&params, context),
        "github.defaults" => defaults(&params, context),
        "github.create" => create_pull_request(&params, context),
        "github.link" => link_pull_request(&params, context),
        "github.merge" => merge_pull_request(&params, context),
        "github.markReady" => mark_ready(&params, context),
        "github.failureLogs" => failure_logs(&params, context),
        "github.reconcile" => reconcile(context),
        _ => Err(RequestError::Message("method not found".to_string())),
    }
}

fn handshake(params: &Value) -> Result<Value, String> {
    if params.get("host_api_version").and_then(Value::as_str) != Some(HOST_API_VERSION) {
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

fn repository_context(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    host_call(
        context,
        &format!("github-context-{session_id}"),
        "host.sessions.repositoryContext",
        json!({ "session_id": session_id }),
    )
}

fn status(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let remote = git_output(context, &cwd, ["remote", "get-url", "origin"])?;
    if !is_github_remote(&remote) {
        return Ok(
            json!({ "applicable": false, "reason": "The origin remote is not github.com." }),
        );
    }
    let branch = context_string(&repository, "branch")?;
    let raw = gh_output(
        context,
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
            let pr: Value = serde_json::from_str(&raw).map_err(|error| {
                RequestError::Message(format!("failed to parse gh pr view output: {error}"))
            })?;
            Ok(json!({ "applicable": true, "remote": remote.trim(), "pr": project_pr_status(&pr) }))
        }
        Err(RequestError::Message(error)) if is_no_pull_request(&error) => {
            Ok(json!({ "applicable": true, "remote": remote.trim(), "pr": Value::Null }))
        }
        Err(error) => Err(error),
    }
}

fn defaults(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    let base_branch = repository
        .get("base_branch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "main".to_string());
    let diff = git_output(
        context,
        &cwd,
        ["diff", "--stat", &format!("{base_branch}...HEAD")],
    )
    .unwrap_or_default();
    let body = if diff.trim().is_empty() {
        String::new()
    } else {
        format!("## Changes\n\n```\n{}\n```", diff.trim())
    };
    Ok(json!({ "title": branch, "body": body, "base_branch": base_branch }))
}

fn create_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    let title = required_string(params, "title")?;
    let body = params
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let base_branch = required_string(params, "base_branch")?;
    let draft = params
        .get("draft")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    git_output(context, &cwd, ["push", "-u", "origin", &branch])?;
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
    let url = command_output(
        context,
        &cwd,
        &context.runner.gh,
        args.iter().map(String::as_str),
    )?;
    Ok(json!({ "url": url.trim() }))
}

fn link_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let url = required_string(params, "url")?;
    let raw = gh_output(
        context,
        &cwd,
        ["pr", "view", url, "--json", "url,state,isDraft"],
    )?;
    let pr: Value = serde_json::from_str(&raw).map_err(|error| {
        RequestError::Message(format!("failed to parse linked pull request: {error}"))
    })?;
    Ok(json!({ "pr": project_pr_status(&pr) }))
}

fn merge_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    let strategy = match params
        .get("strategy")
        .and_then(Value::as_str)
        .unwrap_or("squash")
    {
        "squash" => "--squash",
        "merge" => "--merge",
        "rebase" => "--rebase",
        _ => {
            return Err(RequestError::Message(
                "merge strategy must be squash, merge, or rebase".to_string(),
            ))
        }
    };
    gh_output(
        context,
        &cwd,
        ["pr", "merge", &branch, strategy, "--delete-branch"],
    )?;
    Ok(json!({ "merged": true }))
}

fn mark_ready(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    gh_output(context, &cwd, ["pr", "ready", &branch])?;
    Ok(json!({ "ready": true }))
}

fn failure_logs(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    let raw = gh_output(
        context,
        &cwd,
        ["pr", "view", &branch, "--json", "statusCheckRollup"],
    )?;
    let status: Value = serde_json::from_str(&raw)
        .map_err(|error| RequestError::Message(format!("failed to parse CI status: {error}")))?;
    let run_id = status
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|check| {
            check
                .get("conclusion")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case("failure"))
        })
        .filter_map(|check| check.get("detailsUrl").and_then(Value::as_str))
        .find_map(action_run_id)
        .ok_or_else(|| RequestError::Message("no failed GitHub Actions run found".to_string()))?;
    let logs = gh_output(context, &cwd, ["run", "view", run_id, "--log-failed"])?;
    let tail = logs
        .lines()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(
        json!({ "message": format!("CI failed on branch `{branch}`. Here are the failure logs:\n\n{tail}") }),
    )
}

fn reconcile(context: &ExecutionContext) -> Result<Value, RequestError> {
    let sessions = host_call(
        context,
        "github-sessions",
        "host.sessions.list",
        Value::Null,
    )?;
    let mut checked = 0_u64;
    for session in sessions
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        context.check_cancelled()?;
        if session.get("status").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let Some(session_id) = session.get("id").and_then(Value::as_str) else {
            continue;
        };
        let _ = status(&json!({ "session_id": session_id }), context);
        checked += 1;
    }
    Ok(json!({ "checked": checked }))
}

fn project_pr_status(pr: &Value) -> Value {
    let state = if pr.get("isDraft").and_then(Value::as_bool) == Some(true) {
        "draft".to_string()
    } else {
        pr.get("state")
            .and_then(Value::as_str)
            .unwrap_or("open")
            .to_ascii_lowercase()
    };
    let mergeable = pr
        .get("mergeable")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let merge_state = pr
        .get("mergeStateStatus")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "url": pr.get("url").cloned().unwrap_or(Value::Null),
        "state": state,
        "checks": pr.get("statusCheckRollup").cloned().unwrap_or_else(|| json!([])),
        "conflicting": mergeable == "CONFLICTING" || merge_state == "DIRTY",
        "merge_blocked": merge_state == "BLOCKED",
        "review_decision": pr.get("reviewDecision").cloned().unwrap_or(Value::Null),
    })
}

fn host_call(
    context: &ExecutionContext,
    id_prefix: &str,
    method: &str,
    params: Value,
) -> Result<Value, RequestError> {
    context.check_cancelled()?;
    let id = format!(
        "{id_prefix}-{}",
        context.callback_sequence.fetch_add(1, Ordering::Relaxed)
    );
    let (response_tx, response_rx) = mpsc::channel();
    context
        .events
        .send(ControllerEvent::HostCall {
            id,
            request_key: context.request_key.clone(),
            method: method.to_string(),
            params,
            response: response_tx,
        })
        .map_err(|_| RequestError::Message("request controller stopped".to_string()))?;
    loop {
        context.check_cancelled()?;
        match response_rx.recv_timeout(POLL_INTERVAL) {
            Ok(response) => return response,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RequestError::Message(
                    "request controller stopped".to_string(),
                ))
            }
        }
    }
}

fn context_path(context: &Value) -> Result<String, RequestError> {
    context_string(context, "working_tree_path")
}

fn context_string(context: &Value, field: &str) -> Result<String, RequestError> {
    context
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| RequestError::Message(format!("repository context did not include {field}")))
}

fn required_string<'a>(params: &'a Value, field: &str) -> Result<&'a str, RequestError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RequestError::Message(format!("{field} is required")))
}

fn git_output<'a>(
    context: &ExecutionContext,
    cwd: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, RequestError> {
    command_output(context, cwd, &context.runner.git, args)
}

fn gh_output<'a>(
    context: &ExecutionContext,
    cwd: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, RequestError> {
    command_output(context, cwd, &context.runner.gh, args)
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn capture_output<R: Read>(mut reader: R) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(bytes.len());
        let kept = remaining.min(count);
        bytes.extend_from_slice(&buffer[..kept]);
        truncated |= kept < count;
    }
    Ok(CapturedOutput { bytes, truncated })
}

fn command_output<'a>(
    context: &ExecutionContext,
    cwd: &str,
    program: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, RequestError> {
    context.check_cancelled()?;
    let program_name = program.to_string_lossy();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| RequestError::Message(format!("failed to run {program_name}: {error}")))?;
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let stdout_reader = thread::spawn(move || capture_output(stdout));
    let stderr_reader = thread::spawn(move || capture_output(stderr));

    let status = loop {
        if context.cancelled.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(RequestError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(RequestError::Message(format!(
                    "failed waiting for {program_name}: {error}"
                )));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| RequestError::Message(format!("failed collecting {program_name} stdout")))?
        .map_err(|error| {
            RequestError::Message(format!("failed reading {program_name} stdout: {error}"))
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| RequestError::Message(format!("failed collecting {program_name} stderr")))?
        .map_err(|error| {
            RequestError::Message(format!("failed reading {program_name} stderr: {error}"))
        })?;
    if stdout.truncated {
        return Err(RequestError::Message(format!(
            "{program_name} stdout exceeded {MAX_COMMAND_OUTPUT_BYTES} bytes"
        )));
    }
    if !status.success() {
        let mut message = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
        if stderr.truncated {
            message.push_str("\n[stderr truncated]");
        }
        return Err(RequestError::Message(if message.is_empty() {
            format!("{program_name} exited with {status}")
        } else {
            message
        }));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).to_string())
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
    output.write_all(
        serde_json::to_string(frame)
            .expect("JSON value serializes")
            .as_bytes(),
    )?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc::Receiver;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn recognizes_existing_github_remote_forms_only() {
        assert!(is_github_remote("git@github.com:planeai/planeai.git"));
        assert!(is_github_remote("ssh://git@github.com/planeai/planeai.git"));
        assert!(is_github_remote("https://github.com/planeai/planeai.git"));
        assert!(!is_github_remote("git@gitlab.com:planeai/planeai.git"));
    }

    #[test]
    fn extracts_actions_run_id() {
        assert_eq!(
            action_run_id("https://github.com/o/r/actions/runs/123/job/45"),
            Some("123")
        );
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

    #[cfg(unix)]
    #[test]
    fn fake_commands_return_output_and_report_stderr_failures() {
        let directory = TestDirectory::new();
        let success = directory.script("success", "#!/bin/sh\nprintf 'fake output\\n'\n");
        let failure = directory.script(
            "failure",
            "#!/bin/sh\nprintf 'fake failure\\n' >&2\nexit 7\n",
        );
        let context = test_context(CommandRunner::new(&success, &success));
        assert_eq!(
            command_output(&context, directory.path(), &success, ["ignored"]).unwrap(),
            "fake output\n"
        );
        let failure_context = test_context(CommandRunner::new(&failure, &failure));
        assert!(matches!(
            command_output(&failure_context, directory.path(), &failure, ["ignored"]),
            Err(RequestError::Message(message)) if message == "fake failure"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_fake_subprocess_and_returns_json_rpc_cancellation() {
        let directory = TestDirectory::new();
        let marker = directory.0.join("started");
        let git = directory.script("git", "#!/bin/sh\nprintf 'https://github.com/o/r.git\\n'\n");
        let gh = directory.script(
            "gh",
            &format!(
                "#!/bin/sh\nprintf started > '{}'\nwhile :; do sleep 1; done\n",
                marker.display()
            ),
        );
        let (input_tx, input_rx) = mpsc::channel();
        let (output_tx, output_rx) = mpsc::channel();
        let runner = CommandRunner::new(git, gh);
        let thread = thread::spawn(move || {
            run_with_runner(
                ChannelInput::new(input_rx),
                LineOutput::new(output_tx),
                runner,
            )
        });

        input_tx
            .send(json!({ "jsonrpc": "2.0", "id": "request-1", "method": "github.status", "params": { "session_id": "s1"} }).to_string())
            .unwrap();
        let callback = receive_frame(&output_rx);
        let callback_id = callback["id"].as_str().unwrap();
        assert_eq!(callback["method"], "host.sessions.repositoryContext");
        input_tx
            .send(json!({ "jsonrpc": "2.0", "id": callback_id, "result": { "working_tree_path": directory.path(), "branch": "topic" } }).to_string())
            .unwrap();
        wait_for_file(&marker);
        input_tx
            .send(json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": "request-1" } }).to_string())
            .unwrap();
        let response = receive_frame(&output_rx);
        assert_eq!(response["id"], "request-1");
        assert_eq!(response["error"]["code"], CANCELLATION_ERROR_CODE);
        drop(input_tx);
        thread.join().unwrap().unwrap();
    }

    fn test_context(runner: CommandRunner) -> ExecutionContext {
        let (events, _events_rx) = mpsc::channel();
        ExecutionContext {
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            runner,
            request_key: "test-request".to_string(),
            callback_sequence: Arc::new(AtomicU64::new(1)),
        }
    }

    #[cfg(unix)]
    struct TestDirectory(PathBuf);

    #[cfg(unix)]
    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "planeai-plugin-github-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn script(&self, name: &str, content: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            fs::write(&path, content).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
            path
        }
    }

    #[cfg(unix)]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    struct ChannelInput {
        receiver: Receiver<String>,
        buffer: Vec<u8>,
        position: usize,
    }

    #[cfg(unix)]
    impl ChannelInput {
        fn new(receiver: Receiver<String>) -> Self {
            Self {
                receiver,
                buffer: Vec::new(),
                position: 0,
            }
        }
    }

    #[cfg(unix)]
    impl Read for ChannelInput {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            while self.position == self.buffer.len() {
                match self.receiver.recv() {
                    Ok(line) => {
                        self.buffer = format!("{line}\n").into_bytes();
                        self.position = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let count = output.len().min(self.buffer.len() - self.position);
            output[..count].copy_from_slice(&self.buffer[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        }
    }

    #[cfg(unix)]
    struct LineOutput {
        sender: Sender<String>,
        buffer: Vec<u8>,
    }

    #[cfg(unix)]
    impl LineOutput {
        fn new(sender: Sender<String>) -> Self {
            Self {
                sender,
                buffer: Vec::new(),
            }
        }
    }

    #[cfg(unix)]
    impl Write for LineOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.extend_from_slice(bytes);
            while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8(self.buffer.drain(..=end).collect()).unwrap();
                self.sender.send(line.trim_end().to_string()).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "test receiver closed")
                })?;
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    fn receive_frame(receiver: &Receiver<String>) -> Value {
        let line = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("expected JSON-RPC frame");
        serde_json::from_str(&line).unwrap()
    }

    #[cfg(unix)]
    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() {
            assert!(Instant::now() < deadline, "fake gh did not start");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
