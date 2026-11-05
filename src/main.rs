mod durable_state;
mod github_graphql;
mod pull_requests;
mod refresh;

use durable_state::{
    begin_reconciliation_patch, empty_durable_state, finish_reconciliation_patch,
    migrate_durable_state, reconciliation_from_settings_value, validate_task_transitions,
    STATE_NAMESPACE, STATE_VERSION,
};
use github_graphql::{
    project_detail, CheckResult, FetchedPr, GraphqlResponse, Lookup, LookupRequest,
};
use pull_requests::{
    aggregate, dismiss, own_link, parse_session_links, parse_sessions, record_created,
    record_manual, refresh_snapshot, session_links_value, task_group, task_transition,
    AggregatedPr, PrKey, PrSnapshot, PrState, PrStateTransition, PullRequestLink, SessionInfo,
};
use refresh::{BranchTarget, Plan, SessionLinks};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, RecvTimeoutError, Sender},
    Arc, Mutex, MutexGuard,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PLUGIN_ID: &str = "github";
const PLUGIN_NAME: &str = "GitHub";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const HOST_API_VERSION: &str = "planeai.plugin-host.v6";
/// The MCP tool agents in PlaneAI sessions call, served as `github_link_pull_request`.
const LINK_PULL_REQUEST_TOOL: &str = "link_pull_request";
const CANCELLATION_ERROR_CODE: i64 = -32800;
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Lookup rounds per refresh: each round reaches one more pull request along a stack.
const STATUS_REFRESH_ROUNDS: usize = 3;
const RECONCILE_REFRESH_ROUNDS: usize = 2;

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
    #[cfg(all(test, unix))]
    fn new(git: impl Into<PathBuf>, gh: impl Into<PathBuf>) -> Self {
        Self {
            git: git.into(),
            gh: gh.into(),
        }
    }
}

