//! Plans the batched lookups that refresh tracked pull requests, detect branch pull requests, and discover stacks.

use crate::github_graphql::{FetchedPr, GraphqlResponse, Lookup, LookupRequest};
use crate::pull_requests::{
    known_keys, record_branch_pr, record_stack, refresh_snapshot, LinkStatus, PrKey,
    PullRequestLink, TrackedSource,
};
use std::collections::{BTreeMap, BTreeSet};

/// Stack discovery stops adding pull requests to a task beyond this many.
pub const MAX_STACK_LINKS: usize = 8;

pub type SessionLinks = BTreeMap<String, Vec<PullRequestLink>>;

#[derive(Clone, Debug)]
pub struct BranchTarget {
    pub session_id: String,
    pub repository: String,
    pub branch: String,
    /// Also fetch checks and mergeability for the panel.
    pub detail: bool,
}

pub struct Plan<'a> {
    pub lists: &'a SessionLinks,
    /// Sessions that share a task, each session in exactly one group.
    pub groups: &'a [Vec<String>],
    pub branch_targets: &'a [BranchTarget],
    /// Pull requests to fetch with checks and mergeability.
    pub detail_keys: &'a BTreeSet<PrKey>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub fetched: BTreeMap<PrKey, FetchedPr>,
    /// The pull request each targeted branch resolves to; absent when GitHub could not answer.
    pub branch_prs: BTreeMap<String, Option<PrKey>>,
    pub discovered: Vec<(String, PrKey)>,
    pub merge_methods: BTreeMap<String, Vec<String>>,
}

fn group_of<'a>(groups: &'a [Vec<String>], session_id: &str) -> &'a [String] {
    groups
        .iter()
        .find(|group| group.iter().any(|member| member == session_id))
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn group_lists<'a>(lists: &'a SessionLinks, group: &[String]) -> Vec<&'a [PullRequestLink]> {
    group
        .iter()
        .filter_map(|member| lists.get(member).map(Vec::as_slice))
        .collect()
}

fn stack_count(lists: &SessionLinks, group: &[String]) -> usize {
    group_lists(lists, group)
        .into_iter()
        .flatten()
        .filter(|link| link.source() == Some(TrackedSource::Stack))
        .count()
}

/// The stack-neighbor lookups of every live pull request whose branches are known.
fn neighbor_lookups(lists: &SessionLinks) -> BTreeMap<LookupRequest, BTreeSet<String>> {
    let mut lookups: BTreeMap<LookupRequest, BTreeSet<String>> = BTreeMap::new();
    for (session_id, links) in lists {
        for link in links {
            let Some(snapshot) = link.snapshot().filter(|snapshot| snapshot.state.is_live()) else {
                continue;
            };
            let repository = &link.key.repository;
            if let Some(base) = &snapshot.base_branch {
                lookups
                    .entry(LookupRequest::new(
                        repository,
                        Lookup::OpenHead(base.clone()),
                    ))
                    .or_default()
                    .insert(session_id.clone());
            }
            if let Some(head) = &snapshot.head_branch {
                lookups
                    .entry(LookupRequest::new(
                        repository,
                        Lookup::OpenBase(head.clone()),
                    ))
                    .or_default()
                    .insert(session_id.clone());
            }
        }
    }
    lookups
}

/// GitHub's pick for a branch: an open pull request first, else the most recently updated one.
fn branch_pull_request<'a>(prs: &'a [FetchedPr], branch: &str) -> Option<&'a FetchedPr> {
    let same_repository = || {
        prs.iter().filter(move |pr| {
            !pr.cross_repository && pr.snapshot.head_branch.as_deref() == Some(branch)
        })
    };
    same_repository()
        .find(|pr| pr.snapshot.state.is_live())
        .or_else(|| same_repository().next())
}

