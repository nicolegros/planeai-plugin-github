//! One batched GitHub GraphQL query for every pull request lookup a refresh needs.

use crate::pull_requests::{CheckState, PrKey, PrSnapshot, PrState};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lookup {
    Number {
        number: u64,
        detail: bool,
    },
    /// Same-repository pull requests whose head is the branch, most recently updated first.
    Head {
        branch: String,
        detail: bool,
    },
    /// Open pull requests whose head is the branch: the parent of a stacked pull request.
    OpenHead(String),
    /// Open pull requests based on the branch: the children of a stacked pull request.
    OpenBase(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LookupRequest {
    pub repository: String,
    pub lookup: Lookup,
}

impl LookupRequest {
    pub fn new(repository: &str, lookup: Lookup) -> Self {
        Self {
            repository: repository.to_ascii_lowercase(),
            lookup,
        }
    }

    fn wants_detail(&self) -> bool {
        matches!(
            self.lookup,
            Lookup::Number { detail: true, .. } | Lookup::Head { detail: true, .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrDetail {
    pub checks: Vec<Value>,
    pub conflicting: bool,
    pub merge_blocked: bool,
    pub review_decision: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FetchedPr {
    pub key: PrKey,
    pub url: String,
    pub snapshot: PrSnapshot,
    pub cross_repository: bool,
    pub detail: Option<PrDetail>,
}

/// The panel's view of a pull request fetched with details.
pub fn project_detail(pr: &FetchedPr, merge_methods: Vec<String>) -> Value {
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

#[derive(Debug, Default)]
pub struct GraphqlResponse {
    /// `None` when GitHub could not answer that lookup, so cached state must be kept.
    pub lookups: BTreeMap<LookupRequest, Option<Vec<FetchedPr>>>,
    pub merge_methods: BTreeMap<String, Vec<String>>,
    /// Each answered repository's default branch.
    pub default_branches: BTreeMap<String, String>,
}

const PR_FIELDS: &str = "number url title state isDraft updatedAt headRefName baseRefName isCrossRepository commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";
const PR_DETAIL_FIELDS: &str = "number url title state isDraft updatedAt headRefName baseRefName isCrossRepository mergeable mergeStateStatus reviewDecision commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 100) { nodes { __typename ... on CheckRun { name status conclusion detailsUrl checkSuite { workflowRun { workflow { name } } } } ... on StatusContext { context state targetUrl } } } } } } }";
const LIST_SIZE: u32 = 10;

fn literal(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

/// Repositories in order of first appearance, each with its lookups and their request indexes.
type RepositoryLookups<'a> = Vec<(&'a str, Vec<(usize, &'a LookupRequest)>)>;

fn by_repository(requests: &[LookupRequest]) -> RepositoryLookups<'_> {
    let mut grouped: RepositoryLookups = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        match grouped
            .iter_mut()
            .find(|(repository, _)| *repository == request.repository)
        {
            Some((_, lookups)) => lookups.push((index, request)),
            None => grouped.push((request.repository.as_str(), vec![(index, request)])),
        }
    }
    grouped
}

pub fn build_query(requests: &[LookupRequest]) -> String {
    let mut query = String::from("query {");
    for (repository_index, (repository, lookups)) in by_repository(requests).into_iter().enumerate()
    {
        let (owner, name) = repository.split_once('/').unwrap_or((repository, ""));
        query.push_str(&format!(
            " r{repository_index}: repository(owner: {}, name: {}) {{",
            literal(owner),
            literal(name)
        ));
        if lookups.iter().any(|(_, request)| request.wants_detail()) {
            query.push_str(" squashMergeAllowed mergeCommitAllowed rebaseMergeAllowed");
        }
        for (index, request) in lookups {
            let fields = if request.wants_detail() {
                PR_DETAIL_FIELDS
            } else {
                PR_FIELDS
            };
            let selection = match &request.lookup {
                Lookup::Number { number, .. } => format!("pullRequest(number: {number}) {{ {fields} }}"),
                Lookup::Head { branch, .. } => format!(
                    "pullRequests(headRefName: {}, first: {LIST_SIZE}, orderBy: {{ field: UPDATED_AT, direction: DESC }}) {{ nodes {{ {fields} }} }}",
                    literal(branch)
                ),
                Lookup::OpenHead(branch) => format!(
                    "pullRequests(headRefName: {}, states: OPEN, first: {LIST_SIZE}) {{ nodes {{ {fields} }} }}",
                    literal(branch)
                ),
                Lookup::OpenBase(branch) => format!(
                    "pullRequests(baseRefName: {}, states: OPEN, first: {LIST_SIZE}) {{ nodes {{ {fields} }} }}",
                    literal(branch)
                ),
            };
            query.push_str(&format!(" l{index}: {selection}"));
        }
        query.push_str(" defaultBranchRef { name } }");
    }
    query.push_str(" }");
    query
}

fn string(node: &Value, field: &str) -> Option<String> {
    node.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn rollup(node: &Value) -> Option<&Value> {
    node.pointer("/commits/nodes/0/commit/statusCheckRollup")
        .filter(|rollup| !rollup.is_null())
}

fn check_state(node: &Value) -> Option<CheckState> {
    match rollup(node)?.get("state")?.as_str()? {
        "SUCCESS" => Some(CheckState::Passing),
        "FAILURE" | "ERROR" => Some(CheckState::Failing),
        "PENDING" | "EXPECTED" => Some(CheckState::Pending),
        _ => None,
    }
}

/// Projects check contexts onto the `gh pr view --json statusCheckRollup` item shape the panel reads.
fn check_items(node: &Value) -> Vec<Value> {
    rollup(node)
        .and_then(|rollup| rollup.pointer("/contexts/nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|context| match context.get("__typename").and_then(Value::as_str) {
            Some("StatusContext") => {
                let state = context.get("state").cloned().unwrap_or(Value::Null);
                let pending = matches!(state.as_str(), Some("PENDING" | "EXPECTED"));
                json!({
                    "name": context.get("context").cloned().unwrap_or(Value::Null),
                    "conclusion": if pending { Value::Null } else { state.clone() },
                    "status": if pending { state } else { json!("COMPLETED") },
                    "detailsUrl": context.get("targetUrl").cloned().unwrap_or(Value::Null),
                })
            }
            _ => json!({
                "name": context.get("name").cloned().unwrap_or(Value::Null),
                "workflowName": context.pointer("/checkSuite/workflowRun/workflow/name").cloned().unwrap_or(Value::Null),
                "conclusion": context.get("conclusion").cloned().unwrap_or(Value::Null),
                "status": context.get("status").cloned().unwrap_or(Value::Null),
                "detailsUrl": context.get("detailsUrl").cloned().unwrap_or(Value::Null),
            }),
        })
        .collect()
}

fn parse_pr(node: &Value, detail: bool, now: u64) -> Option<FetchedPr> {
    let url = string(node, "url")?;
    let key = PrKey::from_url(&url)?;
    let state = match node.get("state")?.as_str()? {
        "OPEN" if node.get("isDraft").and_then(Value::as_bool) == Some(true) => PrState::Draft,
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => return None,
    };
    let detail = detail.then(|| {
        let mergeable = node
            .get("mergeable")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let merge_state = node
            .get("mergeStateStatus")
            .and_then(Value::as_str)
            .unwrap_or_default();
        PrDetail {
            checks: check_items(node),
            conflicting: mergeable == "CONFLICTING" || merge_state == "DIRTY",
            merge_blocked: merge_state == "BLOCKED",
            review_decision: node.get("reviewDecision").cloned().unwrap_or(Value::Null),
        }
    });
    Some(FetchedPr {
        key,
        url,
        snapshot: PrSnapshot {
            state,
            title: string(node, "title"),
            checks: check_state(node),
            head_branch: string(node, "headRefName"),
            base_branch: string(node, "baseRefName"),
            updated_at: string(node, "updatedAt"),
            refreshed_at: now,
        },
        cross_repository: node.get("isCrossRepository").and_then(Value::as_bool) == Some(true),
        detail,
    })
}

/// Reads `gh api graphql` output; GitHub answers partially when some lookups fail, so `data` may be incomplete.
pub fn parse_response(requests: &[LookupRequest], body: &Value, now: u64) -> GraphqlResponse {
    let mut response = GraphqlResponse::default();
    let data = body.get("data").unwrap_or(&Value::Null);
    for (repository_index, (repository, lookups)) in by_repository(requests).into_iter().enumerate()
    {
        let node = data
            .get(format!("r{repository_index}"))
            .filter(|node| !node.is_null());
        if let Some(node) = node {
            if let Some(name) = node
                .pointer("/defaultBranchRef/name")
                .and_then(Value::as_str)
            {
                response
                    .default_branches
                    .insert(repository.to_string(), name.to_string());
            }
            if lookups.iter().any(|(_, request)| request.wants_detail()) {
                let methods = [
                    ("squash", "squashMergeAllowed"),
                    ("merge", "mergeCommitAllowed"),
                    ("rebase", "rebaseMergeAllowed"),
                ]
                .into_iter()
                .filter(|(_, field)| node.get(field).and_then(Value::as_bool) == Some(true))
                .map(|(method, _)| method.to_string())
                .collect();
                response
                    .merge_methods
                    .insert(repository.to_string(), methods);
            }
        }
        for (index, request) in lookups {
            let detail = request.wants_detail();
            let result = node
                .and_then(|node| node.get(format!("l{index}")))
                .and_then(|field| match &request.lookup {
                    Lookup::Number { .. } => parse_pr(field, detail, now).map(|pr| vec![pr]),
                    _ => field.get("nodes").and_then(Value::as_array).map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|node| parse_pr(node, detail, now))
                            .collect()
                    }),
                });
            response.lookups.insert(request.clone(), result);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(number: u64, state: &str, draft: bool, head: &str, base: &str, rollup: Value) -> Value {
        json!({
            "number": number, "url": format!("https://github.com/O/R/pull/{number}"), "title": format!("Title {number}"),
            "state": state, "isDraft": draft, "updatedAt": "2026-10-01T00:00:00Z",
            "headRefName": head, "baseRefName": base, "isCrossRepository": false,
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": rollup } }] },
        })
    }

    #[test]
    fn query_groups_lookups_by_repository_with_stable_aliases_and_escaped_branches() {
        let requests = vec![
            LookupRequest::new(
                "o/r",
                Lookup::Number {
                    number: 4,
                    detail: false,
                },
            ),
            LookupRequest::new("x/y", Lookup::OpenBase("feat/\"quoted\"".into())),
            LookupRequest::new(
                "o/r",
                Lookup::Head {
                    branch: "topic".into(),
                    detail: true,
                },
            ),
            LookupRequest::new("o/r", Lookup::OpenHead("base".into())),
        ];
        let query = build_query(&requests);
        assert!(query
            .starts_with("query { r0: repository(owner: \"o\", name: \"r\") { squashMergeAllowed"));
        assert!(query.contains("l0: pullRequest(number: 4) { number url"));
        assert!(query.contains("l2: pullRequests(headRefName: \"topic\", first: 10, orderBy: { field: UPDATED_AT, direction: DESC }) { nodes { number url title state isDraft updatedAt headRefName baseRefName isCrossRepository mergeable"));
        assert!(query.contains("l3: pullRequests(headRefName: \"base\", states: OPEN, first: 10)"));
        assert!(query.contains("r1: repository(owner: \"x\", name: \"y\") { l1: pullRequests(baseRefName: \"feat/\\\"quoted\\\"\", states: OPEN"));
        assert!(query.contains("states: OPEN, first: 10) { nodes { number url title state isDraft updatedAt headRefName baseRefName isCrossRepository commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } } } defaultBranchRef { name } }"));
        assert_eq!(query.matches("defaultBranchRef { name }").count(), 2);
        assert_eq!(query.matches('{').count(), query.matches('}').count());
    }

    #[test]
    fn response_maps_states_checks_and_partial_failures() {
        let requests = vec![
            LookupRequest::new(
                "o/r",
                Lookup::Number {
                    number: 1,
                    detail: false,
                },
            ),
            LookupRequest::new(
                "o/r",
                Lookup::Number {
                    number: 2,
                    detail: false,
                },
            ),
            LookupRequest::new("o/r", Lookup::OpenBase("a".into())),
            LookupRequest::new(
                "gone/repo",
                Lookup::Number {
                    number: 3,
                    detail: false,
                },
            ),
        ];
        let body = json!({
            "data": {
                "r0": { "l0": Value::Null, "l1": node(2, "OPEN", true, "b", "a", json!({ "state": "FAILURE" })), "l2": { "nodes": [
                    node(5, "OPEN", false, "c", "a", Value::Null),
                    node(6, "MERGED", false, "d", "a", json!({ "state": "SUCCESS" })),
                ] } },
                "r1": Value::Null,
            },
            "errors": [{ "type": "NOT_FOUND" }],
        });
        let response = parse_response(&requests, &body, 77);
        assert_eq!(response.lookups[&requests[0]], None);
        let draft = &response.lookups[&requests[1]].as_ref().unwrap()[0];
        assert_eq!(
            draft.key,
            PrKey::from_url("https://github.com/o/r/pull/2").unwrap()
        );
        assert_eq!(draft.snapshot.state, PrState::Draft);
        assert_eq!(draft.snapshot.checks, Some(CheckState::Failing));
        assert_eq!(draft.snapshot.head_branch.as_deref(), Some("b"));
        assert_eq!(draft.snapshot.refreshed_at, 77);
        assert!(draft.detail.is_none());
        let children = response.lookups[&requests[2]].as_ref().unwrap();
        assert_eq!(
            children
                .iter()
                .map(|pr| pr.snapshot.state)
                .collect::<Vec<_>>(),
            vec![PrState::Open, PrState::Merged]
        );
        assert_eq!(children[0].snapshot.checks, None);
        assert_eq!(children[1].snapshot.checks, Some(CheckState::Passing));
        assert_eq!(response.lookups[&requests[3]], None);
        assert!(response.merge_methods.is_empty());
    }

    #[test]
    fn detail_lookups_project_checks_mergeability_and_merge_methods() {
        let requests = vec![LookupRequest::new(
            "o/r",
            Lookup::Head {
                branch: "topic".into(),
                detail: true,
            },
        )];
        let mut pr = node(
            9,
            "OPEN",
            false,
            "topic",
            "main",
            json!({ "state": "PENDING", "contexts": { "nodes": [
            { "__typename": "CheckRun", "name": "test", "status": "IN_PROGRESS", "conclusion": null, "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2", "checkSuite": { "workflowRun": { "workflow": { "name": "CI" } } } },
            { "__typename": "StatusContext", "context": "deploy", "state": "FAILURE", "targetUrl": "https://ci" },
            { "__typename": "StatusContext", "context": "lint", "state": "PENDING", "targetUrl": null },
        ] } }),
        );
        pr["mergeable"] = json!("MERGEABLE");
        pr["mergeStateStatus"] = json!("BLOCKED");
        pr["reviewDecision"] = json!("APPROVED");
        let body = json!({ "data": { "r0": {
            "squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": true,
            "l0": { "nodes": [pr] }, "defaultBranchRef": { "name": "trunk" },
        } } });
        let response = parse_response(&requests, &body, 1);
        assert_eq!(response.merge_methods["o/r"], vec!["squash", "rebase"]);
        assert_eq!(response.default_branches["o/r"], "trunk");
        let fetched = &response.lookups[&requests[0]].as_ref().unwrap()[0];
        let detail = fetched.detail.as_ref().unwrap();
        assert!(detail.merge_blocked);
        assert!(!detail.conflicting);
        assert_eq!(detail.review_decision, "APPROVED");
        assert_eq!(
            detail.checks,
            vec![
                json!({ "name": "test", "workflowName": "CI", "conclusion": null, "status": "IN_PROGRESS", "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2" }),
                json!({ "name": "deploy", "conclusion": "FAILURE", "status": "COMPLETED", "detailsUrl": "https://ci" }),
                json!({ "name": "lint", "conclusion": null, "status": "PENDING", "detailsUrl": null }),
            ]
        );
    }
}
