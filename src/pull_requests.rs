//! The pull requests tracked for each session, as persisted under `github.pull_requests.<session>`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const GITHUB_HOST: &str = "github.com";

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrKey {
    pub host: String,
    /// `owner/name`, lowercased because GitHub compares repository names case-insensitively.
    pub repository: String,
    pub number: u64,
}

impl PrKey {
    pub fn new(host: &str, repository: &str, number: u64) -> Result<Self, String> {
        if host != GITHUB_HOST {
            return Err(format!("unsupported pull request host {host}"));
        }
        if number == 0 || !is_repository_slug(repository) {
            return Err("pull request reference must name owner/repository and a number".into());
        }
        Ok(Self {
            host: host.to_string(),
            repository: repository.to_ascii_lowercase(),
            number,
        })
    }

    /// Parses `https://github.com/<owner>/<name>/pull/<number>` with an optional trailing path, query, or fragment.
    pub fn from_url(url: &str) -> Option<Self> {
        let rest = url
            .trim()
            .strip_prefix("https://")
            .or_else(|| url.trim().strip_prefix("http://"))?;
        let rest = rest.split(['?', '#']).next()?;
        let mut segments = rest.split('/');
        let host = segments.next()?.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let owner = segments.next()?;
        let name = segments.next()?;
        if !matches!(segments.next()?, "pull" | "pulls") {
            return None;
        }
        let number = segments.next()?.parse().ok()?;
        Self::new(&host, &format!("{owner}/{name}"), number).ok()
    }
}

fn is_repository_slug(value: &str) -> bool {
    let mut segments = value.split('/');
    let valid = |segment: Option<&str>| {
        segment.is_some_and(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    };
    valid(segments.next()) && valid(segments.next()) && segments.next().is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Draft,
    Open,
    Merged,
    Closed,
}

impl PrState {
    /// Open on GitHub, whether or not it is still a draft.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Draft | Self::Open)
    }
}

/// Ordered from best to worst, so the worst of several is their maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
    Passing,
    Pending,
    Failing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackedSource {
    /// Detected from the session's branch.
    Branch,
    /// Opened from the session through Create PR.
    Created,
    /// Linked by the user.
    Manual,
    /// Discovered through a base-branch chain from another tracked pull request.
    Stack,
}

