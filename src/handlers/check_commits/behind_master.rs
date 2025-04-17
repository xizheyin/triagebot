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

    // Check how many commits the PR is behind master
    let behind_by = match event.issue.commits_behind_base(client).await {
        Ok(Some(count)) => count,
        Ok(None) => {
            log::warn!(
                "Unable to determine commits behind base for PR #{}",
                event.issue.number
            );
            return None;
        }
        Err(e) => {
            log::error!(
                "Error checking commits behind master for PR #{}: {}",
                event.issue.number,
                e
            );
            return None;
        }
    };

    // If PR is behind by at least the threshold, generate a warning
    if behind_by >= threshold {
        // Get repository information for the message
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

        log::info!(
            "PR #{} is {} commits behind {} (threshold: {})",
            event.issue.number,
            behind_by,
            repo_info.default_branch,
            threshold
        );

        return Some(format!(
            "This PR is {} commits behind the `{}` branch. \
            It's recommended to update your branch according to the \
            [Rustc Dev Guide](https://rustc-dev-guide.rust-lang.org/contributing.html#keeping-your-branch-up-to-date).",
            behind_by,
            repo_info.default_branch
        ));
    }

    None
}