/// Fetches in rounds: tracked pull requests, branch pull requests and known stack neighbors first,
/// then the neighbors of whatever that round discovered.
pub fn run<E>(
    plan: &Plan,
    max_rounds: usize,
    mut fetch: impl FnMut(&[LookupRequest]) -> Result<GraphqlResponse, E>,
) -> Result<Outcome, E> {
    let mut working = plan.lists.clone();
    let mut outcome = Outcome::default();
    let tracked: BTreeSet<PrKey> = working
        .values()
        .flatten()
        .filter(|link| link.snapshot().is_some())
        .map(|link| link.key.clone())
        .collect();
    let mut requests: Vec<LookupRequest> = tracked
        .iter()
        .chain(plan.detail_keys.iter().filter(|key| !tracked.contains(key)))
        .map(|key| {
            LookupRequest::new(
                &key.repository,
                Lookup::Number {
                    number: key.number,
                    detail: plan.detail_keys.contains(key),
                },
            )
        })
        .collect();
    for target in plan.branch_targets {
        let request = LookupRequest::new(
            &target.repository,
            Lookup::Head {
                branch: target.branch.clone(),
                detail: target.detail,
            },
        );
        if !requests.contains(&request) {
            requests.push(request);
        }
    }
    let mut neighbors = neighbor_lookups(&working);
    let mut queried: BTreeSet<LookupRequest> = BTreeSet::new();
    requests.extend(neighbors.keys().cloned());

    for _ in 0..max_rounds {
        if requests.is_empty() {
            break;
        }
        let response = fetch(&requests)?;
        outcome.merge_methods.extend(response.merge_methods);
        for request in &requests {
            queried.insert(request.clone());
            let Some(Some(prs)) = response.lookups.get(request) else {
                continue;
            };
            match &request.lookup {
                Lookup::Number { .. } => {
                    for pr in prs {
                        for links in working.values_mut() {
                            refresh_snapshot(links, &pr.key, &pr.snapshot);
                        }
                        outcome.fetched.insert(pr.key.clone(), pr.clone());
                    }
                }
                Lookup::Head { branch, .. } => {
                    let detected = branch_pull_request(prs, branch);
                    for target in plan.branch_targets.iter().filter(|target| {
                        target.branch == *branch
                            && target.repository.eq_ignore_ascii_case(&request.repository)
                    }) {
                        if let Some(pr) = detected {
                            outcome.fetched.insert(pr.key.clone(), pr.clone());
                        }
                        let links = working.entry(target.session_id.clone()).or_default();
                        record_branch_pr(
                            links,
                            detected.map(|pr| (&pr.key, pr.url.as_str(), &pr.snapshot)),
                        );
                        outcome
                            .branch_prs
                            .insert(target.session_id.clone(), detected.map(|pr| pr.key.clone()));
                    }
                }
                Lookup::OpenHead(_) | Lookup::OpenBase(_) => {
                    let owners = neighbors.get(request).cloned().unwrap_or_default();
                    for pr in prs
                        .iter()
                        .filter(|pr| !pr.cross_repository && pr.snapshot.state.is_live())
                    {
                        for owner in &owners {
                            let group = group_of(plan.groups, owner);
                            if known_keys(group_lists(&working, group)).contains(&pr.key)
                                || stack_count(&working, group) >= MAX_STACK_LINKS
                            {
                                continue;
                            }
                            let links = working.entry(owner.clone()).or_default();
                            record_stack(links, &pr.key, &pr.url, &pr.snapshot);
                            outcome.fetched.insert(pr.key.clone(), pr.clone());
                            outcome.discovered.push((owner.clone(), pr.key.clone()));
                        }
                    }
                }
            }
        }
        neighbors = neighbor_lookups(&working);
        neighbors.retain(|request, _| !queried.contains(request));
        requests = neighbors.keys().cloned().collect();
    }
    Ok(outcome)
}