impl TrackedSource {
    /// Lower is preferred when several sessions of a task track the same pull request.
    fn precedence(self) -> u8 {
        match self {
            Self::Branch => 0,
            Self::Created => 1,
            Self::Manual => 2,
            Self::Stack => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    pub state: PrState,
    pub title: Option<String>,
    pub checks: Option<CheckState>,
    pub head_branch: Option<String>,
    pub base_branch: Option<String>,
    /// GitHub's `updatedAt` (RFC 3339, UTC), so it orders lexicographically.
    pub updated_at: Option<String>,
    /// When the plugin last stored this snapshot, in Unix milliseconds.
    pub refreshed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    Tracked {
        source: TrackedSource,
        snapshot: PrSnapshot,
    },
    /// The user removed it; stack discovery must not add it back.
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestLink {
    pub key: PrKey,
    pub url: String,
    pub status: LinkStatus,
}

impl PullRequestLink {
    pub fn tracked(key: PrKey, url: &str, source: TrackedSource, snapshot: PrSnapshot) -> Self {
        Self {
            key,
            url: url.to_string(),
            status: LinkStatus::Tracked { source, snapshot },
        }
    }

    pub fn snapshot(&self) -> Option<&PrSnapshot> {
        match &self.status {
            LinkStatus::Tracked { snapshot, .. } => Some(snapshot),
            LinkStatus::Dismissed => None,
        }
    }

    pub fn source(&self) -> Option<TrackedSource> {
        match &self.status {
            LinkStatus::Tracked { source, .. } => Some(*source),
            LinkStatus::Dismissed => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SourceRecord {
    Branch,
    Created,
    Manual,
    Stack,
    StackDismissed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkRecord {
    host: String,
    repository: String,
    number: u64,
    url: String,
    source: SourceRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<PrState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checks: Option<CheckState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refreshed_at: Option<u64>,
}

impl TryFrom<LinkRecord> for PullRequestLink {
    type Error = String;

    fn try_from(record: LinkRecord) -> Result<Self, String> {
        let key = PrKey::new(&record.host, &record.repository, record.number)?;
        if record.url.trim().is_empty() {
            return Err("pull request link has an empty url".into());
        }
        let source = match record.source {
            SourceRecord::Branch => TrackedSource::Branch,
            SourceRecord::Created => TrackedSource::Created,
            SourceRecord::Manual => TrackedSource::Manual,
            SourceRecord::Stack => TrackedSource::Stack,
            SourceRecord::StackDismissed => {
                let has_snapshot = record.state.is_some()
                    || record.title.is_some()
                    || record.checks.is_some()
                    || record.head_branch.is_some()
                    || record.base_branch.is_some()
                    || record.updated_at.is_some()
                    || record.refreshed_at.is_some();
                if has_snapshot {
                    return Err("a dismissed pull request link carries no status".into());
                }
                return Ok(Self {
                    key,
                    url: record.url,
                    status: LinkStatus::Dismissed,
                });
            }
        };
        let (Some(state), Some(refreshed_at)) = (record.state, record.refreshed_at) else {
            return Err("a tracked pull request link requires state and refreshed_at".into());
        };
        let non_empty = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
        Ok(Self::tracked(
            key,
            &record.url,
            source,
            PrSnapshot {
                state,
                title: non_empty(record.title),
                checks: record.checks,
                head_branch: non_empty(record.head_branch),
                base_branch: non_empty(record.base_branch),
                updated_at: non_empty(record.updated_at),
                refreshed_at,
            },
        ))
    }
}

impl From<&PullRequestLink> for LinkRecord {
    fn from(link: &PullRequestLink) -> Self {
        let mut record = LinkRecord {
            host: link.key.host.clone(),
            repository: link.key.repository.clone(),
            number: link.key.number,
            url: link.url.clone(),
            source: SourceRecord::StackDismissed,
            state: None,
            title: None,
            checks: None,
            head_branch: None,
            base_branch: None,
            updated_at: None,
            refreshed_at: None,
        };
        if let LinkStatus::Tracked { source, snapshot } = &link.status {
            record.source = match source {
                TrackedSource::Branch => SourceRecord::Branch,
                TrackedSource::Created => SourceRecord::Created,
                TrackedSource::Manual => SourceRecord::Manual,
                TrackedSource::Stack => SourceRecord::Stack,
            };
            record.state = Some(snapshot.state);
            record.title = snapshot.title.clone();
            record.checks = snapshot.checks;
            record.head_branch = snapshot.head_branch.clone();
            record.base_branch = snapshot.base_branch.clone();
            record.updated_at = snapshot.updated_at.clone();
            record.refreshed_at = Some(snapshot.refreshed_at);
        }
        record
    }
}

/// The single mapping a session held before it could track several pull requests.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMapping {
    #[allow(dead_code)]
    session_id: String,
    url: String,
    state: PrState,
    #[allow(dead_code)]
    remote: Option<String>,
    #[allow(dead_code)]
    branch: Option<String>,
    updated_at: u64,
    #[serde(default)]
    check_summary: Option<LegacyCheckSummary>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCheckSummary {
    state: CheckState,
    #[allow(dead_code)]
    updated_at: u64,
}

/// Reads a session's stored links, accepting the legacy single-object shape as a one-element list.
pub fn parse_session_links(value: &Value) -> Result<Vec<PullRequestLink>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(records) => {
            let mut links: Vec<PullRequestLink> = Vec::with_capacity(records.len());
            for record in records {
                let record: LinkRecord = serde_json::from_value(record.clone())
                    .map_err(|error| format!("invalid pull request link: {error}"))?;
                let link = PullRequestLink::try_from(record)?;
                if !links.iter().any(|existing| existing.key == link.key) {
                    links.push(link);
                }
            }
            Ok(links)
        }
        Value::Object(_) => {
            let legacy = parse_legacy_mapping(value)?;
            let url = value.get("url").and_then(Value::as_str).unwrap_or_default();
            Ok(vec![legacy.ok_or_else(|| {
                format!("legacy pull request url {url} is not supported")
            })?])
        }
        _ => Err("a session's pull requests must be a list".into()),
    }
}

/// Reads the single mapping a session held before version 4; `None` when its URL is not a
/// supported pull request, such as one on a GitHub Enterprise host.
pub fn parse_legacy_mapping(value: &Value) -> Result<Option<PullRequestLink>, String> {
    let legacy: LegacyMapping = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid legacy pull request mapping: {error}"))?;
    let Some(key) = PrKey::from_url(&legacy.url) else {
        return Ok(None);
    };
    // The legacy shape did not record why the PR was tracked, so keep it as a removable link.
    Ok(Some(PullRequestLink::tracked(
        key,
        &legacy.url,
        TrackedSource::Manual,
        PrSnapshot {
            state: legacy.state,
            title: None,
            checks: legacy.check_summary.map(|summary| summary.state),
            head_branch: None,
            base_branch: None,
            updated_at: None,
            refreshed_at: legacy.updated_at,
        },
    )))
}

pub fn session_links_value(links: &[PullRequestLink]) -> Value {
    Value::Array(
        links
            .iter()
            .map(|link| serde_json::to_value(LinkRecord::from(link)).expect("links serialize"))
            .collect(),
    )
}

fn position(links: &[PullRequestLink], key: &PrKey) -> Option<usize> {
    links.iter().position(|link| &link.key == key)
}

/// Replaces the session's branch link with the pull request its branch resolves to now, if any.
pub fn record_branch_pr(
    links: &mut Vec<PullRequestLink>,
    detected: Option<(&PrKey, &str, &PrSnapshot)>,
) {
    links.retain(|link| {
        link.source() != Some(TrackedSource::Branch)
            || detected.is_some_and(|(key, ..)| *key == link.key)
    });
    let Some((key, url, snapshot)) = detected else {
        return;
    };
    match position(links, key) {
        Some(index) => {
            let source = match links[index].source() {
                Some(TrackedSource::Created) => TrackedSource::Created,
                _ => TrackedSource::Branch,
            };
            links[index] = PullRequestLink::tracked(key.clone(), url, source, snapshot.clone());
        }
        None => links.push(PullRequestLink::tracked(
            key.clone(),
            url,
            TrackedSource::Branch,
            snapshot.clone(),
        )),
    }
}

pub fn record_created(
    links: &mut Vec<PullRequestLink>,
    key: &PrKey,
    url: &str,
    snapshot: &PrSnapshot,
) {
    let link = PullRequestLink::tracked(key.clone(), url, TrackedSource::Created, snapshot.clone());
    match position(links, key) {
        Some(index) => links[index] = link,
        None => links.push(link),
    }
}

/// Appends a user-linked pull request; one the session already tracks keeps its source.
pub fn record_manual(
    links: &mut Vec<PullRequestLink>,
    key: &PrKey,
    url: &str,
    snapshot: &PrSnapshot,
) {
    let source = match position(links, key).and_then(|index| links[index].source()) {
        Some(source @ (TrackedSource::Branch | TrackedSource::Created | TrackedSource::Manual)) => {
            source
        }
        _ => TrackedSource::Manual,
    };
    let link = PullRequestLink::tracked(key.clone(), url, source, snapshot.clone());
    match position(links, key) {
        Some(index) => links[index] = link,
        None => links.push(link),
    }
}

/// Adds a stack pull request unless the session already has an entry for it, dismissed or not.
pub fn record_stack(
    links: &mut Vec<PullRequestLink>,
    key: &PrKey,
    url: &str,
    snapshot: &PrSnapshot,
) {
    if position(links, key).is_none() {
        links.push(PullRequestLink::tracked(
            key.clone(),
            url,
            TrackedSource::Stack,
            snapshot.clone(),
        ));
    }
}

/// Stores a fresh snapshot for a tracked pull request; returns whether the session tracks it.
pub fn refresh_snapshot(links: &mut [PullRequestLink], key: &PrKey, snapshot: &PrSnapshot) -> bool {
    let Some(index) = position(links, key) else {
        return false;
    };
    match &mut links[index].status {
        LinkStatus::Tracked {
            snapshot: stored, ..
        } => {
            *stored = snapshot.clone();
            true
        }
        LinkStatus::Dismissed => false,
    }
}

/// Turns a removable link into a tombstone; branch links follow the branch and cannot be removed.
pub fn dismiss(links: &mut [PullRequestLink], key: &PrKey) -> bool {
    let Some(index) = position(links, key) else {
        return false;
    };
    match links[index].source() {
        Some(TrackedSource::Created | TrackedSource::Manual | TrackedSource::Stack) => {
            links[index].status = LinkStatus::Dismissed;
            true
        }
        _ => false,
    }
}

/// The session's own pull request that the panel details and merges: its branch's, else one it
/// created, else one it linked, in the session's repository; the most recently updated wins a tie.
pub fn own_link<'a>(links: &'a [PullRequestLink], repository: &str) -> Option<&'a PullRequestLink> {
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

/// One pull request as a task sees it across all of its sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregatedPr {
    pub key: PrKey,
    pub url: String,
    pub source: TrackedSource,
    pub snapshot: PrSnapshot,
}

impl AggregatedPr {
    pub fn to_json(&self) -> Value {
        let snapshot = &self.snapshot;
        json!({
            "host": self.key.host,
            "repository": self.key.repository,
            "number": self.key.number,
            "url": self.url,
            "source": self.source,
            "state": snapshot.state,
            "title": snapshot.title,
            "checks": snapshot.checks,
            "head_branch": snapshot.head_branch,
            "base_branch": snapshot.base_branch,
            "updated_at": snapshot.updated_at,
        })
    }
}

/// The union of several sessions' links, deduplicated by key.
/// A tombstone in any session hides a pull request that only stack discovery tracks.
pub fn aggregate<'a>(lists: impl IntoIterator<Item = &'a [PullRequestLink]>) -> Vec<AggregatedPr> {
    let mut merged: BTreeMap<PrKey, AggregatedPr> = BTreeMap::new();
    let mut dismissed = BTreeSet::new();
    for link in lists.into_iter().flatten() {
        let LinkStatus::Tracked { source, snapshot } = &link.status else {
            dismissed.insert(link.key.clone());
            continue;
        };
        match merged.get_mut(&link.key) {
            None => {
                merged.insert(
                    link.key.clone(),
                    AggregatedPr {
                        key: link.key.clone(),
                        url: link.url.clone(),
                        source: *source,
                        snapshot: snapshot.clone(),
                    },
                );
            }
            Some(existing) => {
                if source.precedence() < existing.source.precedence() {
                    existing.source = *source;
                }
                if snapshot.refreshed_at > existing.snapshot.refreshed_at {
                    existing.snapshot = snapshot.clone();
                    existing.url = link.url.clone();
                }
            }
        }
    }
    merged
        .into_values()
        .filter(|pr| pr.source != TrackedSource::Stack || !dismissed.contains(&pr.key))
        .collect()
}

/// Every key any of the lists mentions, tombstones included.
pub fn known_keys<'a>(lists: impl IntoIterator<Item = &'a [PullRequestLink]>) -> BTreeSet<PrKey> {
    lists
        .into_iter()
        .flatten()
        .map(|link| link.key.clone())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrStateTransition {
    Opened,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskPhase {
    /// Nothing tracked has opened yet (or everything was closed unmerged).
    Inactive,
    Open,
    /// Every tracked pull request is merged or closed, and at least one merged.
    Done,
}

pub fn task_phase(prs: &[AggregatedPr]) -> TaskPhase {
    let states = prs.iter().map(|pr| pr.snapshot.state);
    let any_merged = states.clone().any(|state| state == PrState::Merged);
    if any_merged && states.clone().all(|state| !state.is_live()) {
        TaskPhase::Done
    } else if states
        .into_iter()
        .any(|state| matches!(state, PrState::Open | PrState::Merged))
    {
        TaskPhase::Open
    } else {
        TaskPhase::Inactive
    }
}

pub fn task_transition(before: TaskPhase, after: TaskPhase) -> Option<PrStateTransition> {
    match (before, after) {
        (TaskPhase::Inactive, TaskPhase::Open) => Some(PrStateTransition::Opened),
        (TaskPhase::Open, TaskPhase::Done) => Some(PrStateTransition::Merged),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub project_id: Option<String>,
    pub task_key: Option<String>,
    pub status: Option<String>,
    pub branch: Option<String>,
}

pub fn parse_sessions(response: &Value) -> Vec<SessionInfo> {
    let text = |session: &Value, field: &str| {
        session
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    };
    response
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|session| {
            Some(SessionInfo {
                id: text(session, "id")?,
                project_id: text(session, "project_id"),
                task_key: text(session, "task_key"),
                status: text(session, "status"),
                branch: text(session, "branch"),
            })
        })
        .collect()
}

/// The session followed by every other session linked to the same task in the same project.
pub fn task_group(sessions: &[SessionInfo], session_id: &str) -> Vec<String> {
    let mut group = vec![session_id.to_string()];
    let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
        return group;
    };
    let Some(task_key) = &session.task_key else {
        return group;
    };
    let mut siblings: Vec<String> = sessions
        .iter()
        .filter(|other| {
            other.id != session_id
                && other.task_key.as_ref() == Some(task_key)
                && other.project_id == session.project_id
        })
        .map(|other| other.id.clone())
        .collect();
    siblings.sort();
    group.extend(siblings);
    group
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(number: u64) -> PrKey {
        PrKey::new(GITHUB_HOST, "o/r", number).unwrap()
    }

    fn snapshot(state: PrState, refreshed_at: u64) -> PrSnapshot {
        PrSnapshot {
            state,
            title: Some(format!("PR {refreshed_at}")),
            checks: None,
            head_branch: None,
            base_branch: None,
            updated_at: None,
            refreshed_at,
        }
    }

    fn link(number: u64, source: TrackedSource, state: PrState) -> PullRequestLink {
        PullRequestLink::tracked(
            key(number),
            &format!("https://github.com/o/r/pull/{number}"),
            source,
            snapshot(state, 1),
        )
    }

    fn tombstone(number: u64) -> PullRequestLink {
        PullRequestLink {
            key: key(number),
            url: format!("https://github.com/o/r/pull/{number}"),
            status: LinkStatus::Dismissed,
        }
    }

    #[test]
    fn keys_parse_github_pull_request_urls_case_insensitively() {
        assert_eq!(
            PrKey::from_url("https://github.com/Owner/Repo/pull/42/files?x=1#y"),
            Some(PrKey::new(GITHUB_HOST, "owner/repo", 42).unwrap())
        );
        assert_eq!(PrKey::from_url("https://github.com/o/r/issues/42"), None);
        assert_eq!(PrKey::from_url("https://gitlab.com/o/r/pull/42"), None);
        assert_eq!(PrKey::from_url("https://github.com/o/r/pull/0"), None);
        assert_eq!(PrKey::from_url("https://github.com/o/pull/4"), None);
    }

    #[test]
    fn links_round_trip_through_their_stored_shape() {
        let mut tracked = link(7, TrackedSource::Stack, PrState::Open);
        if let LinkStatus::Tracked { snapshot, .. } = &mut tracked.status {
            snapshot.checks = Some(CheckState::Failing);
            snapshot.head_branch = Some("b".into());
            snapshot.base_branch = Some("a".into());
            snapshot.updated_at = Some("2026-10-01T00:00:00Z".into());
        }
        let links = vec![tracked, tombstone(8)];
        let value = session_links_value(&links);
        assert_eq!(value[0]["source"], "stack");
        assert_eq!(value[0]["checks"], "failing");
        assert_eq!(
            value[1],
            json!({ "host": "github.com", "repository": "o/r", "number": 8, "url": "https://github.com/o/r/pull/8", "source": "stack-dismissed" })
        );
        assert_eq!(parse_session_links(&value).unwrap(), links);
    }

    #[test]
    fn stored_links_reject_illegal_states() {
        for record in [
            json!([{ "host": "github.com", "repository": "o/r", "number": 1, "url": "u", "source": "stack-dismissed", "state": "open" }]),
            json!([{ "host": "github.com", "repository": "o/r", "number": 1, "url": "u", "source": "manual" }]),
            json!([{ "host": "gitlab.com", "repository": "o/r", "number": 1, "url": "u", "source": "manual", "state": "open", "refreshed_at": 1 }]),
            json!([{ "host": "github.com", "repository": "o/r", "number": 1, "url": "u", "source": "other", "state": "open", "refreshed_at": 1 }]),
            json!([{ "host": "github.com", "repository": "o/r", "number": 1, "url": "u", "source": "manual", "state": "open", "refreshed_at": 1, "extra": true }]),
            json!("not a list"),
        ] {
            assert!(parse_session_links(&record).is_err(), "{record}");
        }
    }

    #[test]
    fn legacy_single_mapping_migrates_to_one_link_idempotently() {
        let legacy = json!({
            "session_id": "s1", "url": "https://github.com/o/r/pull/3", "state": "open",
            "remote": "https://github.com/o/r.git", "branch": "topic", "updated_at": 9,
            "check_summary": { "state": "pending", "updated_at": 9 },
        });
        let links = parse_session_links(&legacy).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].key, key(3));
        assert_eq!(links[0].source(), Some(TrackedSource::Manual));
        let snapshot = links[0].snapshot().unwrap();
        assert_eq!(snapshot.state, PrState::Open);
        assert_eq!(snapshot.checks, Some(CheckState::Pending));
        assert_eq!(snapshot.refreshed_at, 9);
        let migrated = session_links_value(&links);
        assert_eq!(parse_session_links(&migrated).unwrap(), links);
        assert_eq!(
            session_links_value(&parse_session_links(&migrated).unwrap()),
            migrated
        );
    }

