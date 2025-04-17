use crate::{
    config::PRBehindCommitsConfig,
    db::issue_data::IssueData,
    github::{Event, IssuesAction, ReportedContentClassifiers},
    handlers::Context,
};
use anyhow::Context as _;
use tracing as log;

/// Key for storing the state in the database
const BRANCH_BEHIND_STATUS_KEY: &str = "branch-behind-status-warnings";

/// Default threshold for the number of commits behind master to trigger a warning
const DEFAULT_BEHIND_THRESHOLD: u32 = 100;

/// State stored in the database for a PR
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct BranchBehindStatusState {
    /// The GraphQL ID of the most recent warning comment.
    last_warned_comment: Option<String>,
    /// The last measured number of commits behind master
    last_behind_count: Option<u32>,
}

pub(super) async fn handle(
    ctx: &Context,
    event: &Event,
    config: &PRBehindCommitsConfig,
) -> anyhow::Result<()> {
    let Event::Issue(event) = event else {
        return Ok(());
    };

    if !matches!(
        event.action,
        IssuesAction::Opened | IssuesAction::Synchronize
    ) || !event.issue.is_pr()
    {
        return Ok(());
    }

    let threshold = config.threshold.unwrap_or(DEFAULT_BEHIND_THRESHOLD);
    
    log::debug!("Checking branch status for PR #{}", event.issue.number);
    
    // Check how many commits the PR is behind master using the GitHub API
    let behind_by = match event.issue.commits_behind_base(&ctx.github).await? {
        Some(count) => count,
        None => {
            log::warn!("Unable to determine commits behind base for PR #{}", event.issue.number);
            return Ok(());
        }
    };
    
    // Get repository information for the message
    let repo_info = ctx.github.repository(&event.issue.repository().full_repo_name()).await?;
    
    // Get the state from the database
    let mut db = ctx.db.get().await;
    let mut state: IssueData<'_, BranchBehindStatusState> =
        IssueData::load(&mut db, &event.issue, BRANCH_BEHIND_STATUS_KEY).await?;
    
    if behind_by >= threshold {
        // Check if we've already warned with the same count, to avoid spamming
        if state.data.last_behind_count != Some(behind_by) || state.data.last_warned_comment.is_none() {
            // Hide previous warning if it exists
            if let Some(last_warned_comment_id) = &state.data.last_warned_comment {
                event
                    .issue
                    .hide_comment(
                        &ctx.github,
                        last_warned_comment_id,
                        ReportedContentClassifiers::Outdated,
                    )
                    .await
                    .context("Failed to hide previous warning comment")?;
                state.data.last_warned_comment = None;
            }
            
            // Create the warning message
            let warning = format!(
                ":warning: **Warning** :warning:\n\n\
                 This PR is {} commits behind the `{}` branch. \
It's recommended to update your branch according to the \
[Rustc Dev Guide](https://rustc-dev-guide.rust-lang.org/contributing.html#keeping-your-branch-up-to-date).\n\n\
                 ",
                behind_by,
                repo_info.default_branch
            );
            
            // Post the warning
            let comment = event.issue.post_comment(&ctx.github, &warning).await
                .context("Failed to post warning comment")?;
            
            // Update state
            state.data.last_warned_comment = Some(comment.node_id);
            state.data.last_behind_count = Some(behind_by);
            state.save().await?;
            
            log::info!("Posted warning for PR #{}: {} commits behind {}", 
                      event.issue.number, 
                      behind_by, 
                      repo_info.default_branch);
        }
    } else if let Some(last_warned_comment_id) = &state.data.last_warned_comment {
        // PR is not behind much anymore, hide the previous warning
        event
            .issue
            .hide_comment(
                &ctx.github,
                last_warned_comment_id,
                ReportedContentClassifiers::Resolved,
            )
            .await
            .context("Failed to hide previous warning comment")?;
        
        // Update state
        state.data.last_warned_comment = None;
        state.data.last_behind_count = None;
        state.save().await?;
        
        log::info!("Removed warning for PR #{} as it's only {} commits behind {}", 
                  event.issue.number, 
                  behind_by, 
                  repo_info.default_branch);
    }
    
    Ok(())
} 