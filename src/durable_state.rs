//! The versioned `github` settings namespace: its shape, migrations, validation, and reconciliation patches.

use crate::pull_requests::{parse_legacy_mapping, parse_session_links, session_links_value};
use serde_json::{json, Map, Value};

pub const STATE_NAMESPACE: &str = "github";
pub const STATE_VERSION: u64 = 4;
const LEGACY_STATE_VERSION: u64 = 1;
const V2_STATE_VERSION: u64 = 2;
const V3_STATE_VERSION: u64 = 3;
const MAX_RECONCILIATION_ERROR_CHARS: usize = 500;
const TASK_STATUSES: &[&str] = &["todo", "in_progress", "in_review", "done"];
const TASK_TRANSITION_FIELDS: &[&str] = &["on_open", "on_merge"];
const RECONCILIATION_OPTIONAL_FIELDS: &[&str] = &[
    "attempt_id",
    "started_at",
    "finished_at",
    "recovered_attempt_id",
    "recovered_at",
    "error",
];
const STATE_FIELDS: &[&str] = &[
    "version",
    "pull_requests",
    "reconciliation",
    "task_transitions",
];

fn empty_task_transitions() -> Value {
    json!({ "on_open": Value::Null, "on_merge": Value::Null })
}

pub fn empty_durable_state() -> Value {
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

fn require_only_keys(object: &Map<String, Value>, keys: &[&str]) -> Result<(), String> {
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err("GitHub durable state contains unknown fields".to_string());
    }
    if keys.iter().any(|key| !object.contains_key(*key)) {
        return Err("GitHub durable state is missing required fields".to_string());
    }
    Ok(())
}

fn strict_optional_string(value: Option<&Value>, field: &str) -> Result<(), String> {
    match value {
        Some(Value::Null) => Ok(()),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(()),
        _ => Err(format!("GitHub durable state has invalid {field}")),
    }
}

fn strict_optional_timestamp(value: Option<&Value>, field: &str) -> Result<(), String> {
    match value {
        Some(Value::Null) => Ok(()),
        Some(Value::Number(number)) if number.as_u64().is_some() => Ok(()),
        _ => Err(format!("GitHub durable state has invalid {field}")),
    }
}

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

/// Checks the task transitions and returns them with absent fields as null.
pub fn validate_task_transitions(value: &Value) -> Result<Value, String> {
    let value = with_null_defaults(value, TASK_TRANSITION_FIELDS);
    let transitions = value
        .as_object()
        .ok_or("GitHub durable state task_transitions must be an object")?;
    require_only_keys(transitions, TASK_TRANSITION_FIELDS)?;
    for field in TASK_TRANSITION_FIELDS {
        match transitions.get(*field) {
            Some(Value::Null) => {}
            Some(Value::String(status)) if TASK_STATUSES.contains(&status.as_str()) => {}
            _ => {
                return Err(format!(
                    "GitHub durable state has invalid {field} transition"
                ))
            }
        }
    }
    Ok(value)
}

fn add_empty_check_summaries(state: &mut Value) -> Result<(), String> {
    let mappings = state
        .get_mut("pull_requests")
        .and_then(Value::as_object_mut)
        .ok_or("GitHub durable state pull_requests must be an object")?;
    for mapping in mappings.values_mut() {
        mapping
            .as_object_mut()
            .ok_or("GitHub durable state pull request must be an object")?
            .entry("check_summary".to_string())
            .or_insert(Value::Null);
    }
    Ok(())
}

fn normalize_task_transitions(state: &mut Value) -> Result<(), String> {
    let transitions = state_object(state)?
        .entry("task_transitions".to_string())
        .or_insert_with(empty_task_transitions)
        .as_object_mut()
        .ok_or("GitHub durable state task_transitions must be an object")?;
    if transitions
        .keys()
        .any(|key| !TASK_TRANSITION_FIELDS.contains(&key.as_str()))
    {
        return Err("GitHub durable state contains unknown fields".to_string());
    }
    for key in TASK_TRANSITION_FIELDS {
        transitions.entry(key.to_string()).or_insert(Value::Null);
    }
    Ok(())
}

pub fn reconciliation_from_settings_value(value: Value) -> Result<Value, String> {
    let mut state = empty_durable_state();
    state["reconciliation"] = value;
    validate_durable_state(&state)?;
    Ok(with_null_defaults(
        &state["reconciliation"],
        RECONCILIATION_OPTIONAL_FIELDS,
    ))
}

pub fn begin_reconciliation_patch(
    reconciliation: &Value,
    attempt_id: &str,
    started_at: u64,
) -> Result<Value, String> {
    let reconciliation = reconciliation
        .as_object()
        .ok_or("GitHub durable state reconciliation must be an object")?;
    let mut patch = Map::new();
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

pub fn finish_reconciliation_patch(
    reconciliation: &Value,
    attempt_id: &str,
    status: &str,
    checked: u64,
    finished_at: u64,
    error: Option<&str>,
) -> Result<Value, String> {
    let reconciliation = reconciliation
        .as_object()
        .ok_or("GitHub durable state reconciliation must be an object")?;
    if reconciliation.get("status").and_then(Value::as_str) != Some("running")
        || reconciliation.get("attempt_id").and_then(Value::as_str) != Some(attempt_id)
    {
        return Err("reconciliation attempt was superseded".to_string());
    }
    Ok(json!({
        "status": status,
        "finished_at": finished_at,
        "checked": checked,
        "error": error.map_or(Value::Null, |error| json!(bounded_error(error))),
    }))
}

fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_RECONCILIATION_ERROR_CHARS).collect()
}

fn state_object(state: &mut Value) -> Result<&mut Map<String, Value>, String> {
    state
        .as_object_mut()
        .ok_or_else(|| "GitHub durable state must be a JSON object".to_string())
}

