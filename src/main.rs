mod github_graphql;
mod pull_requests;
mod refresh;

use github_graphql::{FetchedPr, GraphqlResponse, Lookup, LookupRequest};
use pull_requests::{
    aggregate, dismiss, parse_session_links, parse_sessions, record_created, record_manual,
    refresh_snapshot, session_links_value, task_group, task_phase, task_transition, AggregatedPr,
    PrKey, PrSnapshot, PrState, PrStateTransition, PullRequestLink, SessionInfo, TrackedSource,
};
use refresh::{BranchTarget, Plan, SessionLinks};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, RecvTimeoutError, Sender},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PLUGIN_ID: &str = "github";
const PLUGIN_NAME: &str = "GitHub";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const HOST_API_VERSION: &str = "planeai.plugin-host.v5";
const CANCELLATION_ERROR_CODE: i64 = -32800;
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const STATE_NAMESPACE: &str = "github";
const STATE_VERSION: u64 = 4;
const LEGACY_STATE_VERSION: u64 = 1;
const V2_STATE_VERSION: u64 = 2;
const V3_STATE_VERSION: u64 = 3;
/// Lookup rounds per refresh: each round reaches one more pull request along a stack.
const STATUS_REFRESH_ROUNDS: usize = 3;
const RECONCILE_REFRESH_ROUNDS: usize = 2;
const MAX_RECONCILIATION_ERROR_CHARS: usize = 500;
const TASK_STATUSES: &[&str] = &["todo", "in_progress", "in_review", "done"];

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
                durable_state_locks.clone(),
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

fn empty_task_transitions() -> Value {
    json!({ "on_open": Value::Null, "on_merge": Value::Null })
}

fn empty_durable_state() -> Value {
    json!({
        "version": STATE_VERSION,
        "pull_requests": {},
        "reconciliation": {
            "status": "idle",
            "attempt_id": Value::Null,
            "started_at": Value::Null,
            "finished_at": Value::Null,
            "checked": 0,
            "recovered_attempt_id": Value::Null,
            "recovered_at": Value::Null,
            "error": Value::Null,
        },
        "task_transitions": empty_task_transitions(),
    })
}

fn require_only_keys(
    object: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Result<(), RequestError> {
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(RequestError::Message(
            "GitHub durable state contains unknown fields".to_string(),
        ));
    }
    if keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(RequestError::Message(
            "GitHub durable state is missing required fields".to_string(),
        ));
    }
    Ok(())
}

fn strict_optional_string(value: Option<&Value>, field: &str) -> Result<(), RequestError> {
    match value {
        Some(Value::Null) => Ok(()),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(()),
        _ => Err(RequestError::Message(format!(
            "GitHub durable state has invalid {field}"
        ))),
    }
}

fn strict_optional_timestamp(value: Option<&Value>, field: &str) -> Result<(), RequestError> {
    match value {
        Some(Value::Null) => Ok(()),
        Some(Value::Number(number)) if number.as_u64().is_some() => Ok(()),
        _ => Err(RequestError::Message(format!(
            "GitHub durable state has invalid {field}"
        ))),
    }
}

const TASK_TRANSITION_FIELDS: &[&str] = &["on_open", "on_merge"];
const RECONCILIATION_OPTIONAL_FIELDS: &[&str] = &[
    "attempt_id",
    "started_at",
    "finished_at",
    "recovered_attempt_id",
    "recovered_at",
    "error",
];

/// The host's settings patch deletes keys patched to null, so absent optional fields read as null.
fn with_null_defaults(value: &Value, fields: &[&str]) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        for field in fields {
            object.entry(field.to_string()).or_insert(Value::Null);
        }
    }
    value
}

fn validate_task_transitions(value: &Value) -> Result<(), RequestError> {
    let value = with_null_defaults(value, TASK_TRANSITION_FIELDS);
    let transitions = value.as_object().ok_or_else(|| {
        RequestError::Message("GitHub durable state task_transitions must be an object".to_string())
    })?;
    require_only_keys(transitions, &["on_open", "on_merge"])?;
    for field in ["on_open", "on_merge"] {
        match transitions.get(field) {
            Some(Value::Null) => {}
            Some(Value::String(status)) if TASK_STATUSES.contains(&status.as_str()) => {}
            _ => {
                return Err(RequestError::Message(format!(
                    "GitHub durable state has invalid {field} transition"
                )))
            }
        }
    }
    Ok(())
}

