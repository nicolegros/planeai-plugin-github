use serde_json::{json, Value};
use std::collections::HashMap;
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
const HOST_API_VERSION: &str = "planeai.plugin-host.v1";
const CANCELLATION_ERROR_CODE: i64 = -32800;
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const STATE_NAMESPACE: &str = "github";
const STATE_VERSION: u64 = 1;
const MAX_RECONCILIATION_ERROR_CHARS: usize = 500;

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

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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

fn strict_string(value: Option<&Value>, field: &str) -> Result<String, RequestError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| RequestError::Message(format!("GitHub durable state has invalid {field}")))
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

fn validate_durable_state(state: &Value) -> Result<(), RequestError> {
    let object = state.as_object().ok_or_else(|| {
        RequestError::Message("GitHub durable state must be a JSON object".to_string())
    })?;
    require_only_keys(object, &["version", "pull_requests", "reconciliation"])?;
    if object.get("version").and_then(Value::as_u64) != Some(STATE_VERSION) {
        return Err(RequestError::Message(
            "unsupported GitHub durable state version".to_string(),
        ));
    }
    let pull_requests = object
        .get("pull_requests")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RequestError::Message(
                "GitHub durable state pull_requests must be an object".to_string(),
            )
        })?;
    for (session_id, pull_request) in pull_requests {
        if session_id.trim().is_empty() {
            return Err(RequestError::Message(
                "GitHub durable state has an empty session id".to_string(),
            ));
        }
        let pull_request = pull_request.as_object().ok_or_else(|| {
            RequestError::Message("GitHub durable state pull request must be an object".to_string())
        })?;
        if pull_request.keys().any(|key| {
            ![
                "session_id",
                "url",
                "state",
                "remote",
                "branch",
                "updated_at",
            ]
            .contains(&key.as_str())
        }) {
            return Err(RequestError::Message(
                "GitHub durable state pull request contains unknown fields".to_string(),
            ));
        }
        if strict_string(pull_request.get("session_id"), "pull request session_id")? != *session_id
            || strict_string(pull_request.get("url"), "pull request url").is_err()
            || strict_string(pull_request.get("state"), "pull request state").is_err()
            || pull_request
                .get("updated_at")
                .and_then(Value::as_u64)
                .is_none()
        {
            return Err(RequestError::Message(
                "GitHub durable state has an invalid pull request mapping".to_string(),
            ));
        }
        for field in ["remote", "branch"] {
            if let Some(value) = pull_request.get(field) {
                if value.as_str().is_none_or(|value| value.trim().is_empty()) {
                    return Err(RequestError::Message(format!(
                        "GitHub durable state has invalid pull request {field}"
                    )));
                }
            }
        }
    }
    let reconciliation = object
        .get("reconciliation")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RequestError::Message(
                "GitHub durable state reconciliation must be an object".to_string(),
            )
        })?;
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
        return Err(RequestError::Message(
            "GitHub durable state has invalid reconciliation status".to_string(),
        ));
    }
    for field in ["attempt_id", "recovered_attempt_id", "error"] {
        strict_optional_string(reconciliation.get(field), field)?;
    }
    for field in ["started_at", "finished_at", "recovered_at"] {
        strict_optional_timestamp(reconciliation.get(field), field)?;
    }
    Ok(())
}

