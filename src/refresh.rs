//! Plans the batched lookups that refresh tracked pull requests, detect branch pull requests, and discover stacks.

use crate::github_graphql::{FetchedPr, GraphqlResponse, Lookup, LookupRequest};
use crate::pull_requests::{
    known_keys, record_branch_pr, record_stack, refresh_snapshot, LinkStatus, PrKey, PrSnapshot,
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

/// Stack links and their tombstones both count, so dismissing a stack pull request never makes room for another.
fn stack_count(lists: &SessionLinks, group: &[String]) -> usize {
    group_lists(lists, group)
        .into_iter()
        .flatten()
        .filter(|link| {
            matches!(
                link.status,
                LinkStatus::Tracked {
                    source: TrackedSource::Stack,
                    ..
                } | LinkStatus::Dismissed { stack: true }
            )
        })
        .count()
}

/// Which way a tracked pull request extends its task's stack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// The task's own pull requests look for their parent and their children.
    Both,
    /// A discovered parent looks only for its own parent, never for its other children.
    Up,
    /// A discovered child looks only for its own children.
    Down,
}

/// Each tracked pull request of a task that extends its stack, with how it does.
fn stack_reach<'a>(
    lists: &'a SessionLinks,
    group: &[String],
) -> Vec<(&'a str, &'a PullRequestLink, &'a PrSnapshot, Reach)> {
    let entries: Vec<(&str, &PullRequestLink, &PrSnapshot)> = group
        .iter()
        .filter_map(|member| lists.get_key_value(member))
        .flat_map(|(member, links)| {
            links
                .iter()
                .filter_map(move |link| Some((member.as_str(), link, link.snapshot()?)))
        })
        .collect();
    let mut reach: Vec<Option<Reach>> = entries
        .iter()
        .map(|(_, link, _)| (link.source() != Some(TrackedSource::Stack)).then_some(Reach::Both))
        .collect();
    let walk = |reach: &mut [Option<Reach>], direction: Reach, seeds: Vec<usize>| {
        let joints = |snapshot: &'a PrSnapshot| match direction {
            Reach::Up => (&snapshot.base_branch, &snapshot.head_branch),
            _ => (&snapshot.head_branch, &snapshot.base_branch),
        };
        let mut frontier = seeds;
        while let Some(index) = frontier.pop() {
            let (_, from, from_snapshot) = entries[index];
            let Some(joint) = joints(from_snapshot).0 else {
                continue;
            };
            for (next, (_, link, snapshot)) in entries.iter().enumerate() {
                if reach[next].is_none()
                    && link.key.repository == from.key.repository
                    && joints(snapshot).1.as_ref() == Some(joint)
                {
                    reach[next] = Some(direction);
                    frontier.push(next);
                }
            }
        }
    };
    let own: Vec<usize> = (0..entries.len())
        .filter(|index| reach[*index] == Some(Reach::Both))
        .collect();
    walk(&mut reach, Reach::Up, own.clone());
    walk(&mut reach, Reach::Down, own);
    // GitHub retargets a stack onto the base branch when its bottom merges, so a live stack link
    // that no longer connects to the task's own pull requests still grows upward from itself.
    let detached: Vec<usize> = (0..entries.len())
        .filter(|index| reach[*index].is_none() && entries[*index].2.state.is_live())
        .collect();
    for index in &detached {
        reach[*index] = Some(Reach::Down);
    }
    walk(&mut reach, Reach::Down, detached);
    entries
        .into_iter()
        .zip(reach)
        .filter_map(|((member, link, snapshot), reach)| Some((member, link, snapshot, reach?)))
        .collect()
}