fn migrate_v1_durable_state(state: &mut Value) -> Result<(), String> {
    let object = state_object(state)?;
    require_only_keys(object, &["version", "pull_requests", "reconciliation"])?;
    object.insert("version".to_string(), json!(V2_STATE_VERSION));
    object.insert("task_transitions".to_string(), empty_task_transitions());
    Ok(())
}

fn migrate_v2_durable_state(state: &mut Value) -> Result<(), String> {
    let object = state_object(state)?;
    if object
        .keys()
        .any(|key| !STATE_FIELDS.contains(&key.as_str()))
        || !object.contains_key("pull_requests")
        || !object.contains_key("reconciliation")
    {
        return Err("unsupported GitHub durable state version".to_string());
    }
    object.insert("version".to_string(), json!(V3_STATE_VERSION));
    normalize_task_transitions(state)?;
    add_empty_check_summaries(state)
}

/// Version 4 tracks a list of pull requests per session instead of one mapping.
fn migrate_v3_durable_state(state: &mut Value) -> Result<(), String> {
    let object = state_object(state)?;
    let pull_requests = object
        .get_mut("pull_requests")
        .and_then(Value::as_object_mut)
        .ok_or("GitHub durable state pull_requests must be an object")?;
    for (session_id, links) in pull_requests.iter_mut() {
        let parsed = if links.is_array() {
            parse_session_links(links)?
        } else if let Some(link) = parse_legacy_mapping(links)? {
            vec![link]
        } else {
            eprintln!("github dropped the unsupported legacy pull request of session {session_id}: {links}");
            Vec::new()
        };
        *links = session_links_value(&parsed);
    }
    object.insert("version".to_string(), json!(STATE_VERSION));
    Ok(())
}

pub fn migrate_durable_state(state: &Value) -> Result<Value, String> {
    let mut migrated = state.clone();
    loop {
        match migrated.get("version").and_then(Value::as_u64) {
            Some(LEGACY_STATE_VERSION) => migrate_v1_durable_state(&mut migrated)?,
            Some(V2_STATE_VERSION) => migrate_v2_durable_state(&mut migrated)?,
            Some(V3_STATE_VERSION) => migrate_v3_durable_state(&mut migrated)?,
            Some(STATE_VERSION) => break,
            _ => return Err("unsupported GitHub durable state version".to_string()),
        }
    }
    validate_durable_state(&migrated)?;
    Ok(migrated)
}

fn validate_durable_state(state: &Value) -> Result<(), String> {
    let object = state
        .as_object()
        .ok_or("GitHub durable state must be a JSON object")?;
    if object
        .keys()
        .any(|key| !STATE_FIELDS.contains(&key.as_str()))
    {
        return Err("GitHub durable state contains unknown fields".to_string());
    }
    if object.get("version").and_then(Value::as_u64) != Some(STATE_VERSION) {
        return Err("unsupported GitHub durable state version".to_string());
    }
    let pull_requests = object
        .get("pull_requests")
        .and_then(Value::as_object)
        .ok_or("GitHub durable state pull_requests must be an object")?;
    for (session_id, links) in pull_requests {
        if session_id.trim().is_empty() {
            return Err("GitHub durable state has an empty session id".to_string());
        }
        if !links.is_array() {
            return Err("a session's pull requests must be a list".to_string());
        }
        parse_session_links(links)?;
    }
    let reconciliation = with_null_defaults(
        object.get("reconciliation").unwrap_or(&Value::Null),
        RECONCILIATION_OPTIONAL_FIELDS,
    );
    let reconciliation = reconciliation
        .as_object()
        .ok_or("GitHub durable state reconciliation must be an object")?;
    let fields: Vec<&str> = ["status", "checked"]
        .into_iter()
        .chain(RECONCILIATION_OPTIONAL_FIELDS.iter().copied())
        .collect();
    require_only_keys(reconciliation, &fields)?;
    if !matches!(
        reconciliation.get("status").and_then(Value::as_str),
        Some("idle" | "running" | "failed")
    ) || reconciliation
        .get("checked")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err("GitHub durable state has invalid reconciliation status".to_string());
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
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn durable_state_from_settings(settings: &Value) -> Result<Value, String> {
        let settings = settings
            .as_object()
            .ok_or("plugin settings must be a JSON object")?;
        let state = settings
            .get(STATE_NAMESPACE)
            .cloned()
            .unwrap_or_else(empty_durable_state);
        migrate_durable_state(&state)
    }

    #[test]
    fn durable_state_accepts_missing_settings_and_rejects_invalid_or_incompatible_documents() {
        assert_eq!(
            durable_state_from_settings(&json!({})).unwrap(),
            empty_durable_state()
        );
        let explicit_current = json!({ "github": empty_durable_state() });
        assert_eq!(
            durable_state_from_settings(&explicit_current).unwrap(),
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
    fn v3_migration_drops_legacy_mappings_on_unsupported_hosts() {
        let mapping = |url: &str| json!({ "session_id": "s", "url": url, "state": "open", "remote": null, "branch": null, "updated_at": 1, "check_summary": null });
        let mut v3 = empty_durable_state();
        v3["version"] = json!(V3_STATE_VERSION);
        v3["pull_requests"] = json!({
            "enterprise": mapping("https://github.example.com/o/r/pull/3"),
            "kept": mapping("https://github.com/o/r/pull/4"),
        });
        let migrated = migrate_durable_state(&v3).unwrap();
        assert_eq!(
            migrated["pull_requests"]["enterprise"],
            json!([]),
            "an empty list, because the host deep-merges the migration and would keep a removed key"
        );
        assert_eq!(migrated["pull_requests"]["kept"][0]["number"], 4);
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
}