#[derive(Clone)]
struct DurableStateLocks {
    reconciliation: Arc<Mutex<()>>,
    pull_requests: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct ExecutionContext {
    cancelled: Arc<AtomicBool>,
    events: Sender<ControllerEvent>,
    runner: CommandRunner,
    request_key: String,
    callback_sequence: Arc<AtomicU64>,
    durable_state_locks: DurableStateLocks,
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

type CallbackResponses = HashMap<String, (String, Sender<Result<Value, RequestError>>)>;

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
    let durable_state_locks = DurableStateLocks {
        reconciliation: Arc::new(Mutex::new(())),
        pull_requests: Arc::new(Mutex::new(())),
    };
    let mut stopping = false;

    'serve: loop {
        for event in event_rx.try_iter() {
            if let Err(error) = handle_controller_event(
                event,
                &mut output,
                &mut active_requests,
                &mut callbacks,
                &mut stopping,
            ) {
                eprintln!("{PLUGIN_ID} lost host output: {error}");
                break 'serve;
            }
        }
        if stopping && active_requests.is_empty() {
            return Ok(());
        }
        match input_rx.recv_timeout(POLL_INTERVAL) {
            Ok(InputEvent::Frame(frame)) => handle_input_frame(
                frame,
                &event_tx,
                &runner,
                Arc::clone(&callback_sequence),
                durable_state_locks.clone(),
                &mut active_requests,
                &mut callbacks,
            ),
            Ok(InputEvent::Invalid(error)) => {
                eprintln!("{PLUGIN_ID} ignored malformed JSON-RPC frame: {error}")
            }
            Ok(InputEvent::Closed) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    for (_, (_, response)) in callbacks.drain() {
        let _ = response.send(Err(host_connection_closed()));
    }
    for cancelled in active_requests.values() {
        cancelled.store(true, Ordering::SeqCst);
    }
    while !active_requests.is_empty() {
        match event_rx.recv().expect("the loop holds event_tx") {
            ControllerEvent::HostCall { response, .. } => {
                let _ = response.send(Err(host_connection_closed()));
            }
            // The host usually closes stdout with stdin; ignore write errors so cancelled requests can finish.
            finished => {
                let _ = handle_controller_event(
                    finished,
                    &mut output,
                    &mut active_requests,
                    &mut callbacks,
                    &mut stopping,
                );
            }
        }
    }
    Ok(())
}

fn read_input<R: Read>(input: R, sender: Sender<InputEvent>) {
    let lines = BufReader::new(input).lines();
    for line in lines {
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
    durable_state_locks: DurableStateLocks,
    active_requests: &mut HashMap<String, Arc<AtomicBool>>,
    callbacks: &mut CallbackResponses,
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
            durable_state_locks,
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
    callbacks: &mut CallbackResponses,
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

fn host_connection_closed() -> RequestError {
    RequestError::Message("host connection closed".to_string())
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
        "github.summary" => summary(&params, context),
        "github.defaults" => defaults(&params, context),
        "github.settings" => github_settings(context),
        "github.settings.update" => update_github_settings(&params, context),
        "github.create" => create_pull_request(&params, context),
        "github.link" => link_pull_request(&params, context),
        "github.unlink" => unlink_pull_request(&params, context),
        "github.merge" => merge_pull_request(&params, context),
        "github.markReady" => mark_ready(&params, context),
        "github.failureLogs" => failure_logs(&params, context),
        "github.sendFailureLogs" => send_failure_logs(&params, context),
        "github.reconcile" => reconcile(context),
        "tool.call" => tool_call(&params, context),
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

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn settings_value_from_host_response(response: &Value) -> Result<Value, RequestError> {
    response.get("settings").cloned().ok_or_else(|| {
        RequestError::Message("host settings response did not contain settings".to_string())
    })
}

fn host_call_with_cancellation(
    context: &ExecutionContext,
    id_prefix: &str,
    method: &str,
    params: Value,
    allow_cancelled: bool,
) -> Result<Value, RequestError> {
    if !allow_cancelled {
        context.check_cancelled()?;
    }
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
        if !allow_cancelled {
            context.check_cancelled()?;
        }
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

fn durable_state_patch_request(patch: Value) -> Value {
    let mut namespaces = serde_json::Map::new();
    namespaces.insert(STATE_NAMESPACE.to_string(), patch);
    json!({ "patch": Value::Object(namespaces) })
}

/// Callers make sure the namespace is current first, with `ensure_durable_state`.
fn patch_durable_state(
    context: &ExecutionContext,
    allow_cancelled: bool,
    patch: Value,
) -> Result<(), RequestError> {
    if !allow_cancelled {
        context.check_cancelled()?;
    }
    let response = host_call_with_cancellation(
        context,
        "github-settings-patch",
        "host.settings.patch",
        durable_state_patch_request(patch),
        allow_cancelled,
    )?;
    if response.get("updated").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(RequestError::Message(
            "host settings patch response did not confirm the update".to_string(),
        ))
    }
}

fn read_reconciliation_state(
    context: &ExecutionContext,
    allow_cancelled: bool,
) -> Result<Value, RequestError> {
    ensure_durable_state(context, allow_cancelled)?;
    let response = host_call_with_cancellation(
        context,
        "github-settings-reconciliation",
        "host.settings.get",
        json!({ "path": [STATE_NAMESPACE, "reconciliation"] }),
        allow_cancelled,
    )?;
    Ok(reconciliation_from_settings_value(
        settings_value_from_host_response(&response)?,
    )?)
}

fn apply_task_transition(
    context: &ExecutionContext,
    session_id: &str,
    transition: Option<PrStateTransition>,
) {
    let Some(transition) = transition else { return };
    let field = match transition {
        PrStateTransition::Opened => "on_open",
        PrStateTransition::Merged => "on_merge",
    };
    // The change is already stored, so a transition that cannot be read is logged and dropped.
    let transitions = match read_task_transitions(context, false) {
        Ok(transitions) => transitions,
        Err(error) => {
            eprintln!("{PLUGIN_ID} skipped the {field} task transition: {error:?}");
            return;
        }
    };
    let Some(status) = transitions.get(field).and_then(Value::as_str) else {
        return;
    };
    if let Err(error) = host_call(
        context,
        "github-transition-linked-task",
        "host.sessions.transitionLinkedTask",
        json!({ "session_id": session_id, "status": status }),
    ) {
        let message = match error {
            RequestError::Cancelled => "request cancelled".to_string(),
            RequestError::Message(message) => message,
        };
        eprintln!("{PLUGIN_ID} failed configured {field} task transition: {message}");
    }
}

fn begin_reconciliation(
    context: &ExecutionContext,
    attempt_id: &str,
    started_at: u64,
) -> Result<(), RequestError> {
    let reconciliation = read_reconciliation_state(context, false)?;
    patch_durable_state(
        context,
        false,
        json!({ "reconciliation": begin_reconciliation_patch(&reconciliation, attempt_id, started_at)? }),
    )
}

fn finish_reconciliation(
    context: &ExecutionContext,
    allow_cancelled: bool,
    attempt_id: &str,
    status: &str,
    checked: u64,
    finished_at: u64,
    error: Option<&str>,
) -> Result<(), RequestError> {
    let reconciliation = read_reconciliation_state(context, allow_cancelled)?;
    patch_durable_state(
        context,
        allow_cancelled,
        json!({
            "reconciliation": finish_reconciliation_patch(
                &reconciliation,
                attempt_id,
                status,
                checked,
                finished_at,
                error,
            )?
        }),
    )
}

fn repository_context(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    session_repository_context(required_string(params, "session_id")?, context)
}

fn session_repository_context(
    session_id: &str,
    context: &ExecutionContext,
) -> Result<Value, RequestError> {
    host_call(
        context,
        &format!("github-context-{session_id}"),
        "host.sessions.repositoryContext",
        json!({ "session_id": session_id }),
    )
}

fn session_name(context: &ExecutionContext, session_id: &str, fallback: &str) -> String {
    host_call(
        context,
        "github-sessions-for-defaults",
        "host.sessions.list",
        Value::Null,
    )
    .ok()
    .and_then(|sessions| {
        sessions
            .get("sessions")
            .and_then(Value::as_array)?
            .iter()
            .find(|session| session.get("id").and_then(Value::as_str) == Some(session_id))?
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
    })
    .unwrap_or_else(|| fallback.to_string())
}

fn task_title(context: &ExecutionContext, key: &str) -> Option<String> {
    host_call(
        context,
        "github-task-for-defaults",
        "host.task.get",
        json!({ "key": key }),
    )
    .ok()
    .and_then(|response| {
        response
            .get("task")?
            .get("title")?
            .as_str()
            .map(str::to_string)
    })
    .filter(|title| !title.trim().is_empty())
}

fn default_pr_title(
    session_name: &str,
    branch: &str,
    task_key: Option<&str>,
    task_title: Option<String>,
) -> String {
    match task_key {
        Some(key) => {
            let fallback = session_name
                .strip_prefix(&format!("{key}: "))
                .unwrap_or(session_name);
            let name = task_title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| {
                    if fallback.trim().is_empty() {
                        branch.to_string()
                    } else {
                        fallback.to_string()
                    }
                });
            format!("{name} [{key}]")
        }
        None if !session_name.trim().is_empty() => session_name.to_string(),
        None => branch.to_string(),
    }
}

fn detect_default_branch(context: &ExecutionContext, cwd: &str) -> String {
    git_output(
        context,
        cwd,
        ["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
    )
    .ok()
    .and_then(|value| value.trim().strip_prefix("origin/").map(str::to_string))
    .filter(|value| !value.is_empty())
    .unwrap_or_else(|| "main".to_string())
}

fn defaults(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    let base_branch = repository
        .get("base_branch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| detect_default_branch(context, &cwd));
    let name = session_name(context, session_id, &branch);
    let task_key = repository
        .get("linked_task_key")
        .and_then(Value::as_str)
        .filter(|key| !key.trim().is_empty());
    let title = default_pr_title(
        &name,
        &branch,
        task_key,
        task_key.and_then(|key| task_title(context, key)),
    );
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
    Ok(json!({ "title": title, "body": body, "base_branch": base_branch }))
}

fn github_repository_slug(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = [
        "git@github.com:",
        "git@github.com/",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
    ]
    .iter()
    .find_map(|prefix| remote.strip_prefix(prefix))?;
    let mut segments = path.split('/');
    let owner = segments.next().filter(|segment| !segment.is_empty())?;
    let repository = segments.next().filter(|segment| !segment.is_empty())?;
    (segments.next().is_none()).then(|| format!("{owner}/{repository}"))
}

fn host_call(
    context: &ExecutionContext,
    id_prefix: &str,
    method: &str,
    params: Value,
) -> Result<Value, RequestError> {
    host_call_with_cancellation(context, id_prefix, method, params, false)
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

/// The run id of a `https://github.com/<repository>/actions/runs/<id>` URL, only for that repository.
fn action_run_id<'a>(url: &'a str, repository: &str) -> Option<&'a str> {
    let path = url
        .strip_prefix("https://github.com/")?
        .split(['?', '#'])
        .next()?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    if !format!("{owner}/{name}").eq_ignore_ascii_case(repository)
        || segments.next()? != "actions"
        || segments.next()? != "runs"
    {
        return None;
    }
    segments
        .next()
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
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

fn read_task_transitions(
    context: &ExecutionContext,
    allow_cancelled: bool,
) -> Result<Value, RequestError> {
    ensure_durable_state(context, allow_cancelled)?;
    let response = host_call_with_cancellation(
        context,
        "github-settings-task-transitions",
        "host.settings.get",
        json!({ "path": [STATE_NAMESPACE, "task_transitions"] }),
        allow_cancelled,
    )?;
    Ok(validate_task_transitions(
        &settings_value_from_host_response(&response)?,
    )?)
}

fn github_settings(context: &ExecutionContext) -> Result<Value, RequestError> {
    Ok(json!({ "task_transitions": read_task_transitions(context, false)? }))
}

fn update_github_settings(
    params: &Value,
    context: &ExecutionContext,
) -> Result<Value, RequestError> {
    let transitions = params
        .get("task_transitions")
        .ok_or_else(|| RequestError::Message("task_transitions is required".to_string()))?;
    validate_task_transitions(transitions)?;
    ensure_durable_state(context, false)?;
    patch_durable_state(context, false, json!({ "task_transitions": transitions }))?;
    Ok(json!({ "task_transitions": transitions }))
}

fn invalid_state(message: impl Into<String>) -> RequestError {
    RequestError::Message(message.into())
}

fn read_state_version(
    context: &ExecutionContext,
    allow_cancelled: bool,
) -> Result<Value, RequestError> {
    let response = host_call_with_cancellation(
        context,
        "github-settings-version",
        "host.settings.get",
        json!({ "path": [STATE_NAMESPACE, "version"] }),
        allow_cancelled,
    )?;
    settings_value_from_host_response(&response)
}

/// Initializes or migrates the namespace unless it is current.
/// Never call it while holding the pull request lock, which it takes to rewrite the namespace.
fn ensure_durable_state(
    context: &ExecutionContext,
    allow_cancelled: bool,
) -> Result<(), RequestError> {
    if read_state_version(context, allow_cancelled)?.as_u64() == Some(STATE_VERSION) {
        return Ok(());
    }
    // Rewriting the whole namespace would undo a concurrent links write, so it takes that lock
    // and checks again in case another request migrated meanwhile.
    let _persistence = lock_pull_requests(context)?;
    let version = read_state_version(context, allow_cancelled)?;
    match version.as_u64() {
        None if version.is_null() => {
            host_call_with_cancellation(
                context,
                "github-settings-initialize",
                "host.settings.patch",
                durable_state_patch_request(empty_durable_state()),
                allow_cancelled,
            )?;
            Ok(())
        }
        Some(STATE_VERSION) => Ok(()),
        // migrate_durable_state knows which older versions it can upgrade.
        Some(version) => {
            let response = host_call_with_cancellation(
                context,
                &format!("github-settings-v{version}"),
                "host.settings.get",
                json!({ "path": [STATE_NAMESPACE] }),
                allow_cancelled,
            )?;
            let migrated = migrate_durable_state(&settings_value_from_host_response(&response)?)?;
            let response = host_call_with_cancellation(
                context,
                &format!("github-settings-migrate-v{version}"),
                "host.settings.patch",
                durable_state_patch_request(migrated),
                allow_cancelled,
            )?;
            if response.get("updated").and_then(Value::as_bool) == Some(true) {
                Ok(())
            } else {
                Err(invalid_state(
                    "host settings patch response did not confirm the migration",
                ))
            }
        }
        None => Err(invalid_state("unsupported GitHub durable state version")),
    }
}

fn lock_pull_requests(context: &ExecutionContext) -> Result<MutexGuard<'_, ()>, RequestError> {
    context
        .durable_state_locks
        .pull_requests
        .lock()
        .map_err(|_| invalid_state("pull request persistence lock poisoned"))
}

/// Every session's stored links, unparsed, in one host call; callers ensure the namespace first.
fn read_stored_links(context: &ExecutionContext) -> Result<Map<String, Value>, RequestError> {
    let response = host_call(
        context,
        "github-settings-pr-links",
        "host.settings.get",
        json!({ "path": [STATE_NAMESPACE, "pull_requests"] }),
    )?;
    match settings_value_from_host_response(&response)? {
        Value::Null => Ok(Map::new()),
        Value::Object(stored) => Ok(stored),
        _ => Err(invalid_state(
            "GitHub durable state pull_requests must be an object",
        )),
    }
}

fn group_links(stored: &Map<String, Value>, group: &[String]) -> Result<SessionLinks, String> {
    group
        .iter()
        .map(|session_id| {
            let links = parse_session_links(stored.get(session_id).unwrap_or(&Value::Null))?;
            Ok((session_id.clone(), links))
        })
        .collect()
}

fn read_group_links(
    context: &ExecutionContext,
    group: &[String],
) -> Result<SessionLinks, RequestError> {
    ensure_durable_state(context, false)?;
    Ok(group_links(&read_stored_links(context)?, group)?)
}

fn list_sessions(context: &ExecutionContext) -> Result<Vec<SessionInfo>, RequestError> {
    host_call(
        context,
        "github-sessions",
        "host.sessions.list",
        Value::Null,
    )
    .map(|response| parse_sessions(&response))
}

/// The sessions that share a task, starting with the one a request is about.
struct TaskGroup {
    members: Vec<String>,
    /// False when the host could not list sessions, so the task's other sessions are unknown.
    resolved: bool,
}

/// A session's task group; a host that cannot list sessions leaves the session on its own.
fn session_task_group(
    context: &ExecutionContext,
    session_id: &str,
) -> Result<TaskGroup, RequestError> {
    match list_sessions(context) {
        Ok(sessions) => Ok(TaskGroup {
            members: task_group(&sessions, session_id),
            resolved: true,
        }),
        Err(RequestError::Cancelled) => Err(RequestError::Cancelled),
        Err(RequestError::Message(message)) => {
            eprintln!("{PLUGIN_ID} could not list sessions for {session_id}: {message}");
            Ok(TaskGroup {
                members: vec![session_id.to_string()],
                resolved: false,
            })
        }
    }
}

/// The task group a change is about; without the task's other sessions the change could store a
/// state whose task transition is then lost, so the change fails before it starts.
fn changing_task_group(
    context: &ExecutionContext,
    session_id: &str,
) -> Result<TaskGroup, RequestError> {
    let task = session_task_group(context, session_id)?;
    if task.resolved {
        Ok(task)
    } else {
        Err(invalid_state(
            "Could not list the task's sessions; try again.",
        ))
    }
}

fn group_prs(lists: &SessionLinks, group: &[String]) -> Vec<AggregatedPr> {
    aggregate(
        group
            .iter()
            .filter_map(|session_id| lists.get(session_id).map(Vec::as_slice)),
    )
}

fn prs_json(prs: &[AggregatedPr]) -> Value {
    Value::Array(prs.iter().map(AggregatedPr::to_json).collect())
}

struct GroupUpdate {
    lists: SessionLinks,
    prs: Vec<AggregatedPr>,
    changed: bool,
}

/// Reads, changes, and persists the links of a task group, then applies the task transition
/// that the persisted change caused, so an unchanged state never repeats a transition.
fn update_group_links(
    context: &ExecutionContext,
    task: &TaskGroup,
    mutate: impl FnOnce(&mut SessionLinks) -> Result<(), RequestError>,
) -> Result<GroupUpdate, RequestError> {
    let mut mutate = Some(mutate);
    let mut updates = update_groups(context, std::slice::from_ref(task), false, |lists| {
        mutate.take().expect("one group mutates once")(lists)
    })?;
    Ok(updates.remove(0))
}

/// What a change does to a task group's links: the result, the sessions to store, and the task
/// transition it causes.
struct GroupChange {
    update: GroupUpdate,
    patch: Map<String, Value>,
    transition: Option<(String, PrStateTransition)>,
}

fn project_group(
    before: SessionLinks,
    task: &TaskGroup,
    mutate: impl FnOnce(&mut SessionLinks) -> Result<(), RequestError>,
) -> Result<GroupChange, RequestError> {
    let group = task.members.as_slice();
    let mut after = before.clone();
    mutate(&mut after)?;
    let mut patch = Map::new();
    for (session_id, links) in &after {
        let previous = before
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if refresh::differs(previous, links) {
            patch.insert(session_id.clone(), session_links_value(links));
        }
    }
    let prs = group_prs(&after, group);
    let transition = group
        .first()
        .filter(|_| task.resolved)
        .and_then(|session_id| {
            task_transition(&group_prs(&before, group), &prs)
                .map(|transition| (session_id.clone(), transition))
        });
    Ok(GroupChange {
        update: GroupUpdate {
            changed: !patch.is_empty(),
            lists: after,
            prs,
        },
        patch,
        transition,
    })
}

/// Like `update_group_links` for several tasks at once, with one read and one write.
/// With `skip_unreadable`, a task whose stored links cannot be parsed is left out instead of
/// failing every other task. A task whose other sessions are unknown never transitions.
fn update_groups(
    context: &ExecutionContext,
    tasks: &[TaskGroup],
    skip_unreadable: bool,
    mut mutate: impl FnMut(&mut SessionLinks) -> Result<(), RequestError>,
) -> Result<Vec<GroupUpdate>, RequestError> {
    ensure_durable_state(context, false)?;
    let mut updates = Vec::new();
    let mut transitions = Vec::new();
    {
        let _persistence = lock_pull_requests(context)?;
        let stored = read_stored_links(context)?;
        let mut patch = Map::new();
        for task in tasks {
            let group = task.members.as_slice();
            let before = match group_links(&stored, group) {
                Ok(before) => before,
                Err(message) if skip_unreadable => {
                    eprintln!("{PLUGIN_ID} skipped updating sessions {group:?}: {message}");
                    continue;
                }
                Err(message) => return Err(invalid_state(message)),
            };
            let change = project_group(before, task, &mut mutate)?;
            patch.extend(change.patch);
            if let Some(transition) = change.transition {
                transitions.push(transition);
            }
            updates.push(change.update);
        }
        if !patch.is_empty() {
            patch_durable_state(
                context,
                false,
                json!({ "pull_requests": Value::Object(patch) }),
            )?;
        }
    }
    for (session_id, transition) in transitions {
        apply_task_transition(context, &session_id, Some(transition));
    }
    Ok(updates)
}

fn fetch_graphql(
    context: &ExecutionContext,
    cwd: &str,
    requests: &[LookupRequest],
) -> Result<GraphqlResponse, RequestError> {
    let query = format!("query={}", github_graphql::build_query(requests));
    let output = run_command(
        context,
        cwd,
        &context.runner.gh,
        ["api", "graphql", "-f", &query],
    )?;
    // gh exits non-zero when some lookups fail, yet still prints the data GitHub returned.
    let body: Option<Value> = serde_json::from_str(&output.stdout).ok();
    let Some(body) = body.filter(|body| body.get("data").is_some_and(|data| !data.is_null()))
    else {
        return Err(RequestError::Message(if output.success {
            "GitHub returned no pull request data".to_string()
        } else {
            output.failure_message()
        }));
    };
    if let Some(errors) = body.get("errors") {
        eprintln!("{PLUGIN_ID} GitHub answered some lookups with errors: {errors}");
    }
    Ok(github_graphql::parse_response(
        requests,
        &body,
        unix_timestamp_ms(),
    ))
}

struct GithubSession {
    cwd: String,
    remote: String,
    repository: String,
    branch: String,
}

fn github_session(
    params: &Value,
    context: &ExecutionContext,
) -> Result<Option<GithubSession>, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let remote = git_output(context, &cwd, ["remote", "get-url", "origin"])?
        .trim()
        .to_string();
    if !is_github_remote(&remote) {
        return Ok(None);
    }
    let slug = github_repository_slug(&remote)
        .ok_or_else(|| invalid_state("GitHub remote does not include an owner and repository"))?;
    Ok(Some(GithubSession {
        branch: context_string(&repository, "branch")?,
        repository: slug.to_ascii_lowercase(),
        cwd,
        remote,
    }))
}

fn status(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let Some(session) = github_session(params, context)? else {
        return Ok(
            json!({ "applicable": false, "reason": "The origin remote is not github.com." }),
        );
    };
    let task = session_task_group(context, session_id)?;
    let group = &task.members;
    let lists = read_group_links(context, group)?;
    // The panel can highlight any of the task's pull requests, so each one gets its details.
    let detail_keys: BTreeSet<PrKey> = lists
        .values()
        .flatten()
        .filter(|link| link.source().is_some())
        .map(|link| link.key.clone())
        .collect();
    let targets = [BranchTarget {
        session_id: session_id.to_string(),
        repository: session.repository.clone(),
        branch: session.branch.clone(),
        detail: true,
    }];
    let groups = [group.clone()];
    let plan = Plan {
        lists: &lists,
        groups: &groups,
        branch_targets: &targets,
        detail_keys: &detail_keys,
    };
    let outcome = refresh::run(&plan, STATUS_REFRESH_ROUNDS, |requests| {
        fetch_graphql(context, &session.cwd, requests)
    })?;
    let update = if task.resolved {
        update_group_links(context, &task, |fresh| {
            refresh::apply(fresh, &groups, &outcome);
            Ok(())
        })?
    } else {
        // Storing it would hide the change from the next refresh that knows the whole task,
        // so that refresh could never apply its task transition.
        let change = project_group(lists.clone(), &task, |fresh| {
            refresh::apply(fresh, &groups, &outcome);
            Ok(())
        })?;
        GroupUpdate {
            changed: false,
            ..change.update
        }
    };
    let own = update
        .lists
        .get(session_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let own_url = own_link(own, &session.repository).map(|link| link.url.clone());
    let details: Vec<Value> = outcome
        .fetched
        .values()
        .filter(|pr| pr.detail.is_some())
        .map(|pr| {
            project_detail(
                pr,
                outcome
                    .merge_methods
                    .get(&pr.key.repository)
                    .cloned()
                    .unwrap_or_default(),
            )
        })
        .collect();
    Ok(json!({
        "applicable": true,
        "remote": session.remote,
        "repository": session.repository,
        "branch": session.branch,
        "pull_requests": prs_json(&update.prs),
        "own_url": own_url,
        "details": details,
        "changed": update.changed,
    }))
}

/// The cached pull requests of a session's task, without asking GitHub.
/// With `check_repository`, it also says whether the session's origin is on github.com.
fn summary(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let task = session_task_group(context, session_id)?;
    let group = &task.members;
    let lists = read_group_links(context, group)?;
    let mut summary = json!({ "pull_requests": prs_json(&group_prs(&lists, group)) });
    if params.get("check_repository").and_then(Value::as_bool) == Some(true) {
        let repository = repository_context(params, context)?;
        let remote = git_output(
            context,
            &context_path(&repository)?,
            ["remote", "get-url", "origin"],
        )?;
        summary["applicable"] = json!(is_github_remote(&remote));
    }
    Ok(summary)
}

fn fetch_pull_request(
    context: &ExecutionContext,
    cwd: &str,
    key: &PrKey,
) -> Result<FetchedPr, RequestError> {
    let request = LookupRequest::new(
        &key.repository,
        Lookup::Number {
            number: key.number,
            detail: false,
        },
    );
    let mut response = fetch_graphql(context, cwd, std::slice::from_ref(&request))?;
    response
        .lookups
        .remove(&request)
        .flatten()
        .and_then(|prs| prs.into_iter().next())
        .ok_or_else(|| {
            invalid_state(format!(
                "GitHub could not find pull request {}#{}",
                key.repository, key.number
            ))
        })
}

fn pull_request_key(params: &Value) -> Result<PrKey, RequestError> {
    let url = required_string(params, "url")?;
    PrKey::from_url(url).ok_or_else(|| {
        invalid_state(
            "Enter a github.com pull request URL, such as https://github.com/owner/repo/pull/123.",
        )
    })
}

fn link_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let key = pull_request_key(params)?;
    let update = link_to_task(context, session_id, &key)?;
    Ok(json!({ "pull_requests": prs_json(&update.prs) }))
}

/// Links a pull request to the session, refreshing it wherever the task already tracks it.
fn link_to_task(
    context: &ExecutionContext,
    session_id: &str,
    key: &PrKey,
) -> Result<GroupUpdate, RequestError> {
    let task = changing_task_group(context, session_id)?;
    let repository = session_repository_context(session_id, context)?;
    let pr = fetch_pull_request(context, &context_path(&repository)?, key)?;
    update_group_links(context, &task, |lists| {
        for links in lists.values_mut() {
            refresh_snapshot(links, &pr.key, &pr.snapshot);
        }
        record_manual(
            lists.entry(session_id.to_string()).or_default(),
            &pr.key,
            &pr.url,
            &pr.snapshot,
        );
        Ok(())
    })
}

/// Runs an MCP tool for the agent of `caller.session_id`; failures are results the agent reads.
fn tool_call(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = params
        .pointer("/caller/session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_state("tool.call has no caller session"))?;
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    let tool = params
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let outcome = match tool {
        LINK_PULL_REQUEST_TOOL => pull_request_key(arguments).and_then(|key| {
            Ok(link_summary(
                &key,
                &link_to_task(context, session_id, &key)?.prs,
            ))
        }),
        _ => Err(invalid_state(format!("unknown tool {tool}"))),
    };
    match outcome {
        Ok(text) => Ok(json!({ "text": text })),
        Err(RequestError::Cancelled) => Err(RequestError::Cancelled),
        Err(RequestError::Message(message)) => Ok(json!({ "text": message, "is_error": true })),
    }
}

/// What the agent reads after linking `key`: that pull request, then all of the task's.
fn link_summary(key: &PrKey, prs: &[AggregatedPr]) -> String {
    let name = |key: &PrKey| format!("{}#{}", key.repository, key.number);
    let linked = match prs.iter().find(|pr| &pr.key == key) {
        Some(pr) => format!(
            "{} ({}): {}",
            name(key),
            pr.snapshot.state.label(),
            pr.snapshot.title.as_deref().unwrap_or("untitled")
        ),
        None => name(key),
    };
    let all: Vec<String> = prs.iter().map(|pr| name(&pr.key)).collect();
    format!(
        "Linked {linked}.\nThe session's task has {} pull request{}: {}.",
        all.len(),
        if all.len() == 1 { "" } else { "s" },
        all.join(", ")
    )
}

/// Unlinks a pull request, or dismisses a stack pull request, from every session of the task.
fn unlink_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let task = changing_task_group(context, session_id)?;
    let key = pull_request_key(params)?;
    let update = update_group_links(context, &task, |lists| {
        let mut removed = false;
        for links in lists.values_mut() {
            removed |= dismiss(links, &key);
        }
        if removed {
            Ok(())
        } else {
            Err(invalid_state(
                "This pull request follows the session branch and cannot be unlinked.",
            ))
        }
    })?;
    Ok(json!({ "pull_requests": prs_json(&update.prs) }))
}

fn create_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let task = changing_task_group(context, session_id)?;
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
        "pr",
        "create",
        "--title",
        title,
        "--body",
        body,
        "--base",
        base_branch,
    ];
    if draft {
        args.push("--draft");
    }
    let url = gh_output(context, &cwd, args)?.trim().to_string();
    let key = PrKey::from_url(&url)
        .ok_or_else(|| invalid_state(format!("gh pr create returned an unexpected URL: {url}")))?;
    let snapshot = PrSnapshot {
        state: if draft { PrState::Draft } else { PrState::Open },
        title: Some(title.to_string()),
        checks: None,
        head_branch: Some(branch),
        base_branch: Some(base_branch.to_string()),
        updated_at: None,
        // The time comes from GitHub, so it compares with GitHub's merge times; without it, the
        // missing-time rules of task transitions apply.
        created_at: fetch_pull_request(context, &cwd, &key)
            .ok()
            .and_then(|pr| pr.snapshot.created_at),
        merged_at: None,
        refreshed_at: unix_timestamp_ms(),
    };
    update_group_links(context, &task, |lists| {
        record_created(
            lists.entry(session_id.to_string()).or_default(),
            &key,
            &url,
            &snapshot,
        );
        Ok(())
    })?;
    Ok(json!({ "url": url }))
}

/// The pull request an action targets.
enum ActionTarget {
    PullRequest {
        key: PrKey,
        url: String,
    },
    /// Whatever the session branch resolves to on GitHub.
    Branch(String),
}

impl ActionTarget {
    /// What `gh pr` commands accept to name the target.
    fn reference(&self) -> &str {
        match self {
            Self::PullRequest { url, .. } => url,
            Self::Branch(branch) => branch,
        }
    }
}

/// The URL the panel sent, which must be a pull request; else the session's own pull request;
/// else the session branch's.
fn action_target(
    params: &Value,
    context: &ExecutionContext,
    session: &GithubSession,
) -> Result<ActionTarget, RequestError> {
    if params.get("url").is_some_and(|url| !url.is_null()) {
        let key = pull_request_key(params)?;
        let url = required_string(params, "url")?.trim().to_string();
        return Ok(ActionTarget::PullRequest { key, url });
    }
    let session_id = required_string(params, "session_id")?;
    let links = read_group_links(context, &[session_id.to_string()])?;
    Ok(match own_link(&links[session_id], &session.repository) {
        Some(link) => ActionTarget::PullRequest {
            key: link.key.clone(),
            url: link.url.clone(),
        },
        None => ActionTarget::Branch(session.branch.clone()),
    })
}

fn required_github_session(
    params: &Value,
    context: &ExecutionContext,
) -> Result<GithubSession, RequestError> {
    github_session(params, context)?
        .ok_or_else(|| invalid_state("The origin remote is not github.com."))
}

/// The targeted pull request with its checks and mergeability, and its repository's merge methods.
fn fetch_target_detail(
    context: &ExecutionContext,
    session: &GithubSession,
    target: ActionTarget,
) -> Result<(FetchedPr, Vec<String>), RequestError> {
    let request = match target {
        ActionTarget::PullRequest { key, .. } => LookupRequest::new(
            &key.repository,
            Lookup::Number {
                number: key.number,
                detail: true,
            },
        ),
        ActionTarget::Branch(branch) => LookupRequest::new(
            &session.repository,
            Lookup::Head {
                branch,
                detail: true,
            },
        ),
    };
    let mut response = fetch_graphql(context, &session.cwd, std::slice::from_ref(&request))?;
    let prs = response
        .lookups
        .remove(&request)
        .flatten()
        .unwrap_or_default();
    let pr = match &request.lookup {
        Lookup::Head { branch, .. } => refresh::branch_pull_request(&prs, branch),
        _ => prs.first(),
    }
    .cloned()
    .ok_or_else(|| invalid_state("GitHub could not find the pull request"))?;
    let methods = response
        .merge_methods
        .remove(&request.repository)
        .unwrap_or_default();
    Ok((pr, methods))
}

fn merge_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let task = changing_task_group(context, session_id)?;
    let strategy = params
        .get("strategy")
        .and_then(Value::as_str)
        .unwrap_or("squash");
    let strategy_flag = match strategy {
        "squash" => "--squash",
        "merge" => "--merge",
        "rebase" => "--rebase",
        _ => {
            return Err(invalid_state(
                "merge strategy must be squash, merge, or rebase",
            ))
        }
    };
    let session = required_github_session(params, context)?;
    let cwd = session.cwd.as_str();
    let target = action_target(params, context, &session)?;
    let (pr, methods) = fetch_target_detail(context, &session, target)?;
    // Only the session's own branch is safe to delete: another branch can be someone's work.
    let own_branch = pr.key.repository == session.repository
        && pr.snapshot.head_branch.as_deref() == Some(session.branch.as_str());
    let (key, url) = (pr.key, pr.url);
    if !methods.iter().any(|method| method == strategy) {
        return Err(invalid_state(format!(
            "{strategy} merge is disabled for this GitHub repository"
        )));
    }
    let mut args = vec!["pr", "merge", &url, strategy_flag];
    if own_branch {
        args.push("--delete-branch");
    }
    gh_output(context, cwd, args)?;
    // What GitHub says now, with its own merge time; a merge queue can keep the pull request open.
    // A read lagging right after a real merge is corrected by the next refresh, which then fires on_merge.
    let fetched = fetch_pull_request(context, cwd, &key).ok();
    update_group_links(context, &task, |lists| {
        for links in lists.values_mut() {
            let snapshot = match &fetched {
                Some(pr) => Some(pr.snapshot.clone()),
                // Without the lookup, gh's success is the best answer, at an unknown time.
                None => links
                    .iter()
                    .find(|link| link.key == key)
                    .and_then(PullRequestLink::snapshot)
                    .map(|snapshot| PrSnapshot {
                        state: PrState::Merged,
                        refreshed_at: unix_timestamp_ms(),
                        ..snapshot.clone()
                    }),
            };
            if let Some(snapshot) = snapshot {
                refresh_snapshot(links, &key, &snapshot);
            }
        }
        Ok(())
    })?;
    Ok(json!({ "merged": true }))
}

fn mark_ready(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session = required_github_session(params, context)?;
    let target = action_target(params, context, &session)?;
    gh_output(context, &session.cwd, ["pr", "ready", target.reference()])?;
    Ok(json!({ "ready": true }))
}

fn failure_log_message(params: &Value, context: &ExecutionContext) -> Result<String, RequestError> {
    let session = required_github_session(params, context)?;
    let target = action_target(params, context, &session)?;
    let (pr, _) = fetch_target_detail(context, &session, target)?;
    // A linked pull request can live in another repository than the session's checkout.
    let run_repository = &pr.key.repository;
    let run_id = pr
        .detail
        .iter()
        .flat_map(|detail| &detail.checks)
        .filter(|check| check.result == CheckResult::Fail)
        .filter_map(|check| check.details_url.as_deref())
        .find_map(|url| action_run_id(url, run_repository))
        .ok_or_else(|| invalid_state("no failed GitHub Actions run found"))?;
    let cwd = &session.cwd;
    let logs = gh_output(
        context,
        cwd,
        ["run", "view", run_id, "--log-failed", "-R", run_repository],
    )?;
    let tail = logs
        .lines()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let subject = format!("{}#{}", pr.key.repository, pr.key.number);
    Ok(format!(
        "CI failed on {subject}. Here are the failure logs:\n\n{tail}"
    ))
}

fn failure_logs(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    Ok(json!({ "message": failure_log_message(params, context)? }))
}

fn send_failure_logs(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let source_session_id = required_string(params, "session_id")?;
    let recipient_session_id = required_string(params, "recipient_session_id")?;
    let mut source = json!({ "session_id": source_session_id });
    if let Some(url) = params.get("url").filter(|url| url.is_string()) {
        source["url"] = url.clone();
    }
    let text = failure_log_message(&source, context)?;
    let response = host_call_with_cancellation(
        context,
        "github-send-failure-logs",
        "host.sessions.prompt",
        json!({ "session_id": recipient_session_id, "text": text }),
        false,
    )?;
    if response.get("delivered").and_then(Value::as_bool) != Some(true) {
        return Err(invalid_state(
            "host session prompt response did not confirm delivery",
        ));
    }
    Ok(json!({ "sent": true }))
}

/// Refreshes every task group with an active session using as few GitHub queries as possible;
/// returns whether it stored any change.
fn reconcile_groups(context: &ExecutionContext, checked: &mut u64) -> Result<bool, RequestError> {
    let sessions = list_sessions(context)?;
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut targets = Vec::new();
    let mut cwd = None;
    for session in sessions
        .iter()
        .filter(|session| session.status.as_deref() == Some("active"))
    {
        context.check_cancelled()?;
        *checked += 1;
        let group = task_group(&sessions, &session.id);
        let members: BTreeSet<&String> = group.iter().collect();
        if !groups
            .iter()
            .any(|existing| existing.iter().collect::<BTreeSet<_>>() == members)
        {
            groups.push(group);
        }
        match github_session(&json!({ "session_id": session.id }), context) {
            Ok(Some(github)) => {
                targets.push(BranchTarget {
                    session_id: session.id.clone(),
                    repository: github.repository,
                    branch: github.branch,
                    detail: false,
                });
                cwd.get_or_insert(github.cwd);
            }
            Ok(None) => {}
            Err(RequestError::Cancelled) => return Err(RequestError::Cancelled),
            Err(RequestError::Message(message)) => {
                eprintln!(
                    "{PLUGIN_ID} skipped branch detection for {}: {message}",
                    session.id
                );
            }
        }
    }
    ensure_durable_state(context, false)?;
    let stored = read_stored_links(context)?;
    let mut lists = SessionLinks::new();
    groups.retain(|group| match group_links(&stored, group) {
        Ok(links) => {
            lists.extend(links);
            true
        }
        Err(message) => {
            eprintln!("{PLUGIN_ID} skipped refreshing sessions {group:?}: {message}");
            false
        }
    });
    targets.retain(|target| lists.contains_key(&target.session_id));
    let has_links = lists.values().any(|links| !links.is_empty());
    if targets.is_empty() && !has_links {
        return Ok(false);
    }
    let cwd = cwd.unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let detail_keys = BTreeSet::new();
    let plan = Plan {
        lists: &lists,
        groups: &groups,
        branch_targets: &targets,
        detail_keys: &detail_keys,
    };
    let outcome = refresh::run(&plan, RECONCILE_REFRESH_ROUNDS, |requests| {
        fetch_graphql(context, &cwd, requests)
    })?;
    context.check_cancelled()?;
    let tasks: Vec<TaskGroup> = groups
        .iter()
        .map(|group| TaskGroup {
            members: group.clone(),
            resolved: true,
        })
        .collect();
    let updates = update_groups(context, &tasks, true, |fresh| {
        refresh::apply(fresh, &groups, &outcome);
        Ok(())
    })?;
    Ok(updates.iter().any(|update| update.changed))
}

fn reconcile(context: &ExecutionContext) -> Result<Value, RequestError> {
    let _reconciliation = context
        .durable_state_locks
        .reconciliation
        .try_lock()
        .map_err(|_| invalid_state("reconciliation is already running"))?;
    context.check_cancelled()?;
    let started_at = unix_timestamp_ms();
    let attempt_id = format!(
        "reconcile-{started_at}-{}",
        context.callback_sequence.fetch_add(1, Ordering::Relaxed)
    );
    begin_reconciliation(context, &attempt_id, started_at)?;

    let mut checked = 0_u64;
    let work = reconcile_groups(context, &mut checked);

    let finished_at = unix_timestamp_ms();
    let error = match work {
        Ok(changed) if !context.cancelled.load(Ordering::SeqCst) => {
            match finish_reconciliation(
                context,
                false,
                &attempt_id,
                "idle",
                checked,
                finished_at,
                None,
            ) {
                Ok(()) => {
                    return Ok(
                        json!({ "checked": checked, "attempt_id": attempt_id, "changed": changed }),
                    )
                }
                Err(error) => error,
            }
        }
        Ok(_) => RequestError::Cancelled,
        Err(error) => error,
    };
    let message = match &error {
        RequestError::Cancelled => "request cancelled".to_string(),
        RequestError::Message(message) => message.clone(),
    };
    finish_reconciliation(
        context,
        true,
        &attempt_id,
        "failed",
        checked,
        finished_at,
        Some(&message),
    )?;
    Err(error)
}

struct CommandResult {
    success: bool,
    status: String,
    program: String,
    stdout: String,
    stderr: String,
}

impl CommandResult {
    fn failure_message(&self) -> String {
        if self.stderr.trim().is_empty() {
            format!("{} exited with {}", self.program, self.status)
        } else {
            self.stderr.trim().to_string()
        }
    }
}

fn run_command<'a>(
    context: &ExecutionContext,
    cwd: &str,
    program: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<CommandResult, RequestError> {
    context.check_cancelled()?;
    let program_name = program.to_string_lossy().to_string();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| invalid_state(format!("failed to run {program_name}: {error}")))?;
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
                return Err(invalid_state(format!(
                    "failed waiting for {program_name}: {error}"
                )));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| invalid_state(format!("failed collecting {program_name} stdout")))?
        .map_err(|error| invalid_state(format!("failed reading {program_name} stdout: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| invalid_state(format!("failed collecting {program_name} stderr")))?
        .map_err(|error| invalid_state(format!("failed reading {program_name} stderr: {error}")))?;
    if stdout.truncated {
        return Err(invalid_state(format!(
            "{program_name} stdout exceeded {MAX_COMMAND_OUTPUT_BYTES} bytes"
        )));
    }
    let mut stderr_text = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
    if stderr.truncated {
        stderr_text.push_str("\n[stderr truncated]");
    }
    Ok(CommandResult {
        success: status.success(),
        status: status.to_string(),
        program: program_name,
        stdout: String::from_utf8_lossy(&stdout.bytes).to_string(),
        stderr: stderr_text,
    })
}

fn command_output<'a>(
    context: &ExecutionContext,
    cwd: &str,
    program: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, RequestError> {
    let output = run_command(context, cwd, program, args)?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(RequestError::Message(output.failure_message()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;
    use std::sync::mpsc::Receiver;

    /// Only bounds a hung test; fake subprocesses can take seconds when the suite runs in parallel.
    const TEST_WAIT: Duration = Duration::from_secs(10);
    use std::time::Instant;

    #[test]
    fn recognizes_existing_github_remote_forms_only() {
        assert!(is_github_remote("git@github.com:planeai/planeai.git"));
        assert!(is_github_remote("ssh://git@github.com/planeai/planeai.git"));
        assert!(is_github_remote("https://github.com/planeai/planeai.git"));
        assert!(!is_github_remote("git@gitlab.com:planeai/planeai.git"));
    }

    #[test]
    fn extracts_actions_run_ids_of_the_pull_request_repository_only() {
        for (url, expected) in [
            (
                "https://github.com/o/r/actions/runs/123/job/45",
                Some("123"),
            ),
            ("https://github.com/O/R/actions/runs/123", Some("123")),
            (
                "https://github.com/o/r/actions/runs/123?check_suite_focus=true",
                Some("123"),
            ),
            ("https://github.com/x/y/actions/runs/123", None),
            ("https://github.com/o/r-other/actions/runs/123", None),
            ("https://ci.example.com/o/r/actions/runs/123", None),
            (
                "https://evil.example/https://github.com/o/r/actions/runs/123",
                None,
            ),
            ("http://github.com/o/r/actions/runs/123", None),
            ("https://github.com/o/r/actions/runs/--help", None),
            ("https://github.com/o/r/actions/runs/", None),
            ("https://github.com/o/r/pull/1", None),
        ] {
            assert_eq!(action_run_id(url, "o/r"), expected, "{url}");
        }
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
        let callback = receive_frame(&output_rx);
        let callback_id = callback["id"].as_str().unwrap();
        assert_eq!(callback["method"], "host.sessions.list");
        input_tx
            .send(
                json!({ "jsonrpc": "2.0", "id": callback_id, "result": { "sessions": [] } })
                    .to_string(),
            )
            .unwrap();
        for (path, settings) in [
            (json!(["github", "version"]), json!(STATE_VERSION)),
            (json!(["github", "pull_requests"]), Value::Null),
        ] {
            let callback = receive_frame(&output_rx);
            let callback_id = callback["id"].as_str().unwrap();
            assert_eq!(callback["method"], "host.settings.get");
            assert_eq!(callback["params"], json!({ "path": path }));
            input_tx
                .send(
                    json!({ "jsonrpc": "2.0", "id": callback_id, "result": { "settings": settings } })
                        .to_string(),
                )
                .unwrap();
        }
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

    /// Starts `github.reconcile` against a fake host and stops at its `host.sessions.list` call,
    /// returning the host's input, the plugin's output, and whether the plugin exited cleanly.
    #[cfg(unix)]
    fn reconcile_until_sessions_list() -> (Sender<String>, Receiver<String>, Receiver<bool>) {
        let (input_tx, input_rx) = mpsc::channel();
        let (output_tx, output_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let result = run_with_runner(
                ChannelInput::new(input_rx),
                LineOutput::new(output_tx),
                CommandRunner::default(),
            );
            let _ = done_tx.send(result.is_ok());
        });

        input_tx
            .send(json!({ "jsonrpc": "2.0", "id": "reconcile-1", "method": "github.reconcile", "params": {} }).to_string())
            .unwrap();
        let mut settings = json!({});
        loop {
            let callback = receive_frame(&output_rx);
            let result = match callback["method"].as_str().unwrap() {
                "host.settings.get" => {
                    json!({ "settings": settings_at(&settings, &callback["params"]["path"]) })
                }
                "host.settings.patch" => {
                    merge_settings_patch(&mut settings, &callback["params"]["patch"]);
                    json!({ "updated": true })
                }
                "host.sessions.list" => return (input_tx, output_rx, done_rx),
                other => panic!("unexpected host call {other}"),
            };
            input_tx
                .send(
                    json!({ "jsonrpc": "2.0", "id": callback["id"], "result": result }).to_string(),
                )
                .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn fails_reconciliation_and_exits_when_host_input_closes_during_a_host_call() {
        let (input_tx, output_rx, done_rx) = reconcile_until_sessions_list();
        drop(input_tx);

        let response = receive_frame(&output_rx);
        assert_eq!(response["id"], "reconcile-1");
        assert_eq!(
            response["error"],
            json!({ "code": -32000, "message": "host connection closed" })
        );
        assert_eq!(
            done_rx.recv_timeout(TEST_WAIT),
            Ok(true),
            "plugin kept running after host input closed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn winds_down_cleanly_when_host_output_breaks_before_input_closes() {
        let (input_tx, output_rx, done_rx) = reconcile_until_sessions_list();
        drop(output_rx);
        input_tx
            .send(
                json!({ "jsonrpc": "2.0", "id": "shutdown-1", "method": "plugin.shutdown" })
                    .to_string(),
            )
            .unwrap();

        assert_eq!(done_rx.recv_timeout(TEST_WAIT), Ok(true));
    }

    #[cfg(unix)]
    #[test]
    fn winds_down_cleanly_when_host_output_is_also_gone() {
        let (input_tx, output_rx, done_rx) = reconcile_until_sessions_list();
        drop(output_rx);
        drop(input_tx);

        assert_eq!(done_rx.recv_timeout(TEST_WAIT), Ok(true));
    }

    #[cfg(unix)]
    #[test]
    fn sends_failure_logs_to_the_currently_focused_agent_through_host_callback() {
        let directory = TestDirectory::new();
        let gh_args = directory.0.join("gh-args");
        let failed = |url: &str| json!({ "__typename": "CheckRun", "name": "fmt", "status": "COMPLETED", "conclusion": "FAILURE", "detailsUrl": url });
        let mut pr = graphql_pr("o/r", 456, "OPEN", "topic", "main", "");
        pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({ "state": "FAILURE", "contexts": { "nodes": [
            failed("https://github.com/x/y/actions/runs/999/job/1"),
            failed("https://github.com/o/r/actions/runs/123/job/45"),
        ] } });
        fs::write(
            directory.0.join("graphql.json"),
            json!({ "data": { "r0": { "l0": pr } } }).to_string(),
        )
        .unwrap();
        let gh = directory.script(
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$@" >> '{args}'
if [ "$1" = "api" ]; then
  cat '{root}/graphql.json'
else
  printf 'checkRun cargo fmt --check\nDiff in src-tauri/src/plugins.rs\n'
fi
"#,
                args = gh_args.display(),
                root = directory.path(),
            ),
        );
        let git = directory.script("git", "#!/bin/sh\nprintf 'https://github.com/o/r.git\\n'\n");
        let (events, events_rx) = mpsc::channel();
        let context = ExecutionContext {
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            runner: CommandRunner::new(&git, &gh),
            request_key: "test-request".to_string(),
            callback_sequence: Arc::new(AtomicU64::new(1)),
            durable_state_locks: DurableStateLocks {
                reconciliation: Arc::new(Mutex::new(())),
                pull_requests: Arc::new(Mutex::new(())),
            },
        };
        let worker = thread::spawn(move || {
            send_failure_logs(
                &json!({
                    "session_id": "pull-request-session",
                    "recipient_session_id": "currently-focused-agent-session",
                    "url": "https://github.com/o/r/pull/456",
                }),
                &context,
            )
        });

        let ControllerEvent::HostCall {
            method,
            params,
            response,
            ..
        } = events_rx.recv_timeout(TEST_WAIT).unwrap()
        else {
            panic!("expected repository-context callback");
        };
        assert_eq!(method, "host.sessions.repositoryContext");
        assert_eq!(params, json!({ "session_id": "pull-request-session" }));
        response
            .send(Ok(
                json!({ "working_tree_path": directory.path(), "branch": "topic" }),
            ))
            .unwrap();

        let ControllerEvent::HostCall {
            method,
            params,
            response,
            ..
        } = events_rx.recv_timeout(TEST_WAIT).unwrap()
        else {
            panic!("expected session-prompt callback");
        };
        assert_eq!(method, "host.sessions.prompt");
        let args = fs::read_to_string(&gh_args).unwrap();
        assert!(
            args.starts_with("api\ngraphql\n"),
            "the checks come from one detail lookup"
        );
        assert!(
            args.ends_with("run\nview\n123\n--log-failed\n-R\no/r\n"),
            "the run is read from the pull request's repository, which can differ from the checkout, and a run elsewhere is ignored"
        );
        assert!(params["text"]
            .as_str()
            .unwrap()
            .starts_with("CI failed on o/r#456."));
        assert_eq!(params["session_id"], "currently-focused-agent-session");
        assert!(params["text"]
            .as_str()
            .unwrap()
            .contains("checkRun cargo fmt --check"));
        response.send(Ok(json!({ "delivered": true }))).unwrap();

        assert_eq!(worker.join().unwrap().unwrap(), json!({ "sent": true }));
    }

    #[test]
    fn default_pr_title_prefers_linked_task_title_and_has_session_and_branch_fallbacks() {
        assert_eq!(
            default_pr_title(
                "PLA-42: session name",
                "feature/branch",
                Some("PLA-42"),
                Some("Task title".to_string())
            ),
            "Task title [PLA-42]"
        );
        assert_eq!(
            default_pr_title(
                "PLA-42: session name",
                "feature/branch",
                Some("PLA-42"),
                None
            ),
            "session name [PLA-42]"
        );
        assert_eq!(
            default_pr_title("", "feature/branch", Some("PLA-42"), None),
            "feature/branch [PLA-42]"
        );
    }

    #[test]
    fn extracts_github_repository_slugs() {
        assert_eq!(
            github_repository_slug("git@github.com:o/r.git"),
            Some("o/r".to_string())
        );
        assert_eq!(github_repository_slug("git@gitlab.com:o/r.git"), None);
    }

    #[test]
    fn handshake_identity_matches_manifest() {
        let manifest: Value = serde_json::from_str(include_str!("../planeai-plugin.json")).unwrap();
        let response = handshake(&json!({ "host_api_version": HOST_API_VERSION })).unwrap();

        assert_eq!(response["plugin_id"], manifest["id"]);
        assert_eq!(response["plugin_name"], manifest["name"]);
        assert_eq!(response["plugin_version"], manifest["version"]);
        assert_eq!(response["host_api_version"], manifest["host_api_version"]);
    }

    /// An in-process PlaneAI host: deep-merged settings, a session list, and recorded task transitions.
    struct FakeHost {
        settings: Value,
        sessions: Value,
        contexts: HashMap<String, Value>,
        transitions: Vec<(String, String)>,
        /// Every host method called, in order.
        calls: Vec<String>,
    }

    fn settings_at(settings: &Value, path: &Value) -> Value {
        path.as_array()
            .unwrap()
            .iter()
            .try_fold(settings, |value, key| value.get(key.as_str().unwrap()))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn merge_settings_patch(target: &mut Value, patch: &Value) {
        let target = target.as_object_mut().unwrap();
        for (key, value) in patch.as_object().unwrap() {
            if value.is_null() {
                target.remove(key);
            } else if value.is_object() {
                let entry = target.entry(key.clone()).or_insert_with(|| json!({}));
                if !entry.is_object() {
                    *entry = json!({});
                }
                merge_settings_patch(entry, value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    }

    fn serve_host(
        events: Receiver<ControllerEvent>,
        host: Arc<Mutex<FakeHost>>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            while let Ok(ControllerEvent::HostCall {
                method,
                params,
                response,
                ..
            }) = events.recv()
            {
                let mut host = host.lock().unwrap();
                host.calls.push(method.clone());
                let result = match method.as_str() {
                    "host.settings.get" => {
                        json!({ "settings": settings_at(&host.settings, &params["path"]) })
                    }
                    "host.settings.patch" => {
                        merge_settings_patch(&mut host.settings, &params["patch"]);
                        json!({ "updated": true })
                    }
                    "host.sessions.list" if host.sessions.is_null() => {
                        let _ = response.send(Err(RequestError::Message(
                            "sessions unavailable".to_string(),
                        )));
                        continue;
                    }
                    "host.sessions.list" => host.sessions.clone(),
                    "host.sessions.repositoryContext" => {
                        host.contexts[params["session_id"].as_str().unwrap()].clone()
                    }
                    "host.sessions.transitionLinkedTask" => {
                        let transition = (
                            params["session_id"].as_str().unwrap().to_string(),
                            params["status"].as_str().unwrap().to_string(),
                        );
                        host.transitions.push(transition);
                        json!({ "transitioned": true })
                    }
                    other => panic!("unexpected host call {other}"),
                };
                let _ = response.send(Ok(result));
            }
        })
    }

    /// Ends each `gh` call's arguments in the fake's argument log.
    #[cfg(unix)]
    const INVOCATION_END: &str = "--end-of-invocation--";

    /// Fake `git` and `gh`: the remote follows the working tree. `gh api` serves the queued
    /// `gh-response-*` answers in order, then empty data; `gh pr create` prints `gh-pr-create`;
    /// every call's arguments go to `gh-args`, each call ending with `INVOCATION_END`.
    #[cfg(unix)]
    struct FakeGithubCli {
        directory: TestDirectory,
        runner: CommandRunner,
    }

    #[cfg(unix)]
    impl FakeGithubCli {
        fn new() -> Self {
            let directory = TestDirectory::new();
            fs::create_dir_all(directory.0.join("repo")).unwrap();
            fs::create_dir_all(directory.0.join("other-repo")).unwrap();
            fs::create_dir_all(directory.0.join("gitlab-repo")).unwrap();
            let git = directory.script(
                "git",
                "#!/bin/sh\ncase \"$(pwd)\" in *other-repo) echo https://github.com/x/y.git ;; *gitlab-repo) echo https://gitlab.com/o/r.git ;; *) echo https://github.com/o/r.git ;; esac\n",
            );
            let root = directory.path().to_string();
            let gh = directory.script(
                "gh",
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" '{INVOCATION_END}' >> '{root}/gh-args'\nprintf x >> '{root}/gh-calls'\nif [ \"$1\" = api ]; then next=$(ls '{root}'/gh-response-* 2>/dev/null | head -n 1); if [ -n \"$next\" ]; then cat \"$next\"; rm \"$next\"; else printf '{{\"data\":{{}}}}'; fi; elif [ \"$1 $2\" = 'pr create' ]; then cat '{root}/gh-pr-create'; fi\n"
                ),
            );
            Self {
                runner: CommandRunner::new(git, gh),
                directory,
            }
        }

        /// Queues the answer to the next `gh api graphql` call that has none yet.
        fn respond(&self, body: Value) {
            let queued = fs::read_dir(&self.directory.0)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("gh-response-")
                })
                .count();
            let path = self
                .directory
                .0
                .join(format!("gh-response-{:04}", self.calls() + queued));
            fs::write(path, body.to_string()).unwrap();
        }

        /// The arguments of each `gh` call, in order.
        fn invocations(&self) -> Vec<Vec<String>> {
            let args = fs::read_to_string(self.directory.0.join("gh-args")).unwrap_or_default();
            let mut invocations = vec![Vec::new()];
            for line in args.lines() {
                if line == INVOCATION_END {
                    invocations.push(Vec::new());
                } else {
                    invocations.last_mut().unwrap().push(line.to_string());
                }
            }
            invocations.pop();
            invocations
        }

        /// The calls that start with these arguments.
        fn invocations_of(&self, command: &[&str]) -> Vec<Vec<String>> {
            self.invocations()
                .into_iter()
                .filter(|args| args.len() >= command.len() && args[..command.len()] == *command)
                .collect()
        }

        fn calls(&self) -> usize {
            fs::read_to_string(self.directory.0.join("gh-calls"))
                .map(|calls| calls.len())
                .unwrap_or(0)
        }

        fn context(
            &self,
            session_id: &str,
            repository: &str,
            branch: &str,
            task_key: Option<&str>,
        ) -> Value {
            json!({
                "session_id": session_id, "project_id": "p", "branch": branch, "base_branch": "main",
                "working_tree_path": self.directory.0.join(repository), "linked_task_key": task_key,
            })
        }
    }

    #[cfg(unix)]
    fn graphql_pr(
        repository: &str,
        number: u64,
        state: &str,
        head: &str,
        base: &str,
        rollup: &str,
    ) -> Value {
        json!({
            "number": number, "url": format!("https://github.com/{repository}/pull/{number}"), "title": format!("Change {number}"),
            "state": state, "isDraft": false, "updatedAt": format!("2026-10-{:02}T00:00:00Z", number), "headRefName": head,
            "baseRefName": base, "isCrossRepository": false,
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": if rollup.is_empty() { Value::Null } else { json!({ "state": rollup }) } } }] },
        })
    }

    #[cfg(unix)]
    fn with_host<T>(
        runner: &CommandRunner,
        host: &Arc<Mutex<FakeHost>>,
        operation: impl FnOnce(&ExecutionContext) -> T,
    ) -> T {
        let (events, events_rx) = mpsc::channel();
        let server = serve_host(events_rx, Arc::clone(host));
        let context = ExecutionContext {
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            runner: runner.clone(),
            request_key: "test-request".to_string(),
            callback_sequence: Arc::new(AtomicU64::new(1)),
            durable_state_locks: DurableStateLocks {
                reconciliation: Arc::new(Mutex::new(())),
                pull_requests: Arc::new(Mutex::new(())),
            },
        };
        let result = operation(&context);
        drop(context);
        server.join().unwrap();
        result
    }

    #[cfg(unix)]
    fn numbers(value: &Value) -> Vec<u64> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|pr| pr["number"].as_u64().unwrap())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn status_migrates_legacy_mappings_unions_the_task_and_transitions_once() {
        let cli = FakeGithubCli::new();
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["version"] = json!(3);
        settings["github"]["task_transitions"]["on_open"] = json!("in_review");
        settings["github"]["pull_requests"] = json!({
            "s1": { "session_id": "s1", "url": "https://github.com/o/r/pull/3", "state": "draft", "remote": "https://github.com/o/r.git", "branch": "topic", "updated_at": 1, "check_summary": null },
            "s2": { "session_id": "s2", "url": "https://github.com/x/y/pull/8", "state": "closed", "updated_at": 1, "check_summary": null },
        });
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [
                { "id": "s1", "project_id": "p", "task_key": "T-1", "status": "active", "branch": "topic" },
                { "id": "s2", "project_id": "p", "task_key": "T-1", "status": "archived", "branch": "other" },
                { "id": "s3", "project_id": "p", "task_key": "T-2", "status": "active", "branch": "unrelated" },
            ] }),
            contexts: HashMap::from([(
                "s1".to_string(),
                cli.context("s1", "repo", "topic", Some("T-1")),
            )]),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));
        let round = json!({ "data": {
            "r0": {
                "squashMergeAllowed": true, "mergeCommitAllowed": true, "rebaseMergeAllowed": false,
                "l0": graphql_pr("o/r", 3, "OPEN", "feature", "main", ""),
                "l2": { "nodes": [graphql_pr("o/r", 4, "OPEN", "topic", "main", "FAILURE")] },
            },
            "r1": { "l1": graphql_pr("x/y", 8, "MERGED", "other", "main", "SUCCESS") },
        } });

        cli.respond(round.clone());
        let first = with_host(&cli.runner, &host, |context| {
            status(&json!({ "session_id": "s1" }), context)
        })
        .unwrap();
        assert_eq!(
            numbers(&first["pull_requests"]),
            vec![3, 4, 8],
            "the archived sibling's PR is part of the task"
        );
        assert_eq!(first["repository"], "o/r");
        assert_eq!(
            first["own_url"], "https://github.com/o/r/pull/4",
            "the branch PR is the session's own"
        );
        assert_eq!(
            numbers(&first["details"]),
            vec![3, 4, 8],
            "every PR of the task has details for the panel"
        );
        let own_detail = first["details"]
            .as_array()
            .unwrap()
            .iter()
            .find(|detail| detail["number"] == 4)
            .unwrap();
        assert_eq!(own_detail["merge_methods"], json!(["squash", "merge"]));
        assert_eq!(first["changed"], true);
        assert_eq!(
            cli.calls(),
            2,
            "one batched query, then one for the stack neighbors it revealed"
        );

        let stored = host.lock().unwrap().settings["github"].clone();
        assert_eq!(stored["version"], STATE_VERSION);
        assert_eq!(stored["pull_requests"]["s1"][0]["source"], "manual");
        assert_eq!(stored["pull_requests"]["s1"][0]["state"], "open");
        assert_eq!(stored["pull_requests"]["s1"][1]["source"], "branch");
        assert_eq!(stored["pull_requests"]["s1"][1]["checks"], "failing");
        assert_eq!(stored["pull_requests"]["s2"][0]["state"], "merged");
        assert_eq!(
            host.lock().unwrap().transitions,
            vec![("s1".to_string(), "in_review".to_string())]
        );

        cli.respond(round);
        let second = with_host(&cli.runner, &host, |context| {
            status(&json!({ "session_id": "s1" }), context)
        })
        .unwrap();
        assert_eq!(second["changed"], false);
        assert_eq!(second["pull_requests"], first["pull_requests"]);
        assert_eq!(
            host.lock().unwrap().transitions.len(),
            1,
            "an unchanged state never repeats a transition"
        );

        let first = &cli.invocations()[0];
        assert_eq!(first[..3], ["api", "graphql", "-f"]);
        assert!(first[3].starts_with("query=query { r0: repository(owner: \"o\", name: \"r\")"));
    }

    #[cfg(unix)]
    #[test]
    fn link_appends_and_unlink_or_dismiss_writes_task_wide_tombstones() {
        let cli = FakeGithubCli::new();
        let mut settings = json!({ "github": empty_durable_state() });
        let open = |number: u64, repository: &str, source: &str| {
            json!({
                "host": "github.com", "repository": repository, "number": number, "url": format!("https://github.com/{repository}/pull/{number}"),
                "source": source, "state": "open", "refreshed_at": 1,
            })
        };
        settings["github"]["pull_requests"] =
            json!({ "s1": [open(4, "o/r", "branch")], "s2": [open(9, "o/r", "stack")] });
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [
                { "id": "s1", "project_id": "p", "task_key": "T-1", "status": "active" },
                { "id": "s2", "project_id": "p", "task_key": "T-1", "status": "active" },
            ] }),
            contexts: HashMap::from([(
                "s1".to_string(),
                cli.context("s1", "repo", "topic", Some("T-1")),
            )]),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));

        cli.respond(json!({ "data": { "r0": { "l0": graphql_pr("other/repo", 12, "OPEN", "x", "main", "PENDING") } } }));
        let linked = with_host(&cli.runner, &host, |context| {
            link_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/Other/Repo/pull/12" }),
                context,
            )
        })
        .unwrap();
        assert_eq!(
            numbers(&linked["pull_requests"]),
            vec![4, 9, 12],
            "linking appends instead of replacing"
        );
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][1]["source"],
            "manual"
        );

        let unlink = |url: &str| {
            with_host(&cli.runner, &host, |context| {
                unlink_pull_request(&json!({ "session_id": "s1", "url": url }), context)
            })
        };
        assert_eq!(
            numbers(&unlink("https://github.com/other/repo/pull/12").unwrap()["pull_requests"]),
            vec![4, 9]
        );
        assert_eq!(
            numbers(&unlink("https://github.com/o/r/pull/9").unwrap()["pull_requests"]),
            vec![4],
            "a sibling's stack PR is dismissed"
        );
        assert!(
            unlink("https://github.com/o/r/pull/4").is_err(),
            "branch PRs follow the branch"
        );
        assert!(
            unlink("https://github.com/o/r/pull/9").is_err(),
            "a dismissed PR cannot be dismissed again"
        );
        let stored = host.lock().unwrap().settings["github"]["pull_requests"].clone();
        assert_eq!(stored["s1"][1]["source"], "unlinked");
        assert_eq!(
            stored["s2"][0],
            json!({ "host": "github.com", "repository": "o/r", "number": 9, "url": "https://github.com/o/r/pull/9", "source": "stack-dismissed" })
        );
        assert_eq!(cli.calls(), 1, "unlinking never asks GitHub");
    }

    #[cfg(unix)]
    #[test]
    fn the_link_tool_links_for_the_calling_session_and_reports_failures_to_the_agent() {
        let cli = FakeGithubCli::new();
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["pull_requests"] = json!({ "s1": [stored_link(4, "branch", "open")] });
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [{ "id": "s1", "project_id": "p", "task_key": "T-1", "status": "active" }] }),
            contexts: HashMap::from([(
                "s1".to_string(),
                cli.context("s1", "repo", "topic", Some("T-1")),
            )]),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));
        let call = |tool: &str, arguments: Value| {
            with_host(&cli.runner, &host, |context| {
                dispatch(
                    "tool.call",
                    json!({
                        "tool": tool,
                        "arguments": arguments,
                        "caller": {
                            "session_id": "s1",
                            "project": { "id": "p", "name": "Demo" },
                            "task": null,
                            "auto_approve": false,
                        },
                    }),
                    context,
                )
            })
            .unwrap()
        };

        cli.respond(json!({ "data": { "r0": { "l0": graphql_pr("other/repo", 12, "OPEN", "x", "main", "PENDING") } } }));
        assert_eq!(
            call(
                LINK_PULL_REQUEST_TOOL,
                json!({ "url": "https://github.com/Other/Repo/pull/12" })
            ),
            json!({ "text": "Linked other/repo#12 (open): Change 12.\nThe session's task has 2 pull requests: o/r#4, other/repo#12." })
        );
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][1]["source"],
            "manual"
        );

        assert_eq!(
            call(
                LINK_PULL_REQUEST_TOOL,
                json!({ "url": "https://gitlab.com/o/r/-/merge_requests/1" })
            ),
            json!({ "text": "Enter a github.com pull request URL, such as https://github.com/owner/repo/pull/123.", "is_error": true })
        );
        assert_eq!(
            call(LINK_PULL_REQUEST_TOOL, json!({})),
            json!({ "text": "url is required", "is_error": true })
        );
        assert_eq!(
            call("close_everything", json!({})),
            json!({ "text": "unknown tool close_everything", "is_error": true })
        );
        assert_eq!(cli.calls(), 1, "a refused call never asks GitHub");
    }

    #[cfg(unix)]
    fn stored_link(number: u64, source: &str, state: &str) -> Value {
        json!({
            "host": "github.com", "repository": "o/r", "number": number, "url": format!("https://github.com/o/r/pull/{number}"),
            "source": source, "state": state, "refreshed_at": 1,
        })
    }

    #[cfg(unix)]
    fn merge_transition_host(
        cli: &FakeGithubCli,
        links: Value,
        branch: &str,
    ) -> Arc<Mutex<FakeHost>> {
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["task_transitions"]["on_merge"] = json!("done");
        settings["github"]["pull_requests"] = json!({ "s1": links });
        Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [{ "id": "s1", "project_id": "p", "task_key": "T-1", "status": "active" }] }),
            contexts: HashMap::from([(
                "s1".to_string(),
                cli.context("s1", "repo", branch, Some("T-1")),
            )]),
            transitions: Vec::new(),
            calls: Vec::new(),
        }))
    }

    #[cfg(unix)]
    #[test]
    fn unlinking_the_last_open_pull_request_after_a_merge_finishes_the_task() {
        let cli = FakeGithubCli::new();
        let mut open = stored_link(4, "manual", "open");
        open["created_at"] = json!("2026-10-02T00:00:00Z");
        let mut merged = stored_link(5, "manual", "merged");
        merged["merged_at"] = json!("2026-10-03T00:00:00Z");
        let host = merge_transition_host(&cli, json!([open, merged]), "topic");
        with_host(&cli.runner, &host, |context| {
            unlink_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/4" }),
                context,
            )
        })
        .unwrap();
        assert_eq!(
            host.lock().unwrap().transitions,
            vec![("s1".to_string(), "done".to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_branch_moving_away_keeps_its_open_pull_request_and_the_task_open() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(
            &cli,
            json!([
                stored_link(4, "branch", "open"),
                stored_link(5, "manual", "merged")
            ]),
            "other",
        );
        cli.respond(json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("o/r", 4, "OPEN", "topic", "main", ""),
            "l1": graphql_pr("o/r", 5, "MERGED", "done", "main", ""),
            "l2": { "nodes": [] },
        } } }));
        let result = with_host(&cli.runner, &host, |context| {
            status(&json!({ "session_id": "s1" }), context)
        })
        .unwrap();
        assert_eq!(
            numbers(&result["pull_requests"]),
            vec![4, 5],
            "the old branch pull request stays with the task"
        );
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0]["source"],
            "manual",
            "and can be unlinked now that the branch no longer follows it"
        );
        assert!(
            host.lock().unwrap().transitions.is_empty(),
            "the pull request is still open on GitHub"
        );
    }

    #[cfg(unix)]
    #[test]
    fn actions_fail_before_any_change_when_the_tasks_sessions_cannot_be_listed() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "manual", "open")]), "topic");
        host.lock().unwrap().sessions = Value::Null;
        let before = host.lock().unwrap().settings.clone();
        let failure = |result: Result<Value, RequestError>| match result {
            Err(RequestError::Message(message)) => message,
            other => panic!("expected an error, got {other:?}"),
        };
        let link = with_host(&cli.runner, &host, |context| {
            link_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/12" }),
                context,
            )
        });
        assert_eq!(
            failure(link),
            "Could not list the task's sessions; try again."
        );
        let merge = with_host(&cli.runner, &host, |context| {
            merge_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/4", "strategy": "squash" }),
                context,
            )
        });
        assert_eq!(
            failure(merge),
            "Could not list the task's sessions; try again."
        );
        let unlink = with_host(&cli.runner, &host, |context| {
            unlink_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/4" }),
                context,
            )
        });
        assert_eq!(
            failure(unlink),
            "Could not list the task's sessions; try again."
        );
        assert_eq!(cli.calls(), 0, "nothing happens on GitHub either");
        assert_eq!(host.lock().unwrap().settings, before);
        assert!(host.lock().unwrap().transitions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_skips_a_task_whose_stored_links_cannot_be_read() {
        let cli = FakeGithubCli::new();
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["pull_requests"] = json!({
            "s1": [stored_link(4, "manual", "open")],
            "s2": [{ "host": "github.com", "repository": "o/r", "number": 5, "url": "https://github.com/o/r/pull/5", "source": "unknown" }],
        });
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [
                { "id": "s1", "project_id": "p", "task_key": "T-1", "status": "active" },
                { "id": "s2", "project_id": "p", "task_key": "T-2", "status": "active" },
            ] }),
            contexts: HashMap::from([
                (
                    "s1".to_string(),
                    cli.context("s1", "repo", "topic", Some("T-1")),
                ),
                (
                    "s2".to_string(),
                    cli.context("s2", "repo", "other", Some("T-2")),
                ),
            ]),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));
        cli.respond(json!({ "data": { "r0": {
            "l0": graphql_pr("o/r", 4, "MERGED", "feature", "main", ""),
            "l1": { "nodes": [] },
        } } }));

        let result = with_host(&cli.runner, &host, reconcile).unwrap();
        assert_eq!(result["checked"], 2);
        let stored = host.lock().unwrap().settings["github"].clone();
        assert_eq!(stored["reconciliation"]["status"], "idle");
        assert_eq!(stored["pull_requests"]["s1"][0]["state"], "merged");
        assert_eq!(
            stored["pull_requests"]["s2"][0]["source"], "unknown",
            "left as it was"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_task_reads_its_stored_links_once_however_many_sessions_it_has() {
        let cli = FakeGithubCli::new();
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["pull_requests"] = json!({
            "s1": [stored_link(4, "manual", "open")],
            "s2": [stored_link(5, "manual", "open")],
            "s3": [stored_link(6, "manual", "open")],
        });
        let session = |id: &str| json!({ "id": id, "project_id": "p", "task_key": "T-1", "status": "active" });
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [session("s1"), session("s2"), session("s3")] }),
            contexts: HashMap::new(),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));
        let count = |method: &str| {
            let mut host = host.lock().unwrap();
            let count = host.calls.iter().filter(|call| *call == method).count();
            host.calls.clear();
            count
        };

        with_host(&cli.runner, &host, |context| {
            summary(&json!({ "session_id": "s1" }), context)
        })
        .unwrap();
        assert_eq!(
            count("host.settings.get"),
            2,
            "the version, then every link at once"
        );

        with_host(&cli.runner, &host, |context| {
            unlink_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/5" }),
                context,
            )
        })
        .unwrap();
        let reads = count("host.settings.get");
        assert_eq!(reads, 2, "the version once, then every link at once");
    }

    #[test]
    fn migration_waits_for_pull_request_writes_in_flight() {
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["version"] = json!(3);
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [] }),
            contexts: HashMap::new(),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));
        let (events, events_rx) = mpsc::channel();
        let server = serve_host(events_rx, Arc::clone(&host));
        let mut context = test_context(CommandRunner::default());
        context.events = events;
        let pull_requests = Arc::clone(&context.durable_state_locks.pull_requests);
        let write_in_flight = pull_requests.lock().unwrap();
        let migration = thread::spawn(move || ensure_durable_state(&context, false));

        let deadline = Instant::now() + TEST_WAIT;
        while host.lock().unwrap().calls.is_empty() {
            assert!(Instant::now() < deadline, "the migration never started");
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            host.lock().unwrap().settings["github"]["version"],
            3,
            "rewriting the namespace now would undo the write in flight"
        );
        drop(write_in_flight);
        migration.join().unwrap().unwrap();
        server.join().unwrap();
        assert_eq!(
            host.lock().unwrap().settings["github"]["version"],
            STATE_VERSION
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_checks_the_strategy_with_one_graphql_lookup() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "manual", "open")]), "topic");
        let lookup = json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("o/r", 4, "OPEN", "topic", "main", "SUCCESS"),
        } } });
        let merge = |strategy: &str| {
            with_host(&cli.runner, &host, |context| {
                merge_pull_request(
                    &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/4", "strategy": strategy }),
                    context,
                )
            })
        };

        cli.respond(lookup.clone());
        assert!(
            merge("rebase").is_err(),
            "rebase is disabled for the repository"
        );
        cli.respond(lookup);
        let mut merged = graphql_pr("o/r", 4, "MERGED", "topic", "main", "SUCCESS");
        merged["mergedAt"] = json!("2026-10-05T12:00:00Z");
        cli.respond(json!({ "data": { "r0": { "l0": merged } } }));
        assert_eq!(merge("squash").unwrap(), json!({ "merged": true }));

        assert!(
            cli.invocations_of(&["api", "repos/o/r"]).is_empty(),
            "no REST repository lookup"
        );
        assert_eq!(
            cli.invocations_of(&["api", "graphql"]).len(),
            3,
            "one lookup per merge attempt, and one for the merge time"
        );
        assert_eq!(
            cli.invocations_of(&["pr", "merge"]),
            [[
                "pr",
                "merge",
                "https://github.com/o/r/pull/4",
                "--squash",
                "--delete-branch"
            ]]
        );
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0]["state"],
            "merged"
        );
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0]["merged_at"],
            "2026-10-05T12:00:00Z",
            "GitHub's merge time, not this machine's clock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn summary_says_whether_github_applies_only_when_asked() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "branch", "open")]), "topic");
        host.lock().unwrap().contexts.insert(
            "s2".to_string(),
            cli.context("s2", "gitlab-repo", "topic", None),
        );
        let summary_of = |params: Value| {
            with_host(&cli.runner, &host, |context| summary(&params, context)).unwrap()
        };

        let cached = summary_of(json!({ "session_id": "s1" }));
        assert_eq!(numbers(&cached["pull_requests"]), vec![4]);
        assert!(
            cached.get("applicable").is_none(),
            "the sidebar pill never runs git"
        );
        assert_eq!(
            summary_of(json!({ "session_id": "s1", "check_repository": true }))["applicable"],
            true
        );
        assert_eq!(
            summary_of(json!({ "session_id": "s2", "check_repository": true }))["applicable"],
            false
        );
        assert_eq!(cli.calls(), 0, "a summary never asks GitHub");
    }

    #[cfg(unix)]
    #[test]
    fn a_refresh_without_the_task_keeps_the_change_for_one_that_has_it() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "manual", "open")]), "other");
        let sessions = host.lock().unwrap().sessions.clone();
        host.lock().unwrap().sessions = Value::Null;
        let merged = json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("o/r", 4, "MERGED", "topic", "main", ""),
            "l1": { "nodes": [] },
        } } });
        let refresh = || {
            with_host(&cli.runner, &host, |context| {
                status(&json!({ "session_id": "s1" }), context)
            })
            .unwrap()
        };

        cli.respond(merged.clone());
        let unresolved = refresh();
        assert_eq!(
            unresolved["pull_requests"][0]["state"], "merged",
            "the panel still sees what GitHub said"
        );
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0]["state"],
            "open",
            "nothing is stored while the task's other sessions are unknown"
        );
        assert!(host.lock().unwrap().transitions.is_empty());

        host.lock().unwrap().sessions = sessions;
        cli.respond(merged);
        refresh();
        assert_eq!(
            host.lock().unwrap().transitions,
            vec![("s1".to_string(), "done".to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn actions_target_the_url_else_the_sessions_own_pull_request_and_reject_bad_urls() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "manual", "draft")]), "topic");
        let ready =
            |params: Value| with_host(&cli.runner, &host, |context| mark_ready(&params, context));
        assert!(
            ready(json!({ "session_id": "s1", "url": "https://example.com/o/r/pull/4" })).is_err()
        );
        ready(json!({ "session_id": "s1" })).unwrap();
        ready(json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/9" })).unwrap();
        assert_eq!(
            cli.invocations(),
            [
                ["pr", "ready", "https://github.com/o/r/pull/4"],
                ["pr", "ready", "https://github.com/o/r/pull/9"],
            ],
            "the session's own pull request, not its branch, when no url is given"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_merge_github_queues_stays_open_until_it_lands() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "manual", "open")]), "topic");
        let lookup = json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("o/r", 4, "OPEN", "topic", "main", ""),
        } } });
        cli.respond(lookup.clone());
        cli.respond(lookup);
        with_host(&cli.runner, &host, |context| {
            merge_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/4", "strategy": "squash" }),
                context,
            )
        })
        .unwrap();
        assert_eq!(
            host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0]["state"],
            "open",
            "the follow-up lookup is what GitHub says, even when a merge queue holds the pull request"
        );
        assert!(host.lock().unwrap().transitions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_merge_whose_follow_up_lookup_fails_is_stored_as_merged_at_an_unknown_time() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([stored_link(4, "manual", "open")]), "topic");
        cli.respond(json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("o/r", 4, "OPEN", "topic", "main", ""),
        } } }));
        with_host(&cli.runner, &host, |context| {
            merge_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/4", "strategy": "squash" }),
                context,
            )
        })
        .unwrap();
        let stored = host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0].clone();
        assert_eq!(stored["state"], "merged");
        assert!(stored.get("merged_at").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn merging_a_pull_request_from_another_branch_keeps_that_branch() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([]), "topic");
        cli.respond(json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("o/r", 7, "OPEN", "someone-else", "main", ""),
        } } }));
        with_host(&cli.runner, &host, |context| {
            merge_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/7", "strategy": "squash" }),
                context,
            )
        })
        .unwrap();
        let calls = cli.invocations();
        assert_eq!(
            cli.invocations_of(&["pr", "merge"]),
            [["pr", "merge", "https://github.com/o/r/pull/7", "--squash"]]
        );
        assert_eq!(
            cli.invocations_of(&["api", "graphql"]).len(),
            2,
            "the detail lookup, then one for the merge time"
        );
        assert_eq!(calls.last().unwrap()[..2], ["api", "graphql"]);
    }

    #[cfg(unix)]
    #[test]
    fn creating_stores_githubs_creation_time_when_it_can_read_it() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([]), "topic");
        fs::write(
            cli.directory.0.join("gh-pr-create"),
            "https://github.com/o/r/pull/7\n",
        )
        .unwrap();
        let create = || {
            with_host(&cli.runner, &host, |context| {
                create_pull_request(
                    &json!({ "session_id": "s1", "title": "Change", "base_branch": "main" }),
                    context,
                )
            })
        };
        let mut created = graphql_pr("o/r", 7, "OPEN", "topic", "main", "");
        created["createdAt"] = json!("2026-10-04T08:00:00Z");
        cli.respond(json!({ "data": { "r0": { "l0": created } } }));
        create().unwrap();
        let stored = || host.lock().unwrap().settings["github"]["pull_requests"]["s1"][0].clone();
        assert_eq!(stored()["created_at"], "2026-10-04T08:00:00Z");

        host.lock().unwrap().settings["github"]["pull_requests"]["s1"] = json!([]);
        create().expect("a failed follow-up lookup does not fail the creation");
        assert_eq!(stored()["number"], 7);
        assert!(
            stored().get("created_at").is_none(),
            "an unknown time stays unknown"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merging_another_repositorys_pull_request_keeps_its_branch() {
        let cli = FakeGithubCli::new();
        let host = merge_transition_host(&cli, json!([]), "topic");
        cli.respond(json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": false,
            "l0": graphql_pr("x/y", 7, "OPEN", "feature", "main", ""),
        } } }));
        with_host(&cli.runner, &host, |context| {
            merge_pull_request(
                &json!({ "session_id": "s1", "url": "https://github.com/x/y/pull/7", "strategy": "squash" }),
                context,
            )
        })
        .unwrap();
        let calls = cli.invocations();
        assert_eq!(
            cli.invocations_of(&["pr", "merge"]),
            [["pr", "merge", "https://github.com/x/y/pull/7", "--squash"]]
        );
        assert_eq!(
            cli.invocations_of(&["api", "graphql"]).len(),
            2,
            "the detail lookup, then one for the merge time"
        );
        assert_eq!(calls.last().unwrap()[..2], ["api", "graphql"]);
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_refreshes_every_task_in_one_query_and_transitions_when_all_merge() {
        let cli = FakeGithubCli::new();
        let mut settings = json!({ "github": empty_durable_state() });
        settings["github"]["task_transitions"]["on_merge"] = json!("done");
        settings["github"]["pull_requests"] = json!({
            "s1": [{ "host": "github.com", "repository": "o/r", "number": 4, "url": "https://github.com/o/r/pull/4", "source": "branch", "state": "open", "head_branch": "topic", "base_branch": "main", "refreshed_at": 1 }],
            "s2": [{ "host": "github.com", "repository": "x/y", "number": 8, "url": "https://github.com/x/y/pull/8", "source": "manual", "state": "open", "refreshed_at": 1 }],
        });
        let host = Arc::new(Mutex::new(FakeHost {
            settings,
            sessions: json!({ "sessions": [
                { "id": "s1", "project_id": "p", "task_key": "T-1", "status": "active" },
                { "id": "s2", "project_id": "p", "task_key": "T-1", "status": "active" },
                { "id": "s3", "project_id": "p", "status": "archived" },
            ] }),
            contexts: HashMap::from([
                (
                    "s1".to_string(),
                    cli.context("s1", "repo", "topic", Some("T-1")),
                ),
                (
                    "s2".to_string(),
                    cli.context("s2", "other-repo", "other", Some("T-1")),
                ),
            ]),
            transitions: Vec::new(),
            calls: Vec::new(),
        }));
        let merged = json!({ "data": {
            "r0": {
                "l0": graphql_pr("o/r", 4, "MERGED", "topic", "main", "SUCCESS"),
                "l2": { "nodes": [graphql_pr("o/r", 4, "MERGED", "topic", "main", "SUCCESS")] },
                "l4": { "nodes": [] }, "l5": { "nodes": [] },
            },
            "r1": { "l1": graphql_pr("x/y", 8, "MERGED", "feature", "main", ""), "l3": { "nodes": [] } },
        } });

        cli.respond(merged.clone());
        let result = with_host(&cli.runner, &host, reconcile).unwrap();
        assert_eq!(result["checked"], 2);
        assert_eq!(result["changed"], true);
        assert_eq!(cli.calls(), 1, "both repositories share one query");
        let stored = host.lock().unwrap().settings["github"].clone();
        assert_eq!(stored["reconciliation"]["status"], "idle");
        assert_eq!(stored["pull_requests"]["s1"][0]["state"], "merged");
        assert_eq!(stored["pull_requests"]["s2"][0]["state"], "merged");
        assert_eq!(
            host.lock().unwrap().transitions,
            vec![("s1".to_string(), "done".to_string())]
        );

        cli.respond(merged);
        let unchanged = with_host(&cli.runner, &host, reconcile).unwrap();
        assert_eq!(
            unchanged["changed"], false,
            "the host skips its data refresh after an idle reconcile"
        );
        assert_eq!(host.lock().unwrap().transitions.len(), 1);
    }

    fn test_context(runner: CommandRunner) -> ExecutionContext {
        let (events, _events_rx) = mpsc::channel();
        ExecutionContext {
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            runner,
            request_key: "test-request".to_string(),
            callback_sequence: Arc::new(AtomicU64::new(1)),
            durable_state_locks: DurableStateLocks {
                reconciliation: Arc::new(Mutex::new(())),
                pull_requests: Arc::new(Mutex::new(())),
            },
        }
    }

    #[cfg(unix)]
    struct TestDirectory(PathBuf);

    #[cfg(unix)]
    #[test]
    fn test_directories_never_share_a_path() {
        let start = Arc::new(std::sync::Barrier::new(16));
        let directories: Vec<TestDirectory> = (0..16)
            .map(|_| {
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    TestDirectory::new()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let paths: BTreeSet<&PathBuf> = directories.iter().map(|directory| &directory.0).collect();
        assert_eq!(
            paths.len(),
            directories.len(),
            "tests starting together get their own directory"
        );
    }

    #[cfg(unix)]
    impl TestDirectory {
        fn new() -> Self {
            // The clock alone is not unique: macOS reports microseconds, so parallel tests collided.
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "planeai-plugin-github-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
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
            .recv_timeout(TEST_WAIT)
            .expect("expected JSON-RPC frame");
        serde_json::from_str(&line).unwrap()
    }

    #[cfg(unix)]
    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + TEST_WAIT;
        while !path.exists() {
            assert!(Instant::now() < deadline, "fake gh did not start");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