/// The stack-neighbor lookups of every task, so its stack grows as one linear chain away from the
/// task's own pull requests; nothing is looked up below a repository's default branch.
fn neighbor_lookups(
    lists: &SessionLinks,
    groups: &[Vec<String>],
    default_branches: &BTreeMap<String, String>,
) -> BTreeMap<LookupRequest, BTreeSet<String>> {
    let mut lookups: BTreeMap<LookupRequest, BTreeSet<String>> = BTreeMap::new();
    let singletons: Vec<Vec<String>> = lists
        .keys()
        .filter(|session| group_of(groups, session).is_empty())
        .map(|session| vec![session.clone()])
        .collect();
    for group in groups.iter().chain(&singletons) {
        for (owner, link, snapshot, reach) in stack_reach(lists, group) {
            if !snapshot.state.is_live() {
                continue;
            }
            let repository = &link.key.repository;
            let is_default = |branch: &String| default_branches.get(repository) == Some(branch);
            let mut add = |lookup: Lookup| {
                lookups
                    .entry(LookupRequest::new(repository, lookup))
                    .or_default()
                    .insert(owner.to_string());
            };
            if reach != Reach::Down {
                if let Some(base) = snapshot
                    .base_branch
                    .as_ref()
                    .filter(|base| !is_default(base))
                {
                    add(Lookup::OpenHead(base.clone()));
                }
            }
            if reach != Reach::Up {
                if let Some(head) = snapshot
                    .head_branch
                    .as_ref()
                    .filter(|head| !is_default(head))
                {
                    add(Lookup::OpenBase(head.clone()));
                }
            }
        }
    }
    lookups
}

/// The one pull request a neighbor lookup adds to a stack: none when the branch is the default
/// branch, or when several open pull requests share it, since then it is no linear chain.
fn stack_neighbor<'a>(
    prs: &'a [FetchedPr],
    branch: &str,
    default_branch: Option<&String>,
) -> Option<&'a FetchedPr> {
    if default_branch.is_some_and(|default| default == branch) {
        return None;
    }
    let mut candidates = prs.iter().filter(|pr| {
        !pr.cross_repository
            && pr.snapshot.state.is_live()
            && pr.snapshot.head_branch.as_ref() != default_branch
    });
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}