fn add_empty_check_summaries(state: &mut Value) -> Result<(), RequestError> {
    let mappings = state
        .get_mut("pull_requests")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            RequestError::Message(
                "GitHub durable state pull_requests must be an object".to_string(),
            )
        })?;
    for mapping in mappings.values_mut() {
        let mapping = mapping.as_object_mut().ok_or_else(|| {
            RequestError::Message("GitHub durable state pull request must be an object".to_string())
        })?;
        mapping
            .entry("check_summary".to_string())
            .or_insert(Value::Null);
    }
    Ok(())
}

fn normalize_task_transitions(state: &mut Value) -> Result<(), RequestError> {
    let object = state.as_object_mut().ok_or_else(|| {
        RequestError::Message("GitHub durable state must be a JSON object".to_string())
    })?;
    let transitions = object
        .entry("task_transitions".to_string())
        .or_insert_with(empty_task_transitions)
        .as_object_mut()
        .ok_or_else(|| {
            RequestError::Message(
                "GitHub durable state task_transitions must be an object".to_string(),
            )
        })?;
    if transitions
        .keys()
        .any(|key| !["on_open", "on_merge"].contains(&key.as_str()))
    {
        return Err(RequestError::Message(
            "GitHub durable state contains unknown fields".to_string(),
        ));
    }
    for key in ["on_open", "on_merge"] {
        transitions.entry(key.to_string()).or_insert(Value::Null);
    }
    Ok(())
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

fn patch_durable_state(
    context: &ExecutionContext,
    allow_cancelled: bool,
    patch: Value,
) -> Result<(), RequestError> {
    if !allow_cancelled {
        context.check_cancelled()?;
    }
    ensure_durable_state(context, allow_cancelled)?;
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

fn reconciliation_from_settings_value(value: Value) -> Result<Value, RequestError> {
    let mut state = empty_durable_state();
    state["reconciliation"] = value;
    validate_durable_state(&state)?;
    Ok(with_null_defaults(
        &state["reconciliation"],
        RECONCILIATION_OPTIONAL_FIELDS,
    ))
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
    reconciliation_from_settings_value(settings_value_from_host_response(&response)?)
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
    let Ok(transitions) = read_task_transitions(context, false) else {
        return;
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

fn begin_reconciliation_patch(
    reconciliation: &Value,
    attempt_id: &str,
    started_at: u64,
) -> Result<Value, RequestError> {
    let reconciliation = reconciliation.as_object().ok_or_else(|| {
        RequestError::Message("GitHub durable state reconciliation must be an object".to_string())
    })?;
    let mut patch = serde_json::Map::new();
    if reconciliation.get("status").and_then(Value::as_str) == Some("running") {
        patch.insert(
            "recovered_attempt_id".to_string(),
            reconciliation
                .get("attempt_id")
                .cloned()
                .unwrap_or(Value::Null),
        );
        patch.insert("recovered_at".to_string(), json!(started_at));
    }
    patch.insert("status".to_string(), json!("running"));
    patch.insert("attempt_id".to_string(), json!(attempt_id));
    patch.insert("started_at".to_string(), json!(started_at));
    patch.insert("finished_at".to_string(), Value::Null);
    patch.insert("checked".to_string(), json!(0));
    patch.insert("error".to_string(), Value::Null);
    Ok(Value::Object(patch))
}

fn finish_reconciliation_patch(
    reconciliation: &Value,
    attempt_id: &str,
    status: &str,
    checked: u64,
    finished_at: u64,
    error: Option<&str>,
) -> Result<Value, RequestError> {
    let reconciliation = reconciliation.as_object().ok_or_else(|| {
        RequestError::Message("GitHub durable state reconciliation must be an object".to_string())
    })?;
    if reconciliation.get("status").and_then(Value::as_str) != Some("running")
        || reconciliation.get("attempt_id").and_then(Value::as_str) != Some(attempt_id)
    {
        return Err(RequestError::Message(
            "reconciliation attempt was superseded".to_string(),
        ));
    }
    Ok(json!({
        "status": status,
        "finished_at": finished_at,
        "checked": checked,
        "error": error.map_or(Value::Null, |error| json!(bounded_error(error))),
    }))
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

fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_RECONCILIATION_ERROR_CHARS).collect()
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

fn enabled_repository_merge_methods(repository: &Value) -> Vec<String> {
    [
        ("squash", "allow_squash_merge"),
        ("merge", "allow_merge_commit"),
        ("rebase", "allow_rebase_merge"),
    ]
    .into_iter()
    .filter_map(|(method, field)| {
        (repository.get(field).and_then(Value::as_bool) == Some(true)).then_some(method.to_string())
    })
    .collect()
}

fn repository_merge_methods(
    context: &ExecutionContext,
    cwd: &str,
    slug: &str,
) -> Result<Vec<String>, RequestError> {
    let raw = gh_output(context, cwd, ["api", &format!("repos/{slug}")])?;
    let repository: Value = serde_json::from_str(&raw).map_err(|error| {
        RequestError::Message(format!(
            "failed to parse GitHub repository settings: {error}"
        ))
    })?;
    Ok(enabled_repository_merge_methods(&repository))
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

fn action_run_id(url: &str) -> Option<&str> {
    let marker = "/actions/runs/";
    let rest = url.get(url.find(marker)? + marker.len()..)?;
    rest.split('/').next()
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
    let transitions = with_null_defaults(
        &settings_value_from_host_response(&response)?,
        TASK_TRANSITION_FIELDS,
    );
    validate_task_transitions(&transitions)?;
    Ok(transitions)
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
    patch_durable_state(context, false, json!({ "task_transitions": transitions }))?;
    Ok(json!({ "task_transitions": transitions }))
}

fn invalid_state(message: impl Into<String>) -> RequestError {
    RequestError::Message(message.into())
}

fn state_object(state: &mut Value) -> Result<&mut serde_json::Map<String, Value>, RequestError> {
    state
        .as_object_mut()
        .ok_or_else(|| invalid_state("GitHub durable state must be a JSON object"))
}

fn migrate_v1_durable_state(state: &mut Value) -> Result<(), RequestError> {
    let object = state_object(state)?;
    require_only_keys(object, &["version", "pull_requests", "reconciliation"])?;
    object.insert("version".to_string(), json!(V2_STATE_VERSION));
    object.insert("task_transitions".to_string(), empty_task_transitions());
    Ok(())
}

fn migrate_v2_durable_state(state: &mut Value) -> Result<(), RequestError> {
    let object = state_object(state)?;
    if object.keys().any(|key| {
        ![
            "version",
            "pull_requests",
            "reconciliation",
            "task_transitions",
        ]
        .contains(&key.as_str())
    }) || !object.contains_key("pull_requests")
        || !object.contains_key("reconciliation")
    {
        return Err(invalid_state("unsupported GitHub durable state version"));
    }
    object.insert("version".to_string(), json!(V3_STATE_VERSION));
    normalize_task_transitions(state)?;
    add_empty_check_summaries(state)
}

/// Version 4 tracks a list of pull requests per session instead of one mapping.
fn migrate_v3_durable_state(state: &mut Value) -> Result<(), RequestError> {
    let object = state_object(state)?;
    let pull_requests = object
        .get_mut("pull_requests")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid_state("GitHub durable state pull_requests must be an object"))?;
    for links in pull_requests.values_mut() {
        *links = session_links_value(&parse_session_links(links).map_err(invalid_state)?);
    }
    object.insert("version".to_string(), json!(STATE_VERSION));
    Ok(())
}

fn migrate_durable_state(state: &Value) -> Result<Value, RequestError> {
    let mut migrated = state.clone();
    loop {
        match migrated.get("version").and_then(Value::as_u64) {
            Some(LEGACY_STATE_VERSION) => migrate_v1_durable_state(&mut migrated)?,
            Some(V2_STATE_VERSION) => migrate_v2_durable_state(&mut migrated)?,
            Some(V3_STATE_VERSION) => migrate_v3_durable_state(&mut migrated)?,
            Some(STATE_VERSION) => break,
            _ => return Err(invalid_state("unsupported GitHub durable state version")),
        }
    }
    validate_durable_state(&migrated)?;
    Ok(migrated)
}

fn validate_durable_state(state: &Value) -> Result<(), RequestError> {
    let object = state
        .as_object()
        .ok_or_else(|| invalid_state("GitHub durable state must be a JSON object"))?;
    if object.keys().any(|key| {
        ![
            "version",
            "pull_requests",
            "reconciliation",
            "task_transitions",
        ]
        .contains(&key.as_str())
    }) {
        return Err(invalid_state(
            "GitHub durable state contains unknown fields",
        ));
    }
    if object.get("version").and_then(Value::as_u64) != Some(STATE_VERSION) {
        return Err(invalid_state("unsupported GitHub durable state version"));
    }
    let pull_requests = object
        .get("pull_requests")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_state("GitHub durable state pull_requests must be an object"))?;
    for (session_id, links) in pull_requests {
        if session_id.trim().is_empty() {
            return Err(invalid_state(
                "GitHub durable state has an empty session id",
            ));
        }
        if !links.is_array() {
            return Err(invalid_state("a session's pull requests must be a list"));
        }
        parse_session_links(links).map_err(invalid_state)?;
    }
    let reconciliation = with_null_defaults(
        object.get("reconciliation").unwrap_or(&Value::Null),
        RECONCILIATION_OPTIONAL_FIELDS,
    );
    let reconciliation = reconciliation
        .as_object()
        .ok_or_else(|| invalid_state("GitHub durable state reconciliation must be an object"))?;
    require_only_keys(
        reconciliation,
        &[
            "status",
            "attempt_id",
            "started_at",
            "finished_at",
            "checked",
            "recovered_attempt_id",
            "recovered_at",
            "error",
        ],
    )?;
    if !matches!(
        reconciliation.get("status").and_then(Value::as_str),
        Some("idle" | "running" | "failed")
    ) || reconciliation
        .get("checked")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err(invalid_state(
            "GitHub durable state has invalid reconciliation status",
        ));
    }
    for field in ["attempt_id", "recovered_attempt_id", "error"] {
        strict_optional_string(reconciliation.get(field), field)?;
    }
    for field in ["started_at", "finished_at", "recovered_at"] {
        strict_optional_timestamp(reconciliation.get(field), field)?;
    }
    validate_task_transitions(
        object
            .get("task_transitions")
            .unwrap_or(&empty_task_transitions()),
    )
}

#[cfg(test)]
fn durable_state_from_settings(settings: &Value) -> Result<Value, RequestError> {
    let settings = settings
        .as_object()
        .ok_or_else(|| invalid_state("plugin settings must be a JSON object"))?;
    let state = settings
        .get(STATE_NAMESPACE)
        .cloned()
        .unwrap_or_else(empty_durable_state);
    migrate_durable_state(&state)
}

fn ensure_durable_state(
    context: &ExecutionContext,
    allow_cancelled: bool,
) -> Result<(), RequestError> {
    let response = host_call_with_cancellation(
        context,
        "github-settings-version",
        "host.settings.get",
        json!({ "path": [STATE_NAMESPACE, "version"] }),
        allow_cancelled,
    )?;
    let version = settings_value_from_host_response(&response)?;
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
        Some(version @ (LEGACY_STATE_VERSION | V2_STATE_VERSION | V3_STATE_VERSION)) => {
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
        _ => Err(invalid_state("unsupported GitHub durable state version")),
    }
}

fn read_session_links(
    context: &ExecutionContext,
    session_id: &str,
) -> Result<Vec<PullRequestLink>, RequestError> {
    let response = host_call(
        context,
        "github-settings-pr-links",
        "host.settings.get",
        json!({ "path": [STATE_NAMESPACE, "pull_requests", session_id] }),
    )?;
    parse_session_links(&settings_value_from_host_response(&response)?).map_err(invalid_state)
}

fn read_group_links(
    context: &ExecutionContext,
    group: &[String],
) -> Result<SessionLinks, RequestError> {
    group
        .iter()
        .map(|session_id| Ok((session_id.clone(), read_session_links(context, session_id)?)))
        .collect()
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

/// A session's task group; a host that cannot list sessions leaves the session on its own.
fn session_task_group(
    context: &ExecutionContext,
    session_id: &str,
) -> Result<Vec<String>, RequestError> {
    match list_sessions(context) {
        Ok(sessions) => Ok(task_group(&sessions, session_id)),
        Err(RequestError::Cancelled) => Err(RequestError::Cancelled),
        Err(RequestError::Message(message)) => {
            eprintln!("{PLUGIN_ID} could not list sessions for {session_id}: {message}");
            Ok(vec![session_id.to_string()])
        }
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
    group: &[String],
    mutate: impl FnOnce(&mut SessionLinks) -> Result<(), RequestError>,
) -> Result<GroupUpdate, RequestError> {
    let (update, transition) = {
        let _persistence = context
            .durable_state_locks
            .pull_requests
            .lock()
            .map_err(|_| invalid_state("pull request persistence lock poisoned"))?;
        ensure_durable_state(context, false)?;
        let before = read_group_links(context, group)?;
        let mut after = before.clone();
        mutate(&mut after)?;
        let mut patch = serde_json::Map::new();
        for (session_id, links) in &after {
            let previous = before
                .get(session_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if refresh::differs(previous, links) {
                patch.insert(session_id.clone(), session_links_value(links));
            }
        }
        let changed = !patch.is_empty();
        if changed {
            patch_durable_state(
                context,
                false,
                json!({ "pull_requests": Value::Object(patch) }),
            )?;
        }
        let prs = group_prs(&after, group);
        let transition = task_transition(task_phase(&group_prs(&before, group)), task_phase(&prs));
        (
            GroupUpdate {
                lists: after,
                prs,
                changed,
            },
            transition,
        )
    };
    if let Some(session_id) = group.first() {
        apply_task_transition(context, session_id, transition);
    }
    Ok(update)
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

/// The session's own pull request that the panel details and merges: its branch's, else one it created or linked.
fn detail_link<'a>(links: &'a [PullRequestLink], repository: &str) -> Option<&'a PullRequestLink> {
    [
        TrackedSource::Branch,
        TrackedSource::Created,
        TrackedSource::Manual,
    ]
    .into_iter()
    .find_map(|source| {
        links
            .iter()
            .filter(|link| link.source() == Some(source) && link.key.repository == repository)
            .max_by_key(|link| {
                link.snapshot()
                    .and_then(|snapshot| snapshot.updated_at.clone())
            })
    })
}

fn project_detail(pr: &FetchedPr, merge_methods: Vec<String>) -> Value {
    let detail = pr.detail.as_ref();
    json!({
        "url": pr.url,
        "number": pr.key.number,
        "repository": pr.key.repository,
        "title": pr.snapshot.title,
        "state": pr.snapshot.state,
        "checks": detail.map_or_else(Vec::new, |detail| detail.checks.clone()),
        "conflicting": detail.is_some_and(|detail| detail.conflicting),
        "merge_blocked": detail.is_some_and(|detail| detail.merge_blocked),
        "merge_methods": merge_methods,
        "review_decision": detail.map_or(Value::Null, |detail| detail.review_decision.clone()),
    })
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
    let group = session_task_group(context, session_id)?;
    let lists = read_group_links(context, &group)?;
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
    let update = update_group_links(context, &group, |fresh| {
        refresh::apply(fresh, &groups, &outcome);
        Ok(())
    })?;
    let own = update
        .lists
        .get(session_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let own_url = detail_link(own, &session.repository).map(|link| link.url.clone());
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
fn summary(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let group = session_task_group(context, session_id)?;
    let lists = read_group_links(context, &group)?;
    Ok(json!({ "pull_requests": prs_json(&group_prs(&lists, &group)) }))
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
    let repository = repository_context(params, context)?;
    let pr = fetch_pull_request(context, &context_path(&repository)?, &key)?;
    let group = session_task_group(context, session_id)?;
    let update = update_group_links(context, &group, |lists| {
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
    })?;
    Ok(json!({ "pull_requests": prs_json(&update.prs) }))
}

/// Unlinks a pull request, or dismisses a stack pull request, from every session of the task.
fn unlink_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let key = pull_request_key(params)?;
    let group = session_task_group(context, session_id)?;
    let update = update_group_links(context, &group, |lists| {
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
        refreshed_at: unix_timestamp_ms(),
    };
    let group = session_task_group(context, session_id)?;
    update_group_links(context, &group, |lists| {
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

/// The pull request an action targets: the URL the panel sent, else the session branch's.
fn target_reference(params: &Value, branch: &str) -> String {
    params
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| PrKey::from_url(url).is_some())
        .unwrap_or(branch)
        .to_string()
}

fn merge_pull_request(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
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
    let reference = target_reference(params, &branch);
    let url = match PrKey::from_url(&reference) {
        Some(_) => reference.clone(),
        None => {
            let raw = gh_output(context, &cwd, ["pr", "view", &reference, "--json", "url"])?;
            let view: Value = serde_json::from_str(&raw).map_err(|error| {
                invalid_state(format!(
                    "failed to parse pull request before merge: {error}"
                ))
            })?;
            view.get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_state("gh pr view did not return a pull request url"))?
                .to_string()
        }
    };
    let key = PrKey::from_url(&url)
        .ok_or_else(|| invalid_state(format!("unsupported pull request URL {url}")))?;
    if !repository_merge_methods(context, &cwd, &key.repository)?
        .iter()
        .any(|method| method == strategy)
    {
        return Err(invalid_state(format!(
            "{strategy} merge is disabled for this GitHub repository"
        )));
    }
    gh_output(
        context,
        &cwd,
        ["pr", "merge", &url, strategy_flag, "--delete-branch"],
    )?;
    let group = session_task_group(context, session_id)?;
    update_group_links(context, &group, |lists| {
        for links in lists.values_mut() {
            let merged = links
                .iter()
                .find(|link| link.key == key)
                .and_then(PullRequestLink::snapshot)
                .map(|snapshot| PrSnapshot {
                    state: PrState::Merged,
                    refreshed_at: unix_timestamp_ms(),
                    ..snapshot.clone()
                });
            if let Some(merged) = merged {
                refresh_snapshot(links, &key, &merged);
            }
        }
        Ok(())
    })?;
    Ok(json!({ "merged": true }))
}

fn mark_ready(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    gh_output(
        context,
        &cwd,
        ["pr", "ready", &target_reference(params, &branch)],
    )?;
    Ok(json!({ "ready": true }))
}

fn failure_log_message(params: &Value, context: &ExecutionContext) -> Result<String, RequestError> {
    let session_id = required_string(params, "session_id")?;
    let repository = repository_context(params, context)?;
    let cwd = context_path(&repository)?;
    let branch = context_string(&repository, "branch")?;
    let reference = match params.get("url").and_then(Value::as_str) {
        Some(_) => target_reference(params, &branch),
        None => {
            let links = read_session_links(context, session_id)?;
            let repository = git_output(context, &cwd, ["remote", "get-url", "origin"])
                .ok()
                .and_then(|remote| github_repository_slug(&remote))
                .unwrap_or_default()
                .to_ascii_lowercase();
            detail_link(&links, &repository)
                .map(|link| link.url.clone())
                .unwrap_or_else(|| branch.clone())
        }
    };
    let raw = gh_output(
        context,
        &cwd,
        ["pr", "view", &reference, "--json", "statusCheckRollup"],
    )?;
    let status: Value = serde_json::from_str(&raw)
        .map_err(|error| invalid_state(format!("failed to parse CI status: {error}")))?;
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
        .ok_or_else(|| invalid_state("no failed GitHub Actions run found"))?;
    // A linked pull request can live in another repository than the session's checkout.
    let target = PrKey::from_url(&reference);
    let logs = match &target {
        Some(key) => gh_output(
            context,
            &cwd,
            ["run", "view", run_id, "--log-failed", "-R", &key.repository],
        )?,
        None => gh_output(context, &cwd, ["run", "view", run_id, "--log-failed"])?,
    };
    let tail = logs
        .lines()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let subject = match &target {
        Some(key) => format!("{}#{}", key.repository, key.number),
        None => format!("branch `{branch}`"),
    };
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

/// Refreshes every task group with an active session using as few GitHub queries as possible.
fn reconcile_groups(context: &ExecutionContext, checked: &mut u64) -> Result<(), RequestError> {
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
    let members: Vec<String> = groups.iter().flatten().cloned().collect();
    let lists = read_group_links(context, &members)?;
    let has_links = lists.values().any(|links| !links.is_empty());
    if targets.is_empty() && !has_links {
        return Ok(());
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
    for group in &groups {
        context.check_cancelled()?;
        update_group_links(context, group, |fresh| {
            refresh::apply(fresh, &groups, &outcome);
            Ok(())
        })?;
    }
    Ok(())
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
        Ok(()) if !context.cancelled.load(Ordering::SeqCst) => {
            match finish_reconciliation(
                context,
                false,
                &attempt_id,
                "idle",
                checked,
                finished_at,
                None,
            ) {
                Ok(()) => return Ok(json!({ "checked": checked, "attempt_id": attempt_id })),
                Err(error) => error,
            }
        }
        Ok(()) => RequestError::Cancelled,
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
        let callback = receive_frame(&output_rx);
        let callback_id = callback["id"].as_str().unwrap();
        assert_eq!(callback["method"], "host.settings.get");
        assert_eq!(
            callback["params"],
            json!({ "path": ["github", "pull_requests", "s1"] })
        );
        input_tx
            .send(
                json!({ "jsonrpc": "2.0", "id": callback_id, "result": { "settings": null } })
                    .to_string(),
            )
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

    #[test]
    fn durable_state_accepts_missing_settings_and_rejects_invalid_or_incompatible_documents() {
        assert_eq!(
            durable_state_from_settings(&json!({})).unwrap(),
            empty_durable_state()
        );
        let explicit_v1 = json!({ "github": empty_durable_state() });
        assert_eq!(
            durable_state_from_settings(&explicit_v1).unwrap(),
            empty_durable_state()
        );
        for settings in [
            json!({ "github": Value::Null }),
            json!({ "github": [] }),
            json!({ "github": { "version": 2, "pull_requests": {}, "reconciliation": {} } }),
            json!({ "github": { "version": 1, "pull_requests": { "s1": { "session_id": "other" } }, "reconciliation": {} } }),
            json!({ "github": { "version": 4, "pull_requests": { "s1": { "session_id": "s1" } }, "reconciliation": empty_durable_state()["reconciliation"], "task_transitions": empty_task_transitions() } }),
        ] {
            assert!(
                durable_state_from_settings(&settings).is_err(),
                "{settings}"
            );
        }
    }

    fn merge_json_object(target: &mut Value, patch: &Value) {
        let target = target.as_object_mut().unwrap();
        for (key, value) in patch.as_object().unwrap() {
            target.insert(key.clone(), value.clone());
        }
    }

    #[test]
    fn reconciliation_patches_recover_interruption_fence_attempts_and_bound_errors() {
        let state = empty_durable_state();
        let old = begin_reconciliation_patch(&state["reconciliation"], "old-attempt", 100).unwrap();
        let mut running = state["reconciliation"].clone();
        merge_json_object(&mut running, &old);
        let new = begin_reconciliation_patch(&running, "new-attempt", 200).unwrap();
        assert_eq!(new["recovered_attempt_id"], "old-attempt");
        assert_eq!(new["recovered_at"], 200);
        merge_json_object(&mut running, &new);
        assert!(
            finish_reconciliation_patch(&running, "old-attempt", "idle", 1, 300, None).is_err()
        );

        let idle =
            finish_reconciliation_patch(&running, "new-attempt", "idle", 2, 300, None).unwrap();
        assert_eq!(idle["status"], "idle");
        assert_eq!(idle["checked"], 2);

        let long_error = "x".repeat(MAX_RECONCILIATION_ERROR_CHARS + 10);
        let failed = finish_reconciliation_patch(
            &running,
            "new-attempt",
            "failed",
            3,
            500,
            Some(&long_error),
        )
        .unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(
            failed["error"].as_str().unwrap().chars().count(),
            MAX_RECONCILIATION_ERROR_CHARS
        );
    }

    #[cfg(unix)]
    #[test]
    fn sends_failure_logs_to_the_currently_focused_agent_through_host_callback() {
        let directory = TestDirectory::new();
        let gh_args = directory.0.join("gh-args");
        let gh = directory.script(
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$@" >> '{}'
if [ "$1" = "pr" ]; then
  printf '%s\n' '{{"statusCheckRollup":[{{"conclusion":"FAILURE","detailsUrl":"https://github.com/o/r/actions/runs/123/job/45"}}]}}'
else
  printf 'checkRun cargo fmt --check\nDiff in src-tauri/src/plugins.rs\n'
fi
"#,
                gh_args.display()
            ),
        );
        let (events, events_rx) = mpsc::channel();
        let context = ExecutionContext {
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            runner: CommandRunner::new(&gh, &gh),
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
        } = events_rx.recv_timeout(Duration::from_secs(1)).unwrap()
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
        } = events_rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected session-prompt callback");
        };
        assert_eq!(method, "host.sessions.prompt");
        assert_eq!(
            fs::read_to_string(&gh_args).unwrap(),
            "pr\nview\nhttps://github.com/o/r/pull/456\n--json\nstatusCheckRollup\nrun\nview\n123\n--log-failed\n-R\no/r\n",
            "the run is read from the pull request's repository, which can differ from the checkout"
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
    fn projects_only_github_enabled_merge_methods() {
        let methods = enabled_repository_merge_methods(&json!({
            "allow_squash_merge": true,
            "allow_merge_commit": false,
            "allow_rebase_merge": true,
        }));
        assert_eq!(methods, vec!["squash", "rebase"]);
        assert_eq!(
            github_repository_slug("git@github.com:o/r.git"),
            Some("o/r".to_string())
        );
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
                let result = match method.as_str() {
                    "host.settings.get" => {
                        let value = params["path"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .try_fold(&host.settings, |value, key| {
                                value.get(key.as_str().unwrap())
                            })
                            .cloned()
                            .unwrap_or(Value::Null);
                        json!({ "settings": value })
                    }
                    "host.settings.patch" => {
                        merge_settings_patch(&mut host.settings, &params["patch"]);
                        json!({ "updated": true })
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

    /// Fake `git` and `gh`: the remote follows the working tree, and `gh` serves `gh-response` once, then empty data.
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
            let git = directory.script(
                "git",
                "#!/bin/sh\ncase \"$(pwd)\" in *other-repo) echo https://github.com/x/y.git ;; *) echo https://github.com/o/r.git ;; esac\n",
            );
            let root = directory.path().to_string();
            let gh = directory.script(
                "gh",
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{root}/gh-args'\nprintf x >> '{root}/gh-calls'\nif [ -f '{root}/gh-response' ]; then cat '{root}/gh-response'; rm '{root}/gh-response'; else printf '{{\"data\":{{}}}}'; fi\n"
                ),
            );
            Self {
                runner: CommandRunner::new(git, gh),
                directory,
            }
        }

        fn respond(&self, body: Value) {
            fs::write(self.directory.0.join("gh-response"), body.to_string()).unwrap();
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
        settings["github"]["version"] = json!(V3_STATE_VERSION);
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

        let args = fs::read_to_string(cli.directory.0.join("gh-args")).unwrap();
        assert!(args.starts_with(
            "api\ngraphql\n-f\nquery=query { r0: repository(owner: \"o\", name: \"r\")"
        ));
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
        assert_eq!(stored["s1"][1]["source"], "stack-dismissed");
        assert_eq!(
            stored["s2"][0],
            json!({ "host": "github.com", "repository": "o/r", "number": 9, "url": "https://github.com/o/r/pull/9", "source": "stack-dismissed" })
        );
        assert_eq!(cli.calls(), 1, "unlinking never asks GitHub");
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
        with_host(&cli.runner, &host, reconcile).unwrap();
        assert_eq!(host.lock().unwrap().transitions.len(), 1);
    }

    #[test]
    fn migration_accepts_state_whose_null_fields_the_host_deleted() {
        let v3 = json!({
            "version": V3_STATE_VERSION,
            "reconciliation": { "checked": 0, "status": "idle" },
            "task_transitions": {},
            "pull_requests": { "s1": {
                "branch": "topic", "check_summary": { "state": "passing", "updated_at": 2 }, "remote": "git@github.com:o/r.git",
                "session_id": "s1", "state": "merged", "updated_at": 3, "url": "https://github.com/o/r/pull/317",
            } },
        });
        let migrated = migrate_durable_state(&v3).unwrap();
        assert_eq!(migrated["pull_requests"]["s1"][0]["checks"], "passing");
        assert_eq!(migrated["pull_requests"]["s1"][0]["state"], "merged");
        assert!(
            reconciliation_from_settings_value(json!({ "checked": 0, "status": "idle" })).unwrap()
                ["error"]
                .is_null()
        );
    }

    #[test]
    fn migration_upgrades_every_version_to_lists_and_is_idempotent() {
        let legacy = json!({ "session_id": "s1", "url": "https://github.com/o/r/pull/1", "state": "open", "remote": "https://github.com/o/r.git", "branch": "topic", "updated_at": 7 });
        let v1 = json!({ "version": LEGACY_STATE_VERSION, "pull_requests": { "s1": legacy }, "reconciliation": empty_durable_state()["reconciliation"] });
        let migrated = migrate_durable_state(&v1).unwrap();
        assert_eq!(migrated["version"], STATE_VERSION);
        assert_eq!(migrated["task_transitions"], empty_task_transitions());
        assert_eq!(migrated["pull_requests"]["s1"][0]["number"], 1);
        assert_eq!(migrated["pull_requests"]["s1"][0]["source"], "manual");
        assert_eq!(migrate_durable_state(&migrated).unwrap(), migrated);

        let mut v2 = v1.clone();
        v2["version"] = json!(V2_STATE_VERSION);
        v2["task_transitions"] = json!({ "on_open": "in_review" });
        v2["reconciliation"]["error"] = json!("keep");
        let migrated = migrate_durable_state(&v2).unwrap();
        assert_eq!(
            migrated["task_transitions"],
            json!({ "on_open": "in_review", "on_merge": null })
        );
        assert_eq!(migrated["reconciliation"]["error"], "keep");
        assert_eq!(migrated["pull_requests"]["s1"].as_array().unwrap().len(), 1);
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