    #[test]
    fn duplicate_stored_keys_keep_the_first_entry() {
        let value = session_links_value(&[link(1, TrackedSource::Manual, PrState::Open)]);
        let doubled = json!([value[0], value[0]]);
        assert_eq!(parse_session_links(&doubled).unwrap().len(), 1);
    }

    #[test]
    fn branch_detection_replaces_only_the_branch_link() {
        let mut links = vec![
            link(1, TrackedSource::Branch, PrState::Closed),
            link(2, TrackedSource::Manual, PrState::Open),
        ];
        let fresh = snapshot(PrState::Open, 5);
        record_branch_pr(
            &mut links,
            Some((&key(3), "https://github.com/o/r/pull/3", &fresh)),
        );
        assert_eq!(
            links.iter().map(|l| l.key.number).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(links[1].source(), Some(TrackedSource::Branch));

        record_branch_pr(&mut links, None);
        assert_eq!(
            links.iter().map(|l| l.key.number).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn branch_detection_adopts_manual_stack_and_dismissed_links_but_keeps_created() {
        let fresh = snapshot(PrState::Open, 5);
        for (existing, expected) in [
            (
                link(1, TrackedSource::Manual, PrState::Open),
                TrackedSource::Branch,
            ),
            (
                link(1, TrackedSource::Stack, PrState::Open),
                TrackedSource::Branch,
            ),
            (tombstone(1), TrackedSource::Branch),
            (
                link(1, TrackedSource::Created, PrState::Draft),
                TrackedSource::Created,
            ),
        ] {
            let mut links = vec![existing];
            record_branch_pr(
                &mut links,
                Some((&key(1), "https://github.com/o/r/pull/1", &fresh)),
            );
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].source(), Some(expected));
            assert_eq!(links[0].snapshot(), Some(&fresh));
        }
    }

    #[test]
    fn manual_links_append_and_revive_dismissed_or_stack_entries() {
        let fresh = snapshot(PrState::Open, 5);
        let mut links = vec![link(1, TrackedSource::Branch, PrState::Open)];
        record_manual(&mut links, &key(2), "https://github.com/o/r/pull/2", &fresh);
        assert_eq!(links.len(), 2);
        assert_eq!(links[1].source(), Some(TrackedSource::Manual));

        record_manual(&mut links, &key(1), "https://github.com/o/r/pull/1", &fresh);
        assert_eq!(
            links[0].source(),
            Some(TrackedSource::Branch),
            "linking a tracked PR keeps its source"
        );

        let mut links = vec![tombstone(4), link(5, TrackedSource::Stack, PrState::Open)];
        record_manual(&mut links, &key(4), "https://github.com/o/r/pull/4", &fresh);
        record_manual(&mut links, &key(5), "https://github.com/o/r/pull/5", &fresh);
        assert_eq!(links[0].source(), Some(TrackedSource::Manual));
        assert_eq!(links[1].source(), Some(TrackedSource::Manual));
    }

    #[test]
    fn stack_discovery_never_overrides_an_existing_entry() {
        let fresh = snapshot(PrState::Open, 5);
        let mut links = vec![tombstone(4)];
        record_stack(&mut links, &key(4), "https://github.com/o/r/pull/4", &fresh);
        assert_eq!(links, vec![tombstone(4)]);
        record_stack(&mut links, &key(6), "https://github.com/o/r/pull/6", &fresh);
        assert_eq!(links[1].source(), Some(TrackedSource::Stack));
    }

    #[test]
    fn dismiss_writes_tombstones_for_every_source_but_branch() {
        let mut links = vec![
            link(1, TrackedSource::Branch, PrState::Open),
            link(2, TrackedSource::Created, PrState::Open),
            link(3, TrackedSource::Manual, PrState::Open),
            link(4, TrackedSource::Stack, PrState::Open),
        ];
        assert!(!dismiss(&mut links, &key(1)));
        assert!(dismiss(&mut links, &key(2)));
        assert!(dismiss(&mut links, &key(3)));
        assert!(dismiss(&mut links, &key(4)));
        assert!(
            !dismiss(&mut links, &key(4)),
            "dismissing twice changes nothing"
        );
        assert!(!dismiss(&mut links, &key(9)));
        assert_eq!(links[0].source(), Some(TrackedSource::Branch));
        assert!(links[1..]
            .iter()
            .all(|link| link.status == LinkStatus::Dismissed));
    }

    fn updated(mut link: PullRequestLink, updated_at: &str) -> PullRequestLink {
        if let LinkStatus::Tracked { snapshot, .. } = &mut link.status {
            snapshot.updated_at = Some(updated_at.into());
        }
        link
    }

    #[test]
    fn own_link_prefers_branch_then_created_then_manual() {
        let links = vec![
            link(1, TrackedSource::Stack, PrState::Open),
            link(2, TrackedSource::Manual, PrState::Open),
            link(3, TrackedSource::Created, PrState::Open),
            link(4, TrackedSource::Branch, PrState::Merged),
        ];
        assert_eq!(own_link(&links, "o/r").unwrap().key, key(4));
        assert_eq!(own_link(&links[..3], "o/r").unwrap().key, key(3));
        assert_eq!(own_link(&links[..2], "o/r").unwrap().key, key(2));
        assert_eq!(
            own_link(&links[..1], "o/r"),
            None,
            "a stack pull request is never the session's own"
        );
        assert_eq!(own_link(&[tombstone(5)], "o/r"), None);
    }

    #[test]
    fn own_link_only_considers_the_session_repository() {
        let foreign = PullRequestLink::tracked(
            PrKey::new(GITHUB_HOST, "x/y", 9).unwrap(),
            "https://github.com/x/y/pull/9",
            TrackedSource::Branch,
            snapshot(PrState::Open, 1),
        );
        let links = vec![foreign, link(2, TrackedSource::Manual, PrState::Open)];
        assert_eq!(own_link(&links, "o/r").unwrap().key, key(2));
        assert_eq!(own_link(&links, "x/y").unwrap().key.number, 9);
        assert_eq!(own_link(&links, "a/b"), None);
    }

    #[test]
    fn own_link_breaks_a_tie_with_the_most_recently_updated() {
        let links = vec![
            updated(
                link(1, TrackedSource::Manual, PrState::Open),
                "2026-10-03T00:00:00Z",
            ),
            updated(
                link(2, TrackedSource::Manual, PrState::Open),
                "2026-10-05T00:00:00Z",
            ),
            link(3, TrackedSource::Manual, PrState::Open),
        ];
        assert_eq!(own_link(&links, "o/r").unwrap().key, key(2));
    }

    #[test]
    fn refreshing_updates_tracked_links_and_skips_tombstones() {
        let mut links = vec![link(1, TrackedSource::Manual, PrState::Open), tombstone(2)];
        let fresh = snapshot(PrState::Merged, 9);
        assert!(refresh_snapshot(&mut links, &key(1), &fresh));
        assert!(!refresh_snapshot(&mut links, &key(2), &fresh));
        assert_eq!(links[0].snapshot(), Some(&fresh));
        assert_eq!(links[1].status, LinkStatus::Dismissed);
    }

    #[test]
    fn aggregation_unions_sessions_dedupes_and_prefers_the_freshest_snapshot() {
        let mut stale = link(1, TrackedSource::Stack, PrState::Open);
        let mut fresh = link(1, TrackedSource::Manual, PrState::Merged);
        if let LinkStatus::Tracked { snapshot, .. } = &mut stale.status {
            snapshot.refreshed_at = 1;
        }
        if let LinkStatus::Tracked { snapshot, .. } = &mut fresh.status {
            snapshot.refreshed_at = 2;
        }
        let first = vec![stale, link(2, TrackedSource::Branch, PrState::Open)];
        let second = vec![fresh, link(3, TrackedSource::Created, PrState::Draft)];
        let prs = aggregate([first.as_slice(), second.as_slice()]);
        assert_eq!(
            prs.iter().map(|pr| pr.key.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(prs[0].source, TrackedSource::Manual);
        assert_eq!(prs[0].snapshot.state, PrState::Merged);
    }

    #[test]
    fn a_tombstone_anywhere_in_the_task_hides_stack_only_pull_requests() {
        let first = vec![
            link(1, TrackedSource::Stack, PrState::Open),
            link(2, TrackedSource::Stack, PrState::Open),
        ];
        let second = vec![tombstone(1), link(2, TrackedSource::Manual, PrState::Open)];
        let prs = aggregate([first.as_slice(), second.as_slice()]);
        assert_eq!(
            prs.iter().map(|pr| pr.key.number).collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(known_keys([first.as_slice(), second.as_slice()]).len(), 2);
    }

    #[test]
    fn task_groups_share_task_key_and_project_and_fall_back_to_the_session() {
        let session = |id: &str, project: &str, task: Option<&str>| SessionInfo {
            id: id.into(),
            project_id: Some(project.into()),
            task_key: task.map(str::to_string),
            status: Some("active".into()),
            branch: None,
        };
        let sessions = vec![
            session("c", "p1", Some("T-1")),
            session("a", "p1", Some("T-1")),
            session("b", "p2", Some("T-1")),
            session("d", "p1", None),
            session("e", "p1", None),
        ];
        assert_eq!(task_group(&sessions, "a"), vec!["a", "c"]);
        assert_eq!(task_group(&sessions, "b"), vec!["b"]);
        assert_eq!(task_group(&sessions, "d"), vec!["d"]);
        assert_eq!(task_group(&sessions, "missing"), vec!["missing"]);
        let parsed = parse_sessions(&json!({ "sessions": [
            { "id": "a", "project_id": "p", "task_key": "T-1", "status": "active", "branch": "topic" },
            { "id": "", "project_id": "p" },
            { "id": "b", "task_key": null },
        ] }));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].task_key.as_deref(), Some("T-1"));
        assert_eq!(parsed[1].task_key, None);
    }

    fn aggregated(states: &[PrState]) -> Vec<AggregatedPr> {
        states
            .iter()
            .enumerate()
            .map(|(index, state)| AggregatedPr {
                key: key(index as u64 + 1),
                url: String::new(),
                source: TrackedSource::Manual,
                snapshot: snapshot(*state, 1),
            })
            .collect()
    }

    #[test]
    fn task_phase_is_done_only_when_every_pr_is_merged_or_closed_and_one_merged() {
        use PrState::*;
        assert_eq!(task_phase(&aggregated(&[])), TaskPhase::Inactive);
        assert_eq!(task_phase(&aggregated(&[Draft])), TaskPhase::Inactive);
        assert_eq!(task_phase(&aggregated(&[Closed])), TaskPhase::Inactive);
        assert_eq!(task_phase(&aggregated(&[Open, Draft])), TaskPhase::Open);
        assert_eq!(task_phase(&aggregated(&[Merged, Open])), TaskPhase::Open);
        assert_eq!(task_phase(&aggregated(&[Merged, Draft])), TaskPhase::Open);
        assert_eq!(task_phase(&aggregated(&[Merged, Closed])), TaskPhase::Done);
        assert_eq!(task_phase(&aggregated(&[Merged])), TaskPhase::Done);
    }

    #[test]
    fn task_transitions_fire_once_per_real_phase_change() {
        use TaskPhase::*;
        assert_eq!(
            task_transition(Inactive, Open),
            Some(PrStateTransition::Opened)
        );
        assert_eq!(task_transition(Open, Open), None);
        assert_eq!(task_transition(Open, Done), Some(PrStateTransition::Merged));
        assert_eq!(task_transition(Done, Done), None);
        assert_eq!(task_transition(Inactive, Done), None);
        assert_eq!(task_transition(Done, Open), None);
    }
}
