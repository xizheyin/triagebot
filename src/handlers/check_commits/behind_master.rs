use crate::github::{GithubClient, IssuesEvent};
use tracing as log;

/// Default threshold for the number of commits behind master to trigger a warning
pub const DEFAULT_BEHIND_THRESHOLD: u32 = 100;

/// Check if the PR is behind the main branch by a significant number of commits
pub async fn behind_master(
    threshold: u32,
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
    let total_behind_by = comparison.behind_by;

    // If we're not behind by much, no need to filter commits to make the check faster
    if total_behind_by < threshold {
        return None;
    }

    log::debug!(
        "PR #{} is {} commits behind {}. Filtering auto-merge and rollup-merge commits...",
        event.issue.number,
        total_behind_by,
        repo_info.default_branch
    );

    // Filter out auto-merge and rollup commits
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

    let excluded_count = auto_merge_commits.len() + rollup_merge_commits.len();

    // Calculate actual commits behind (total - excluded)
    let filtered_behind_count = total_behind_by - excluded_count as u32;

    log::info!(
        "PR #{} is {} commits behind {} (excluding {} auto-merge and {} rollup commits)",
        event.issue.number,
        filtered_behind_count,
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

    // If PR is behind by at least the threshold after filtering, generate a warning
    if filtered_behind_count >= threshold {
        log::info!(
            "PR #{} is {} commits behind {} (threshold: {})",
            event.issue.number,
            filtered_behind_count,
            repo_info.default_branch,
            threshold
        );

        return Some(format!(
            "This PR is {} commits behind the `{}` branch (excluding {} auto-merge and {} rollup commits). \
It's recommended to update your branch according to the \
[Rustc Dev Guide](https://rustc-dev-guide.rust-lang.org/contributing.html#keeping-your-branch-up-to-date).",
            filtered_behind_count,
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