/// Applies an outcome to freshly read lists, so writes made while GitHub answered are kept.
pub fn apply(lists: &mut SessionLinks, groups: &[Vec<String>], outcome: &Outcome) {
    for pr in outcome.fetched.values() {
        for links in lists.values_mut() {
            refresh_snapshot(links, &pr.key, &pr.snapshot);
        }
    }
    for (session_id, detected) in &outcome.branch_prs {
        let detected = detected.as_ref().and_then(|key| outcome.fetched.get(key));
        record_branch_pr(
            lists.entry(session_id.clone()).or_default(),
            detected.map(|pr| (&pr.key, pr.url.as_str(), &pr.snapshot)),
        );
    }
    for (owner, key) in &outcome.discovered {
        let Some(pr) = outcome.fetched.get(key) else {
            continue;
        };
        let group = group_of(groups, owner);
        if known_keys(group_lists(lists, group)).contains(key) {
            continue;
        }
        record_stack(
            lists.entry(owner.clone()).or_default(),
            key,
            &pr.url,
            &pr.snapshot,
        );
    }
}

/// Whether two versions of a session's links differ in anything but when they were refreshed.
pub fn differs(before: &[PullRequestLink], after: &[PullRequestLink]) -> bool {
    let normalized = |links: &[PullRequestLink]| {
        links
            .iter()
            .map(|link| {
                let mut link = link.clone();
                if let LinkStatus::Tracked { snapshot, .. } = &mut link.status {
                    snapshot.refreshed_at = 0;
                }
                link
            })
            .collect::<Vec<_>>()
    };
    normalized(before) != normalized(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_requests::{PrSnapshot, PrState, GITHUB_HOST};
    use std::cell::RefCell;

    fn key(number: u64) -> PrKey {
        PrKey::new(GITHUB_HOST, "o/r", number).unwrap()
    }

    fn fetched(number: u64, state: PrState, head: &str, base: &str) -> FetchedPr {
        FetchedPr {
            key: key(number),
            url: format!("https://github.com/o/r/pull/{number}"),
            snapshot: PrSnapshot {
                state,
                title: Some(format!("PR {number}")),
                checks: None,
                head_branch: Some(head.into()),
                base_branch: Some(base.into()),
                updated_at: Some(format!("2026-10-0{}T00:00:00Z", number % 10)),
                refreshed_at: 100,
            },
            cross_repository: false,
            detail: None,
        }
    }

    fn tracked(pr: &FetchedPr, source: TrackedSource) -> PullRequestLink {
        let mut snapshot = pr.snapshot.clone();
        snapshot.refreshed_at = 1;
        PullRequestLink::tracked(pr.key.clone(), &pr.url, source, snapshot)
    }

    /// A fake GitHub holding a fixed set of pull requests, answering lookups like the GraphQL API.
    struct FakeGithub {
        prs: Vec<FetchedPr>,
        calls: RefCell<Vec<Vec<LookupRequest>>>,
    }

    impl FakeGithub {
        fn fetch(&self, requests: &[LookupRequest]) -> Result<GraphqlResponse, String> {
            self.calls.borrow_mut().push(requests.to_vec());
            let mut response = GraphqlResponse::default();
            for request in requests {
                let matches = |pr: &&FetchedPr| match &request.lookup {
                    Lookup::Number { number, .. } => pr.key.number == *number,
                    Lookup::Head { branch, .. } => pr.snapshot.head_branch.as_ref() == Some(branch),
                    Lookup::OpenHead(branch) => {
                        pr.snapshot.state.is_live()
                            && pr.snapshot.head_branch.as_ref() == Some(branch)
                    }
                    Lookup::OpenBase(branch) => {
                        pr.snapshot.state.is_live()
                            && pr.snapshot.base_branch.as_ref() == Some(branch)
                    }
                };
                response.lookups.insert(
                    request.clone(),
                    Some(self.prs.iter().filter(matches).cloned().collect()),
                );
            }
            Ok(response)
        }
    }

    fn stack_github() -> FakeGithub {
        FakeGithub {
            prs: vec![
                fetched(1, PrState::Open, "a", "main"),
                fetched(2, PrState::Open, "b", "a"),
                fetched(3, PrState::Draft, "c", "b"),
                fetched(4, PrState::Merged, "old", "a"),
                fetched(9, PrState::Open, "unrelated", "main"),
            ],
            calls: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn discovers_a_stack_in_both_directions_from_the_branch_pull_request() {
        let github = stack_github();
        let lists = SessionLinks::from([("s1".to_string(), Vec::new())]);
        let groups = vec![vec!["s1".to_string()]];
        let targets = vec![BranchTarget {
            session_id: "s1".into(),
            repository: "o/r".into(),
            branch: "b".into(),
            detail: false,
        }];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &targets,
            detail_keys: &detail,
        };

        let outcome = run(&plan, 3, |requests| github.fetch(requests)).unwrap();
        assert_eq!(outcome.branch_prs["s1"], Some(key(2)));
        let mut discovered: Vec<u64> = outcome
            .discovered
            .iter()
            .map(|(_, key)| key.number)
            .collect();
        discovered.sort();
        assert_eq!(
            discovered,
            vec![1, 3],
            "merged and unrelated pull requests are not part of the stack"
        );
        assert_eq!(
            github.calls.borrow().len(),
            3,
            "the branch, its neighbors, then theirs"
        );

        let mut fresh = lists.clone();
        apply(&mut fresh, &groups, &outcome);
        let sources: Vec<_> = fresh["s1"]
            .iter()
            .map(|link| (link.key.number, link.source().unwrap()))
            .collect();
        assert_eq!(
            sources,
            vec![
                (2, TrackedSource::Branch),
                (1, TrackedSource::Stack),
                (3, TrackedSource::Stack)
            ]
        );
    }

    #[test]
    fn known_neighbors_are_found_in_the_first_round_and_rounds_are_bounded() {
        let github = stack_github();
        let lists = SessionLinks::from([(
            "s1".to_string(),
            vec![tracked(&github.prs[1], TrackedSource::Manual)],
        )]);
        let groups = vec![vec!["s1".to_string()]];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &[],
            detail_keys: &detail,
        };

        let outcome = run(&plan, 1, |requests| github.fetch(requests)).unwrap();
        assert_eq!(github.calls.borrow().len(), 1);
        let first = &github.calls.borrow()[0];
        assert!(first.contains(&LookupRequest::new(
            "o/r",
            Lookup::Number {
                number: 2,
                detail: false
            }
        )));
        assert!(first.contains(&LookupRequest::new("o/r", Lookup::OpenHead("a".into()))));
        assert!(first.contains(&LookupRequest::new("o/r", Lookup::OpenBase("b".into()))));
        assert_eq!(outcome.discovered.len(), 2);
    }

    #[test]
    fn tombstones_anywhere_in_the_task_block_rediscovery() {
        let github = stack_github();
        let tombstone = PullRequestLink {
            key: key(1),
            url: "https://github.com/o/r/pull/1".into(),
            status: LinkStatus::Dismissed,
        };
        let lists = SessionLinks::from([
            (
                "s1".to_string(),
                vec![tracked(&github.prs[1], TrackedSource::Branch)],
            ),
            ("s2".to_string(), vec![tombstone]),
        ]);
        let groups = vec![vec!["s1".to_string(), "s2".to_string()]];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &[],
            detail_keys: &detail,
        };
        let outcome = run(&plan, 3, |requests| github.fetch(requests)).unwrap();
        assert_eq!(
            outcome
                .discovered
                .iter()
                .map(|(_, key)| key.number)
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn stack_discovery_is_bounded_per_task() {
        let mut prs = vec![fetched(1, PrState::Open, "a", "main")];
        for number in 2..30 {
            prs.push(fetched(
                number,
                PrState::Open,
                &format!("child-{number}"),
                "a",
            ));
        }
        let github = FakeGithub {
            prs,
            calls: RefCell::new(Vec::new()),
        };
        let lists = SessionLinks::from([(
            "s1".to_string(),
            vec![tracked(&github.prs[0], TrackedSource::Branch)],
        )]);
        let groups = vec![vec!["s1".to_string()]];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &[],
            detail_keys: &detail,
        };
        let outcome = run(&plan, 3, |requests| github.fetch(requests)).unwrap();
        assert_eq!(outcome.discovered.len(), MAX_STACK_LINKS);
    }

    #[test]
    fn unavailable_lookups_keep_cached_state_and_an_empty_branch_drops_its_link() {
        let pr = fetched(5, PrState::Open, "topic", "main");
        let lists =
            SessionLinks::from([("s1".to_string(), vec![tracked(&pr, TrackedSource::Branch)])]);
        let groups = vec![vec!["s1".to_string()]];
        let targets = vec![BranchTarget {
            session_id: "s1".into(),
            repository: "o/r".into(),
            branch: "topic".into(),
            detail: false,
        }];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &targets,
            detail_keys: &detail,
        };

        let unavailable = run(&plan, 3, |requests| {
            Ok::<_, String>(GraphqlResponse {
                lookups: requests
                    .iter()
                    .map(|request| (request.clone(), None))
                    .collect(),
                merge_methods: BTreeMap::new(),
            })
        })
        .unwrap();
        let mut fresh = lists.clone();
        apply(&mut fresh, &groups, &unavailable);
        assert!(!differs(&lists["s1"], &fresh["s1"]));

        let empty = run(&plan, 3, |requests| {
            Ok::<_, String>(GraphqlResponse {
                lookups: requests
                    .iter()
                    .map(|request| (request.clone(), Some(Vec::new())))
                    .collect(),
                merge_methods: BTreeMap::new(),
            })
        })
        .unwrap();
        apply(&mut fresh, &groups, &empty);
        assert!(fresh["s1"].is_empty());
    }

    #[test]
    fn apply_keeps_links_written_while_github_answered_and_refreshes_every_copy() {
        let github = stack_github();
        let lists = SessionLinks::from([(
            "s1".to_string(),
            vec![tracked(&github.prs[0], TrackedSource::Manual)],
        )]);
        let groups = vec![vec!["s1".to_string(), "s2".to_string()]];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &[],
            detail_keys: &detail,
        };
        let outcome = run(&plan, 1, |requests| github.fetch(requests)).unwrap();

        let mut fresh = SessionLinks::from([
            (
                "s1".to_string(),
                vec![tracked(&github.prs[0], TrackedSource::Manual)],
            ),
            (
                "s2".to_string(),
                vec![
                    tracked(&github.prs[1], TrackedSource::Manual),
                    tracked(&github.prs[0], TrackedSource::Manual),
                ],
            ),
        ]);
        apply(&mut fresh, &groups, &outcome);
        assert_eq!(
            fresh["s2"].len(),
            2,
            "a PR linked meanwhile is not rediscovered as a stack"
        );
        assert!(fresh
            .values()
            .flatten()
            .all(|link| link.snapshot().unwrap().refreshed_at == 100));
        assert!(
            !differs(&lists["s1"], &fresh["s1"]),
            "a refresh that changes nothing is not a change"
        );
    }

    #[test]
    fn branch_detection_prefers_an_open_same_repository_pull_request() {
        let mut fork = fetched(7, PrState::Open, "topic", "main");
        fork.cross_repository = true;
        let closed = fetched(6, PrState::Closed, "topic", "main");
        let open = fetched(5, PrState::Open, "topic", "main");
        assert_eq!(
            branch_pull_request(&[fork.clone(), closed.clone(), open.clone()], "topic")
                .unwrap()
                .key,
            key(5)
        );
        assert_eq!(
            branch_pull_request(&[fork.clone(), closed], "topic")
                .unwrap()
                .key,
            key(6)
        );
        assert!(branch_pull_request(&[fork], "topic").is_none());
    }
}
