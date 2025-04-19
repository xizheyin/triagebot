use crate::github::{GithubClient, IssuesEvent};
use tracing as log;

/// Default threshold for the number of commits behind master to trigger a warning
pub const DEFAULT_COMMITS_BEHIND_THRESHOLD: usize = 100;

/// Default threshold for parent commit age in days to trigger a warning
pub const DEFAULT_PARENT_AGE_THRESHOLD: usize = 14;

/// Check if the PR is behind the main branch by a significant number of commits
/// or based on an old parent commit
pub async fn behind_master(
    age_threshold: usize,
    merge_commits_threshold: usize,
    event: &IssuesEvent,
    client: &GithubClient,
) -> Option<String> {
    if !event.issue.is_pr() {
        return None;
    }

    log::debug!("Checking if PR #{} is behind master", event.issue.number);

    // Get the repository info to determine default branch
    let repo_info = match client
        .repository(&event.issue.repository().full_repo_name())
        .await
    {
        Ok(repo) => repo,
        Err(e) => {
            log::error!(
                "Error getting repository info for PR #{}: {}",
                event.issue.number,
                e
            );
            return None;
        }
    };

    // First try the parent commit age check as it's more accurate
    match event
        .issue
        .is_parent_commit_too_old(client, age_threshold)
        .await
    {
        Ok(Some(days_old)) => {
            log::info!(
                "PR #{} has a parent commit that is {} days old",
                event.issue.number,
                days_old
            );

            return Some(format!(
                "This PR is based on a commit that is {} days old. \
It's recommended to update your branch according to the \
[Rustc Dev Guide](https://rustc-dev-guide.rust-lang.org/contributing.html#keeping-your-branch-up-to-date).",
                days_old
            ));
        }
        Ok(None) => {
            // Parent commit is not too old, continue with the commit count check
            log::debug!(
                "PR #{} parent commit is not too old, checking commit count",
                event.issue.number
            );
        }
        Err(e) => {
            // Error checking parent commit age, log and fall back to commit count
            log::error!(
                "Error checking parent commit age for PR #{}: {}",
                event.issue.number,
                e
            );
        }
    }

    // Fall back to the commit count method
    // check only auto-merge and rollup-merge commits
    let comparison = match event.issue.branch_comparison(client).await {
        Ok(comparison) => comparison,
        Err(e) => {
            log::error!(
                "Error getting branch comparison for PR #{}: {}",
                event.issue.number,
                e
            );
            return None;
        }
    };

    // Total commits behind master
    let total_behind_by = comparison.behind_by as usize;

    // First check if we're not totaly behind by much, no need to filter commits to make the check faster
    if total_behind_by < merge_commits_threshold {
        return None;
    }

    log::debug!(
        "PR #{} is {} commits behind {}. Counting auto-merge and rollup-merge commits...",
        event.issue.number,
        total_behind_by,
        repo_info.default_branch
    );

    // Count only auto-merge and rollup commits
    let mut auto_merge_commits = Vec::new();
    let mut rollup_merge_commits = Vec::new();

    for commit in &comparison.commits {
        let message = &commit.commit.message;
        if message.starts_with("Auto merge of #") {
            auto_merge_commits.push(commit);
        } else if message.starts_with("Rollup merge of #") {
            rollup_merge_commits.push(commit);
        }
    }

    let merge_commits_count = auto_merge_commits.len() + rollup_merge_commits.len();

    log::info!(
        "PR #{} is {} commits behind {} (only including {} auto-merge and {} rollup commits)",
        event.issue.number,
        merge_commits_count,
        repo_info.default_branch,
        auto_merge_commits.len(),
        rollup_merge_commits.len()
    );

    // Log detailed information about the auto-merge and rollup commits
    for commit in &auto_merge_commits {
        log::trace!(
            "Auto-merge commit: {} - {}",
            commit.sha,
            first_line(&commit.commit.message)
        );
    }

    for commit in &rollup_merge_commits {
        log::trace!(
            "Rollup commit: {} - {}",
            commit.sha,
            first_line(&commit.commit.message)
        );
    }

    // If there are at least the threshold of merge commits, generate a warning
    if merge_commits_count >= merge_commits_threshold as usize {
        log::info!(
            "PR #{} has {} merge commits ({} auto-merge and {} rollup) behind {} (threshold: {})",
            event.issue.number,
            merge_commits_count,
            auto_merge_commits.len(),
            rollup_merge_commits.len(),
            repo_info.default_branch,
            merge_commits_threshold
        );

        return Some(format!(
            "This PR is missing {} important merge commits from the `{}` branch ({} auto-merge and {} rollup commits). \
It's recommended to update your branch according to the \
[Rustc Dev Guide](https://rustc-dev-guide.rust-lang.org/contributing.html#keeping-your-branch-up-to-date).",
            merge_commits_count,
            repo_info.default_branch,
            auto_merge_commits.len(),
            rollup_merge_commits.len()
        ));
    }

    None
}

// Helper function to get the first line of a commit message
fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("")
}