/// GitHub's pick for a branch: an open pull request first, else the most recently updated one.
pub fn branch_pull_request<'a>(prs: &'a [FetchedPr], branch: &str) -> Option<&'a FetchedPr> {
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
    let mut default_branches = BTreeMap::new();
    let mut neighbors = neighbor_lookups(&working, plan.groups, &default_branches);
    let mut queried: BTreeSet<LookupRequest> = BTreeSet::new();
    requests.extend(neighbors.keys().cloned());

    for _ in 0..max_rounds {
        if requests.is_empty() {
            break;
        }
        let response = fetch(&requests)?;
        outcome.merge_methods.extend(response.merge_methods);
        default_branches.extend(response.default_branches);
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
                Lookup::OpenHead(branch) | Lookup::OpenBase(branch) => {
                    let owners = neighbors.get(request).cloned().unwrap_or_default();
                    let default_branch = default_branches.get(&request.repository);
                    if let Some(pr) = stack_neighbor(prs, branch, default_branch) {
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
        neighbors = neighbor_lookups(&working, plan.groups, &default_branches);
        neighbors.retain(|request, _| !queried.contains(request));
        requests = neighbors.keys().cloned().collect();
    }
    Ok(outcome)
}

/// Applies an outcome to freshly read lists, so writes made while GitHub answered are kept.
/// Only the sessions in `lists` change: an outcome can cover other tasks, whose stored links
/// these lists never read.
pub fn apply(lists: &mut SessionLinks, groups: &[Vec<String>], outcome: &Outcome) {
    for pr in outcome.fetched.values() {
        for links in lists.values_mut() {
            refresh_snapshot(links, &pr.key, &pr.snapshot);
        }
    }
    for (session_id, detected) in &outcome.branch_prs {
        let Some(links) = lists.get_mut(session_id) else {
            continue;
        };
        let detected = detected.as_ref().and_then(|key| outcome.fetched.get(key));
        record_branch_pr(
            links,
            detected.map(|pr| (&pr.key, pr.url.as_str(), &pr.snapshot)),
        );
    }
    for (owner, key) in &outcome.discovered {
        let Some(pr) = outcome.fetched.get(key) else {
            continue;
        };
        if known_keys(group_lists(lists, group_of(groups, owner))).contains(key) {
            continue;
        }
        if let Some(links) = lists.get_mut(owner) {
            record_stack(links, key, &pr.url, &pr.snapshot);
        }
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
            for request in requests {
                response
                    .default_branches
                    .insert(request.repository.clone(), "main".into());
            }
            Ok(response)
        }
    }

    fn github(prs: Vec<FetchedPr>) -> FakeGithub {
        FakeGithub {
            prs,
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Runs a refresh of one session tracking `links`, then applies it, as a status request would.
    fn refresh_one(
        github: &FakeGithub,
        links: Vec<PullRequestLink>,
        rounds: usize,
    ) -> SessionLinks {
        let lists = SessionLinks::from([("s1".to_string(), links)]);
        let groups = vec![vec!["s1".to_string()]];
        let detail = BTreeSet::new();
        let plan = Plan {
            lists: &lists,
            groups: &groups,
            branch_targets: &[],
            detail_keys: &detail,
        };
        let outcome = run(&plan, rounds, |requests| github.fetch(requests)).unwrap();
        let mut fresh = lists.clone();
        apply(&mut fresh, &groups, &outcome);
        fresh
    }

    fn tracked_numbers(lists: &SessionLinks) -> Vec<u64> {
        let mut numbers: Vec<u64> = lists["s1"].iter().map(|link| link.key.number).collect();
        numbers.sort();
        numbers
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
            status: LinkStatus::Dismissed { stack: true },
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
    fn a_pull_request_from_the_default_branch_ends_the_stack() {
        let release = fetched(1, PrState::Open, "main", "production");
        let feature = fetched(2, PrState::Open, "feature", "main");
        let github = github(vec![
            release.clone(),
            feature.clone(),
            fetched(3, PrState::Open, "unrelated-1", "main"),
            fetched(4, PrState::Open, "unrelated-2", "main"),
        ]);

        let fresh = refresh_one(&github, vec![tracked(&feature, TrackedSource::Branch)], 3);
        assert_eq!(
            tracked_numbers(&fresh),
            vec![2],
            "neither the release pull request nor what else targets main joins the stack"
        );

        let fresh = refresh_one(&github, vec![tracked(&release, TrackedSource::Manual)], 3);
        assert_eq!(
            tracked_numbers(&fresh),
            vec![1],
            "a tracked release pull request does not collect every pull request into main"
        );
    }

    #[test]
    fn stack_discovery_never_reaches_a_parents_other_children() {
        let parent = fetched(1, PrState::Open, "a", "main");
        let mine = fetched(2, PrState::Open, "b", "a");
        let sibling = fetched(3, PrState::Open, "c", "a");
        let github = github(vec![parent, mine.clone(), sibling]);

        let first = refresh_one(&github, vec![tracked(&mine, TrackedSource::Branch)], 3);
        assert_eq!(tracked_numbers(&first), vec![1, 2]);
        let second = refresh_one(&github, first["s1"].clone(), 3);
        assert_eq!(
            tracked_numbers(&second),
            vec![1, 2],
            "a stored parent keeps walking away from the task"
        );
    }

    #[test]
    fn a_restacked_chain_keeps_growing_after_its_bottom_merges() {
        let own = fetched(1, PrState::Merged, "a", "main");
        let retargeted = fetched(2, PrState::Open, "b", "main");
        let above = fetched(3, PrState::Open, "c", "b");
        let github = github(vec![
            own.clone(),
            retargeted.clone(),
            above.clone(),
            fetched(4, PrState::Open, "d", "c"),
        ]);
        let fresh = refresh_one(
            &github,
            vec![
                tracked(&own, TrackedSource::Manual),
                tracked(&retargeted, TrackedSource::Stack),
                tracked(&above, TrackedSource::Stack),
            ],
            3,
        );
        assert_eq!(tracked_numbers(&fresh), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_branch_that_several_pull_requests_target_ends_the_stack() {
        let base = fetched(1, PrState::Open, "a", "main");
        let github = github(vec![
            base.clone(),
            fetched(2, PrState::Open, "b", "a"),
            fetched(3, PrState::Open, "c", "a"),
        ]);
        let fresh = refresh_one(&github, vec![tracked(&base, TrackedSource::Branch)], 3);
        assert_eq!(tracked_numbers(&fresh), vec![1]);
    }

    fn chain(length: u64) -> Vec<FetchedPr> {
        (1..=length)
            .map(|number| {
                let base = if number == 1 {
                    "main".to_string()
                } else {
                    format!("b{}", number - 1)
                };
                fetched(number, PrState::Open, &format!("b{number}"), &base)
            })
            .collect()
    }

    #[test]
    fn stack_discovery_is_bounded_per_task() {
        let github = github(chain(20));
        let fresh = refresh_one(
            &github,
            vec![tracked(&github.prs[0], TrackedSource::Branch)],
            20,
        );
        assert_eq!(fresh["s1"].len(), 1 + MAX_STACK_LINKS);
    }

    #[test]
    fn unlinked_pull_requests_leave_the_bound_to_the_stack() {
        let github = github(chain(20));
        let mut links = vec![tracked(&github.prs[0], TrackedSource::Branch)];
        let unlinked: Vec<serde_json::Value> = (100..100 + MAX_STACK_LINKS as u64)
            .map(|number| serde_json::json!({ "host": "github.com", "repository": "o/r", "number": number, "url": format!("https://github.com/o/r/pull/{number}"), "source": "unlinked" }))
            .collect();
        links.extend(
            crate::pull_requests::parse_session_links(&serde_json::Value::Array(unlinked)).unwrap(),
        );
        let fresh = refresh_one(&github, links, 20);
        assert_eq!(fresh["s1"].len(), 1 + 2 * MAX_STACK_LINKS);
    }

    #[test]
    fn dismissed_stack_pull_requests_count_toward_the_bound() {
        let github = github(chain(20));
        let mut links = vec![tracked(&github.prs[0], TrackedSource::Branch)];
        links.extend(
            (100..100 + MAX_STACK_LINKS as u64).map(|number| PullRequestLink {
                key: key(number),
                url: format!("https://github.com/o/r/pull/{number}"),
                status: LinkStatus::Dismissed { stack: true },
            }),
        );
        let fresh = refresh_one(&github, links.clone(), 20);
        assert!(
            !differs(&links, &fresh["s1"]),
            "dismissing never makes room for more"
        );
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
                ..GraphqlResponse::default()
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
                ..GraphqlResponse::default()
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
    fn apply_only_touches_the_sessions_it_was_given() {
        let theirs = fetched(5, PrState::Open, "theirs", "main");
        let stacked = fetched(6, PrState::Open, "next", "theirs");
        let outcome = Outcome {
            fetched: BTreeMap::from([
                (theirs.key.clone(), theirs.clone()),
                (stacked.key.clone(), stacked.clone()),
            ]),
            branch_prs: BTreeMap::from([("other-task".to_string(), Some(theirs.key.clone()))]),
            discovered: vec![("other-task".to_string(), stacked.key.clone())],
            merge_methods: BTreeMap::new(),
        };
        let groups = vec![vec!["mine".to_string()], vec!["other-task".to_string()]];
        let mut fresh = SessionLinks::from([("mine".to_string(), Vec::new())]);

        apply(&mut fresh, &groups, &outcome);

        assert_eq!(
            fresh.keys().collect::<Vec<_>>(),
            vec!["mine"],
            "another task's links are not rebuilt from its branch alone, which would erase its manual links"
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