#[cfg(test)]
fn durable_state_from_settings(settings: &Value) -> Result<Value, RequestError> {
    let settings = settings.as_object().ok_or_else(|| {
        RequestError::Message("plugin settings must be a JSON object".to_string())
    })?;
    let state = settings
        .get(STATE_NAMESPACE)
        .cloned()
        .unwrap_or_else(empty_durable_state);
    validate_durable_state(&state)?;
    Ok(state)
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
    match settings_value_from_host_response(&response)? {
        Value::Null => {
            host_call_with_cancellation(
                context,
                "github-settings-initialize",
                "host.settings.patch",
                durable_state_patch_request(empty_durable_state()),
                allow_cancelled,
            )?;
            Ok(())
        }
        Value::Number(version) if version.as_u64() == Some(STATE_VERSION) => Ok(()),
        _ => Err(RequestError::Message(
            "unsupported GitHub durable state version".to_string(),
        )),
    }
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
    Ok(state["reconciliation"].clone())
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

fn pr_mapping(
    session_id: &str,
    url: &str,
    pr_state: &str,
    remote: Option<&str>,
    branch: Option<&str>,
    updated_at: u64,
) -> Value {
    let mut mapping = serde_json::Map::new();
    mapping.insert("session_id".to_string(), json!(session_id));
    mapping.insert("url".to_string(), json!(url));
    mapping.insert("state".to_string(), json!(pr_state));
    if let Some(remote) = remote.filter(|value| !value.trim().is_empty()) {
        mapping.insert("remote".to_string(), json!(remote));
    }
    if let Some(branch) = branch.filter(|value| !value.trim().is_empty()) {
        mapping.insert("branch".to_string(), json!(branch));
    }
    mapping.insert("updated_at".to_string(), json!(updated_at));
    Value::Object(mapping)
}

fn persist_pr_mapping(
    context: &ExecutionContext,
    session_id: &str,
    url: &str,
    pr_state: &str,
    remote: Option<&str>,
    branch: Option<&str>,
) -> Result<(), RequestError> {
    let mut pull_requests = serde_json::Map::new();
    pull_requests.insert(
        session_id.to_string(),
        pr_mapping(
            session_id,
            url,
            pr_state,
            remote,
            branch,
            unix_timestamp_ms(),
        ),
    );
    patch_durable_state(
        context,
        false,
        json!({ "pull_requests": Value::Object(pull_requests) }),
    )
}

fn clear_pr_mapping(context: &ExecutionContext, session_id: &str) -> Result<(), RequestError> {
    let mut pull_requests = serde_json::Map::new();
    pull_requests.insert(session_id.to_string(), Value::Null);
    patch_durable_state(
        context,
        false,
        json!({ "pull_requests": Value::Object(pull_requests) }),
    )
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

fn status(params: &Value, context: &ExecutionContext) -> Result<Value, RequestError> {
    let session_id = required_string(params, "session_id")?;
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
            let projected = project_pr_status(&pr);
            let url = strict_string(projected.get("url"), "pull request url")?;
            let pr_state = strict_string(projected.get("state"), "pull request state")?;
            persist_pr_mapping(
                context,
                session_id,
                &url,
                &pr_state,
                Some(remote.trim()),
                Some(&branch),
            )?;
            Ok(json!({ "applicable": true, "remote": remote.trim(), "pr": projected }))
        }
        Err(RequestError::Message(error)) if is_no_pull_request(&error) => {
            clear_pr_mapping(context, session_id)?;
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
    let url = url.trim();
    let remote = git_output(context, &cwd, ["remote", "get-url", "origin"])?;
    persist_pr_mapping(
        context,
        required_string(params, "session_id")?,
        url,
        if draft { "draft" } else { "open" },
        Some(remote.trim()),
        Some(&branch),
    )?;
    Ok(json!({ "url": url }))
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
    let projected = project_pr_status(&pr);
    let pr_url = strict_string(projected.get("url"), "pull request url")?;
    let pr_state = strict_string(projected.get("state"), "pull request state")?;
    let branch = context_string(&repository, "branch").ok();
    let remote = git_output(context, &cwd, ["remote", "get-url", "origin"]).ok();
    persist_pr_mapping(
        context,
        required_string(params, "session_id")?,
        &pr_url,
        &pr_state,
        remote.as_deref().map(str::trim),
        branch.as_deref(),
    )?;
    Ok(json!({ "pr": projected }))
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
    let before_merge = gh_output(context, &cwd, ["pr", "view", &branch, "--json", "url"])?;
    let before_merge: Value = serde_json::from_str(&before_merge).map_err(|error| {
        RequestError::Message(format!(
            "failed to parse pull request before merge: {error}"
        ))
    })?;
    let url = strict_string(before_merge.get("url"), "pull request url")?;
    let remote = git_output(context, &cwd, ["remote", "get-url", "origin"])?;
    gh_output(
        context,
        &cwd,
        ["pr", "merge", &branch, strategy, "--delete-branch"],
    )?;
    persist_pr_mapping(
        context,
        required_string(params, "session_id")?,
        &url,
        "merged",
        Some(remote.trim()),
        Some(&branch),
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
    let _reconciliation = context
        .durable_state_locks
        .reconciliation
        .try_lock()
        .map_err(|_| RequestError::Message("reconciliation is already running".to_string()))?;
    context.check_cancelled()?;
    let started_at = unix_timestamp_ms();
    let attempt_id = format!(
        "reconcile-{started_at}-{}",
        context.callback_sequence.fetch_add(1, Ordering::Relaxed)
    );
    begin_reconciliation(context, &attempt_id, started_at)?;

    let mut checked = 0_u64;
    let work = (|| {
        let sessions = host_call(
            context,
            "github-sessions",
            "host.sessions.list",
            Value::Null,
        )?;
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
            checked += 1;
            status(&json!({ "session_id": session_id }), context)?;
        }
        Ok(())
    })();

    let finished_at = unix_timestamp_ms();
    match work {
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
                Ok(_) => Ok(json!({ "checked": checked, "attempt_id": attempt_id })),
                Err(error) => {
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
            }
        }
        Ok(()) => {
            let error = RequestError::Cancelled;
            finish_reconciliation(
                context,
                true,
                &attempt_id,
                "failed",
                checked,
                finished_at,
                Some("request cancelled"),
            )?;
            Err(error)
        }
        Err(error) => {
            let message = match &error {
                RequestError::Cancelled => "request cancelled".to_string(),
                RequestError::Message(message) => message.clone(),
            };
            let terminal = finish_reconciliation(
                context,
                true,
                &attempt_id,
                "failed",
                checked,
                finished_at,
                Some(&message),
            );
            terminal?;
            Err(error)
        }
    }
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

    #[test]
    fn persisting_a_pr_mapping_uses_bounded_settings_callbacks() {
        let (events, events_rx) = mpsc::channel();
        let context = ExecutionContext {
            cancelled: Arc::new(AtomicBool::new(false)),
            events,
            runner: CommandRunner::default(),
            request_key: "test-request".to_string(),
            callback_sequence: Arc::new(AtomicU64::new(1)),
            durable_state_locks: DurableStateLocks {
                reconciliation: Arc::new(Mutex::new(())),
            },
        };
        let worker = thread::spawn(move || {
            persist_pr_mapping(
                &context,
                "session-1",
                "https://github.com/example/repository/pull/1",
                "open",
                Some("https://github.com/example/repository.git"),
                Some("topic"),
            )
        });

        let ControllerEvent::HostCall {
            method,
            params,
            response,
            ..
        } = events_rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected a host settings version callback");
        };
        assert_eq!(method, "host.settings.get");
        assert_eq!(params, json!({ "path": ["github", "version"] }));
        response
            .send(Ok(json!({ "settings": STATE_VERSION })))
            .unwrap();

        let ControllerEvent::HostCall {
            method,
            params,
            response,
            ..
        } = events_rx.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected a host settings patch callback");
        };
        assert_eq!(method, "host.settings.patch");
        assert_eq!(
            params["patch"]["github"]["pull_requests"]["session-1"]["url"],
            "https://github.com/example/repository/pull/1"
        );
        assert!(params.to_string().len() < 2048);
        response.send(Ok(json!({ "updated": true }))).unwrap();
        worker.join().unwrap().unwrap();
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
        ] {
            assert!(
                durable_state_from_settings(&settings).is_err(),
                "{settings}"
            );
        }
    }

    #[test]
    fn durable_state_patches_are_small_and_leave_unrelated_settings_host_owned() {
        let mapping = pr_mapping(
            "s1",
            "https://github.com/o/r/pull/1",
            "open",
            Some("https://github.com/o/r.git"),
            Some("topic"),
            20,
        );
        let request = durable_state_patch_request(json!({
            "pull_requests": { "s1": mapping }
        }));
        assert_eq!(
            request["patch"]["github"]["pull_requests"]["s1"]["branch"],
            "topic"
        );
        assert!(request.to_string().len() < 1024);
    }

    #[test]
    fn pull_request_mapping_patch_removes_only_the_selected_mapping() {
        let mut removed = serde_json::Map::new();
        removed.insert("s1".to_string(), Value::Null);
        let request = durable_state_patch_request(json!({
            "pull_requests": Value::Object(removed)
        }));
        assert!(request["patch"]["github"]["pull_requests"]["s1"].is_null());
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

    #[test]
    fn handshake_identity_matches_manifest() {
        let manifest: Value = serde_json::from_str(include_str!("../planeai-plugin.json")).unwrap();
        let response = handshake(&json!({ "host_api_version": HOST_API_VERSION })).unwrap();

        assert_eq!(response["plugin_id"], manifest["id"]);
        assert_eq!(response["plugin_name"], manifest["name"]);
        assert_eq!(response["plugin_version"], manifest["version"]);
        assert_eq!(response["host_api_version"], manifest["host_api_version"]);
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
