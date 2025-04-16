//! Handles PR and issue assignment.
//!
//! This supports several ways for setting issue/PR assignment:
//!
//! * `@rustbot assign @gh-user`: Assigns to the given user.
//! * `@rustbot claim`: Assigns to the comment author.
//! * `@rustbot release-assignment`: Removes the commenter's assignment.
//! * `r? @user`: Assigns to the given user (PRs only).
//!
//! Note: this module does not handle review assignments issued from the
//! GitHub "Assignees" dropdown menu
//!
//! This is capable of assigning to any user, even if they do not have write
//! access to the repo. It does this by fake-assigning the bot and adding a
//! "claimed by" section to the top-level comment.
//!
//! Configuration is done with the `[assign]` table.
//!
//! This also supports auto-assignment of new PRs. Based on rules in the
//! `assign.owners` config, it will auto-select an assignee based on the files
//! the PR modifies.

use crate::{
    config::AssignConfig,
    github::{self, Event, FileDiff, Issue, IssuesAction, Selection},
    handlers::{Context, GithubClient, IssuesEvent},
    interactions::EditIssueBody,
};
use anyhow::{bail, Context as _};
use parser::command::assign::AssignCommand;
use parser::command::{Command, Input};
use rand::seq::IteratorRandom;
use rust_team_data::v1::Teams;
use std::collections::{HashMap, HashSet};
use std::fmt;
use tokio_postgres::Client as DbClient;
use tracing as log;

#[cfg(test)]
mod tests {
    mod tests_candidates;
    mod tests_from_diff;
}

const NEW_USER_WELCOME_MESSAGE: &str = "Thanks for the pull request, and welcome! \
The Rust team is excited to review your changes, and you should hear from {who} \
some time within the next two weeks.";

const CONTRIBUTION_MESSAGE: &str = "Please see [the contribution \
instructions]({contributing_url}) for more information. Namely, in order to ensure the \
minimum review times lag, PR authors and assigned reviewers should ensure that the review \
label (`S-waiting-on-review` and `S-waiting-on-author`) stays updated, invoking these commands \
when appropriate:

- `@{bot} author`: the review is finished, PR author should check the comments and take action accordingly
- `@{bot} review`: the author is ready for a review, this PR will be queued again in the reviewer's queue";

const WELCOME_WITH_REVIEWER: &str = "@{assignee} (or someone else)";

const WELCOME_WITHOUT_REVIEWER: &str = "@Mark-Simulacrum (NB. this repo may be misconfigured)";

const RETURNING_USER_WELCOME_MESSAGE: &str = "r? @{assignee}

{bot} has assigned @{assignee}.
They will have a look at your PR within the next two weeks and either review your PR or \
reassign to another reviewer.

Use `r?` to explicitly pick a reviewer";

const RETURNING_USER_WELCOME_MESSAGE_NO_REVIEWER: &str =
    "@{author}: no appropriate reviewer found, use `r?` to override";

fn on_vacation_warning(username: &str) -> String {
    format!(
        r"{username} is on vacation.

Please choose another assignee."
    )
}

pub const SELF_ASSIGN_HAS_NO_CAPACITY: &str = "
You have insufficient capacity to be assigned the pull request at this time. PR assignment has been reverted.

Please choose another assignee or increase your assignment limit.

(see [documentation](https://forge.rust-lang.org/triagebot/pr-assignment-tracking.html))";

pub const REVIEWER_HAS_NO_CAPACITY: &str = "
`{username}` has insufficient capacity to be assigned the pull request at this time. PR assignment has been reverted.

Please choose another assignee.

(see [documentation](https://forge.rust-lang.org/triagebot/pr-assignment-tracking.html))";

const NO_REVIEWER_HAS_CAPACITY: &str = "
Could not find a reviewer with enough capacity to be assigned at this time. This is a problem.

Please contact us on [#t-infra](https://rust-lang.zulipchat.com/#narrow/stream/242791-t-infra) on Zulip.

cc: @jackh726 @apiraino";

const REVIEWER_IS_PR_AUTHOR: &str = "Pull request author cannot be assigned as reviewer.

Please choose another assignee.";

const REVIEWER_ALREADY_ASSIGNED: &str =
    "Requested reviewer is already assigned to this pull request.

Please choose another assignee.";

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AssignData {
    user: Option<String>,
}

/// Input for auto-assignment when a PR is created.
pub(super) struct AssignInput {}

/// Prepares the input when a new PR is opened.
pub(super) async fn parse_input(
    _ctx: &Context,
    event: &IssuesEvent,
    config: Option<&AssignConfig>,
) -> Result<Option<AssignInput>, String> {
    let config = match config {
        Some(config) => config,
        None => return Ok(None),
    };
    if config.owners.is_empty()
        || !matches!(event.action, IssuesAction::Opened)
        || !event.issue.is_pr()
    {
        return Ok(None);
    }
    Ok(Some(AssignInput {}))
}

/// Handles the work of setting an assignment for a new PR and posting a
/// welcome message.
pub(super) async fn handle_input(
    ctx: &Context,
    config: &AssignConfig,
    event: &IssuesEvent,
    _input: AssignInput,
) -> anyhow::Result<()> {
    let Some(diff) = event.issue.diff(&ctx.github).await? else {
        bail!(
            "expected issue {} to be a PR, but the diff could not be determined",
            event.issue.number
        )
    };

    // Don't auto-assign or welcome if the user manually set the assignee when opening.
    if event.issue.assignees.is_empty() {
        let (assignee, from_comment) = determine_assignee(ctx, event, config, &diff).await?;
        if assignee.as_deref() == Some("ghost") {
            // "ghost" is GitHub's placeholder account for deleted accounts.
            // It is used here as a convenient way to prevent assignment. This
            // is typically used for rollups or experiments where you don't
            // want any assignments or noise.
            return Ok(());
        }
        // This is temporarily disabled until we come up with a better
        // solution, or decide to remove this. The `is_new_contributor` query
        // is too expensive and takes too long to process.
        let welcome = if false
            && ctx
                .github
                .is_new_contributor(&event.repository, &event.issue.user.login)
                .await
        {
            let who_text = match &assignee {
                Some(assignee) => WELCOME_WITH_REVIEWER.replace("{assignee}", assignee),
                None => WELCOME_WITHOUT_REVIEWER.to_string(),
            };
            let mut welcome = NEW_USER_WELCOME_MESSAGE.replace("{who}", &who_text);
            if let Some(contrib) = &config.contributing_url {
                welcome.push_str("\n\n");
                welcome.push_str(
                    &CONTRIBUTION_MESSAGE
                        .replace("{contributing_url}", contrib)
                        .replace("{bot}", &ctx.username),
                );
            }
            Some(welcome)
        } else if !from_comment {
            let welcome = match &assignee {
                Some(assignee) => RETURNING_USER_WELCOME_MESSAGE
                    .replace("{assignee}", assignee)
                    .replace("{bot}", &ctx.username),
                None => RETURNING_USER_WELCOME_MESSAGE_NO_REVIEWER
                    .replace("{author}", &event.issue.user.login),
            };
            Some(welcome)
        } else {
            // No welcome is posted if they are not new and they used `r?` in the opening body.
            None
        };
        if let Some(assignee) = assignee {
            set_assignee(&event.issue, &ctx.github, &assignee).await;
        }

        if let Some(welcome) = welcome {
            if let Err(e) = event.issue.post_comment(&ctx.github, &welcome).await {
                log::warn!(
                    "failed to post welcome comment to {}: {e}",
                    event.issue.global_id()
                );
            }
        }
    }

    Ok(())
}

/// Finds the `r?` command in the PR body.
///
/// Returns the name after the `r?` command, or None if not found.
fn find_assign_command(ctx: &Context, event: &IssuesEvent) -> Option<String> {
    let mut input = Input::new(&event.issue.body, vec![&ctx.username]);
    input.find_map(|command| match command {
        Command::Assign(Ok(AssignCommand::RequestReview { name })) => Some(name),
        _ => None,
    })
}

fn is_self_assign(assignee: &str, pr_author: &str) -> bool {
    assignee.to_lowercase() == pr_author.to_lowercase()
}

/// Sets the assignee of a PR, alerting any errors.
async fn set_assignee(issue: &Issue, github: &GithubClient, username: &str) {
    // Don't re-assign if already assigned, e.g. on comment edit
    if issue.contain_assignee(&username) {
        log::trace!(
            "ignoring assign PR {} to {}, already assigned",
            issue.global_id(),
            username,
        );
        return;
    }
    if let Err(err) = issue.set_assignee(github, &username).await {
        log::warn!(
            "failed to set assignee of PR {} to {}: {:?}",
            issue.global_id(),
            username,
            err
        );
        if let Err(e) = issue
            .post_comment(
                github,
                &format!(
                    "Failed to set assignee to `{username}`: {err}\n\
                     \n\
                     > **Note**: Only org members with at least the repository \"read\" role, \
                       users with write permissions, or people who have commented on the PR may \
                       be assigned."
                ),
            )
            .await
        {
            log::warn!("failed to post error comment: {e}");
        }
    }
}

/// Determines who to assign the PR to based on either an `r?` command, or
/// based on which files were modified.
///
/// Will also check if candidates have capacity in their work queue.
///
/// Returns `(assignee, from_comment)` where `assignee` is who to assign to
/// (or None if no assignee could be found). `from_comment` is a boolean
/// indicating if the assignee came from an `r?` command (it is false if
/// determined from the diff).
async fn determine_assignee(
    ctx: &Context,
    event: &IssuesEvent,
    config: &AssignConfig,
    diff: &[FileDiff],
) -> anyhow::Result<(Option<String>, bool)> {
    let db_client = ctx.db.get().await;
    let teams = crate::team_data::teams(&ctx.github).await?;
    if let Some(name) = find_assign_command(ctx, event) {
        if is_self_assign(&name, &event.issue.user.login) {
            return Ok((Some(name.to_string()), true));
        }
        // User included `r?` in the opening PR body.
        match find_reviewer_from_names(&db_client, &teams, config, &event.issue, &[name]).await {
            Ok(assignee) => return Ok((Some(assignee), true)),
            Err(e) => {
                event
                    .issue
                    .post_comment(&ctx.github, &e.to_string())
                    .await?;
                // Fall through below for normal diff detection.
            }
        }
    }
    // Errors fall-through to try fallback group.
    match find_reviewers_from_diff(config, diff) {
        Ok(candidates) if !candidates.is_empty() => {
            match find_reviewer_from_names(&db_client, &teams, config, &event.issue, &candidates)
                .await
            {
                Ok(assignee) => return Ok((Some(assignee), false)),
                Err(FindReviewerError::TeamNotFound(team)) => log::warn!(
                    "team {team} not found via diff from PR {}, \
                    is there maybe a misconfigured group?",
                    event.issue.global_id()
                ),
                Err(
                    e @ FindReviewerError::NoReviewer { .. }
                    | e @ FindReviewerError::AllReviewersFiltered { .. }
                    | e @ FindReviewerError::NoReviewerHasCapacity
                    | e @ FindReviewerError::ReviewerHasNoCapacity { .. }
                    | e @ FindReviewerError::ReviewerIsPrAuthor { .. }
                    | e @ FindReviewerError::ReviewerAlreadyAssigned { .. },
                ) => log::trace!(
                    "no reviewer could be determined for PR {}: {e}",
                    event.issue.global_id()
                ),
                Err(e @ FindReviewerError::ReviewerOnVacation { .. }) => {
                    // TODO: post a comment on the PR if the reviewer(s) were filtered due to being on vacation
                    log::trace!(
                        "no reviewer could be determined for PR {}: {e}",
                        event.issue.global_id()
                    )
                }
            }
        }
        // If no owners matched the diff, fall-through.
        Ok(_) => {}
        Err(e) => {
            log::warn!(
                "failed to find candidate reviewer from diff due to error: {e}\n\
                 Is the triagebot.toml misconfigured?"
            );
        }
    }

    if let Some(fallback) = config.adhoc_groups.get("fallback") {
        match find_reviewer_from_names(&db_client, &teams, config, &event.issue, fallback).await {
            Ok(assignee) => return Ok((Some(assignee), false)),
            Err(e) => {
                log::trace!(
                    "failed to select from fallback group for PR {}: {e}",
                    event.issue.global_id()
                );
            }
        }
    }
    Ok((None, false))
}

/// Returns a list of candidate reviewers to use based on which files were changed.
///
/// May return an error if the owners map is misconfigured.
///
/// Beware this may return an empty list if nothing matches.
fn find_reviewers_from_diff(
    config: &AssignConfig,
    diff: &[FileDiff],
) -> anyhow::Result<Vec<String>> {
    // Map of `owners` path to the number of changes found in that path.
    // This weights the reviewer choice towards places where the most edits are done.
    let mut counts: HashMap<&str, u32> = HashMap::new();
    // Iterate over the diff, counting the number of modified lines in each
    // file, and tracks those in the `counts` map.
    for file_diff in diff {
        // List of the longest `owners` patterns that match the current path. This
        // prefers choosing reviewers from deeply nested paths over those defined
        // for top-level paths, under the assumption that they are more
        // specialized.
        //
        // This is a list to handle the situation if multiple paths of the same
        // length match.
        let mut longest_owner_patterns = Vec::new();

        // Find the longest `owners` entries that match this path.
        let mut longest = HashMap::new();
        for owner_pattern in config.owners.keys() {
            let ignore = ignore::gitignore::GitignoreBuilder::new("/")
                .add_line(None, owner_pattern)
                .with_context(|| format!("owner file pattern `{owner_pattern}` is not valid"))?
                .build()?;
            if ignore
                .matched_path_or_any_parents(&file_diff.path, false)
                .is_ignore()
            {
                let owner_len = owner_pattern.split('/').count();
                longest.insert(owner_pattern, owner_len);
            }
        }
        let max_count = longest.values().copied().max().unwrap_or(0);
        longest_owner_patterns.extend(
            longest
                .iter()
                .filter(|(_, count)| **count == max_count)
                .map(|x| *x.0),
        );
        // Give some weight to these patterns to start. This helps with
        // files modified without any lines changed.
        for owner_pattern in &longest_owner_patterns {
            *counts.entry(owner_pattern).or_default() += 1;
        }

        // Count the modified lines.
        for line in file_diff.diff.lines() {
            if (!line.starts_with("+++") && line.starts_with('+'))
                || (!line.starts_with("---") && line.starts_with('-'))
            {
                for owner_path in &longest_owner_patterns {
                    *counts.entry(owner_path).or_default() += 1;
                }
            }
        }
    }
    // Use the `owners` entry with the most number of modifications.
    let max_count = counts.values().copied().max().unwrap_or(0);
    let max_paths = counts
        .iter()
        .filter(|(_, count)| **count == max_count)
        .map(|(path, _)| path);
    let mut potential: Vec<_> = max_paths
        .flat_map(|owner_path| &config.owners[*owner_path])
        .map(|owner| owner.to_string())
        .collect();
    // Dedupe. This isn't strictly necessary, as `find_reviewer_from_names` will deduplicate.
    // However, this helps with testing.
    potential.sort();
    potential.dedup();
    Ok(potential)
}

/// Handles a command posted in a comment.
pub(super) async fn handle_command(
    ctx: &Context,
    config: &AssignConfig,
    event: &Event,
    cmd: AssignCommand,
) -> anyhow::Result<()> {
    let is_team_member = if let Err(_) | Ok(false) = event.user().is_team_member(&ctx.github).await
    {
        false
    } else {
        true
    };

    // Don't handle commands in comments from the bot. Some of the comments it
    // posts contain commands to instruct the user, not things that the bot
    // should respond to.
    if event.user().login == ctx.username.as_str() {
        return Ok(());
    }

    let issue = event.issue().unwrap();
    if issue.is_pr() {
        if !issue.is_open() {
            issue
                .post_comment(&ctx.github, "Assignment is not allowed on a closed PR.")
                .await?;
            return Ok(());
        }
        if matches!(
            event,
            Event::Issue(IssuesEvent {
                action: IssuesAction::Opened,
                ..
            })
        ) {
            // Don't handle review request comments on new PRs. Those will be
            // handled by the new PR trigger (which also handles the
            // welcome message).
            return Ok(());
        }

        let teams = crate::team_data::teams(&ctx.github).await?;

        let assignee = match cmd {
            AssignCommand::Claim => event.user().login.clone(),
            AssignCommand::AssignUser { username } => username,
            AssignCommand::ReleaseAssignment => {
                log::trace!(
                    "ignoring release on PR {:?}, must always have assignee",
                    issue.global_id()
                );
                return Ok(());
            }
            AssignCommand::RequestReview { name } => {
                // Determine if assignee is a team. If yes, add the corresponding GH label.
                if let Some(team_name) = get_team_name(&teams, &issue, &name) {
                    let t_label = format!("T-{team_name}");
                    if let Err(err) = issue
                        .add_labels(&ctx.github, vec![github::Label { name: t_label }])
                        .await
                    {
                        if let Some(github::UnknownLabels { .. }) = err.downcast_ref() {
                            log::warn!("Error assigning label: {}", err);
                        } else {
                            return Err(err);
                        }
                    }
                }
                name
            }
        };

        let db_client = ctx.db.get().await;
        let assignee = match find_reviewer_from_names(
            &db_client,
            &teams,
            config,
            issue,
            &[assignee.to_string()],
        )
        .await
        {
            Ok(assignee) => assignee,
            Err(e) => {
                issue.post_comment(&ctx.github, &e.to_string()).await?;
                return Ok(());
            }
        };

        // Allow users on vacation to assign themselves to a PR, but not anyone else.
        if config.is_on_vacation(&assignee) && !is_self_assign(&assignee, &event.user().login) {
            // This is a comment, so there must already be a reviewer assigned. No need to assign anyone else.
            issue
                .post_comment(&ctx.github, &on_vacation_warning(&assignee))
                .await?;
            return Ok(());
        }
        // Do not assign PR author
        if issue.user.login.to_lowercase() == assignee.to_lowercase() {
            return Ok(());
        }

        set_assignee(issue, &ctx.github, &assignee).await;
    } else {
        let e = EditIssueBody::new(&issue, "ASSIGN");

        let to_assign = match cmd {
            AssignCommand::Claim => event.user().login.clone(),
            AssignCommand::AssignUser { username } => {
                if !is_team_member && username != event.user().login {
                    bail!("Only Rust team members can assign other users");
                }
                username.clone()
            }
            AssignCommand::ReleaseAssignment => {
                if let Some(AssignData {
                    user: Some(current),
                }) = e.current_data()
                {
                    if current == event.user().login || is_team_member {
                        issue.remove_assignees(&ctx.github, Selection::All).await?;
                        e.apply(&ctx.github, String::new(), AssignData { user: None })
                            .await?;
                        return Ok(());
                    } else {
                        bail!("Cannot release another user's assignment");
                    }
                } else {
                    let current = &event.user().login;
                    if issue.contain_assignee(current) {
                        issue
                            .remove_assignees(&ctx.github, Selection::One(&current))
                            .await?;
                        e.apply(&ctx.github, String::new(), AssignData { user: None })
                            .await?;
                        return Ok(());
                    } else {
                        bail!("Cannot release unassigned issue");
                    }
                };
            }
            AssignCommand::RequestReview { .. } => bail!("r? is only allowed on PRs."),
        };
        // Don't re-assign if aleady assigned, e.g. on comment edit
        if issue.contain_assignee(&to_assign) {
            log::trace!(
                "ignoring assign issue {} to {}, already assigned",
                issue.global_id(),
                to_assign,
            );
            return Ok(());
        }
        let data = AssignData {
            user: Some(to_assign.clone()),
        };

        e.apply(&ctx.github, String::new(), &data).await?;

        match issue.set_assignee(&ctx.github, &to_assign).await {
            Ok(()) => return Ok(()), // we are done
            Err(github::AssignmentError::InvalidAssignee) => {
                issue
                    .set_assignee(&ctx.github, &ctx.username)
                    .await
                    .context("self-assignment failed")?;
                let cmt_body = format!(
                    "This issue has been assigned to @{} via [this comment]({}).",
                    to_assign,
                    event.html_url().unwrap()
                );
                e.apply(&ctx.github, cmt_body, &data).await?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

fn strip_organization_prefix<'a>(issue: &Issue, name: &'a str) -> &'a str {
    let repo = issue.repository();
    // @ is optional, so it is trimmed separately
    // both @rust-lang/compiler and rust-lang/compiler should work
    name.trim_start_matches("@")
        .trim_start_matches(&format!("{}/", repo.organization))
}

/// Returns `Some(team_name)` if `name` corresponds to a name of a team.
fn get_team_name<'a>(teams: &Teams, issue: &Issue, name: &'a str) -> Option<&'a str> {
    let team_name = strip_organization_prefix(issue, name);
    // Remove "t-" or "T-" prefixes before checking if it's a team name
    let team_name = team_name.trim_start_matches("t-").trim_start_matches("T-");
    teams.teams.get(team_name).map(|_| team_name)
}

#[derive(PartialEq, Debug)]
pub enum FindReviewerError {
    /// User specified something like `r? foo/bar` where that team name could
    /// not be found.
    TeamNotFound(String),
    /// No reviewer could be found.
    ///
    /// This could happen if there is a cyclical group or other misconfiguration.
    /// `initial` is the initial list of candidate names.
    NoReviewer { initial: Vec<String> },
    /// All potential candidates were excluded. `initial` is the list of
    /// candidate names that were used to seed the selection. `filtered` is
    /// the users who were prevented from being assigned. One example where
    /// this happens is if the given name was for a team where the PR author
    /// is the only member.
    AllReviewersFiltered {
        initial: Vec<String>,
        filtered: Vec<String>,
    },
    /// No reviewer has capacity to accept a pull request assignment at this time
    NoReviewerHasCapacity,
    /// The requested reviewer has no capacity to accept a pull request
    /// assignment at this time
    ReviewerHasNoCapacity { username: String },
    /// Requested reviewer is on vacation
    /// (i.e. username is in [users_on_vacation] in the triagebot.toml)
    ReviewerOnVacation { username: String },
    /// Requested reviewer is PR author
    ReviewerIsPrAuthor { username: String },
    /// Requested reviewer is already assigned to that PR
    ReviewerAlreadyAssigned { username: String },
}

impl std::error::Error for FindReviewerError {}

impl fmt::Display for FindReviewerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            FindReviewerError::TeamNotFound(team) => {
                write!(
                    f,
                    "Team or group `{team}` not found.\n\
                    \n\
                    rust-lang team names can be found at https://github.com/rust-lang/team/tree/master/teams.\n\
                    Reviewer group names can be found in `triagebot.toml` in this repo."
                )
            }
            FindReviewerError::NoReviewer { initial } => {
                write!(
                    f,
                    "No reviewers could be found from initial request `{}`\n\
                     This repo may be misconfigured.\n\
                     Use `r?` to specify someone else to assign.",
                    initial.join(",")
                )
            }
            FindReviewerError::AllReviewersFiltered { initial, filtered } => {
                write!(
                    f,
                    "Could not assign reviewer from: `{}`.\n\
                     User(s) `{}` are either the PR author, already assigned, or on vacation. \
                     Please use `r?` to specify someone else to assign.",
                    initial.join(","),
                    filtered.join(","),
                )
            }
            FindReviewerError::ReviewerHasNoCapacity { username } => {
                write!(
                    f,
                    "{}",
                    REVIEWER_HAS_NO_CAPACITY.replace("{username}", username)
                )
            }
            FindReviewerError::NoReviewerHasCapacity => {
                write!(f, "{}", NO_REVIEWER_HAS_CAPACITY)
            }
            FindReviewerError::ReviewerOnVacation { username } => {
                write!(f, "{}", on_vacation_warning(username))
            }
            FindReviewerError::ReviewerIsPrAuthor { username } => {
                write!(
                    f,
                    "{}",
                    REVIEWER_IS_PR_AUTHOR.replace("{username}", username)
                )
            }
            FindReviewerError::ReviewerAlreadyAssigned { username } => {
                write!(
                    f,
                    "{}",
                    REVIEWER_ALREADY_ASSIGNED.replace("{username}", username)
                )
            }
        }
    }
}

/// Finds a reviewer to assign to a PR.
///
/// The `names` is a list of candidate reviewers `r?`, such as `compiler` or
/// `@octocat`, or names from the owners map. It can contain GitHub usernames,
/// auto-assign groups, or rust-lang team names. It must have at least one
/// entry.
async fn find_reviewer_from_names(
    _db: &DbClient,
    teams: &Teams,
    config: &AssignConfig,
    issue: &Issue,
    names: &[String],
) -> Result<String, FindReviewerError> {
    let candidates = candidate_reviewers_from_names(teams, config, issue, names)?;
    // This uses a relatively primitive random choice algorithm.
    // GitHub's CODEOWNERS supports much more sophisticated options, such as:
    //
    // - Round robin: Chooses reviewers based on who's received the least
    //   recent review request, focusing on alternating between all members of
    //   the team regardless of the number of outstanding reviews they
    //   currently have.
    // - Load balance: Chooses reviewers based on each member's total number
    //   of recent review requests and considers the number of outstanding
    //   reviews for each member. The load balance algorithm tries to ensure
    //   that each team member reviews an equal number of pull requests in any
    //   30 day period.
    //
    // Additionally, with CODEOWNERS, users marked as "Busy" in the GitHub UI
    // will not be selected for reviewer. There are several other options for
    // configuring CODEOWNERS as well.
    //
    // These are all ideas for improving the selection here. However, I'm not
    // sure they are really worth the effort.

    log::info!(
        "[#{}] Initial unfiltered list of candidates: {:?}",
        issue.number,
        candidates
    );

    // Special case user "ghost", we always skip filtering
    if candidates.contains("ghost") {
        return Ok("ghost".to_string());
    }

    // Return unfiltered list of candidates
    Ok(candidates
        .into_iter()
        .choose(&mut rand::thread_rng())
        .expect("candidate_reviewers_from_names should return at least one entry")
        .to_string())
}

/// Recursively expand all teams and adhoc groups found within `names`.
/// Returns a set of expanded usernames.
/// Also normalizes usernames from `@user` to `user`.
///
/// Returns `(set of expanded users, expansion_happened)`.
/// `expansion_happened` signals if any expansion has been performed.
fn expand_teams_and_groups(
    teams: &Teams,
    issue: &Issue,
    config: &AssignConfig,
    names: &[String],
) -> Result<(HashSet<String>, bool), FindReviewerError> {
    let mut expanded = HashSet::new();
    let mut expansion_happened = false;

    // Keep track of groups seen to avoid cycles and avoid expanding the same
    // team multiple times.
    let mut seen_names = HashSet::new();

    // This is a queue of potential groups or usernames to expand. The loop
    // below will pop from this and then append the expanded results of teams.
    // Usernames will be added to `expanded`.
    let mut to_be_expanded: Vec<&str> = names.iter().map(|n| n.as_str()).collect();

    // Loop over names to recursively expand them.
    while let Some(name_to_expand) = to_be_expanded.pop() {
        // `name_to_expand` could be a team name, an adhoc group name or a username.
        let maybe_team = get_team_name(teams, issue, name_to_expand);
        let maybe_group = strip_organization_prefix(issue, name_to_expand);
        let maybe_user = name_to_expand.strip_prefix('@').unwrap_or(name_to_expand);

        // Try ad-hoc groups first.
        if let Some(group_members) = config.adhoc_groups.get(maybe_group) {
            expansion_happened = true;

            // If a group has already been expanded, don't expand it again.
            if seen_names.insert(maybe_group) {
                to_be_expanded.extend(group_members.iter().map(|s| s.as_str()));
            }
            continue;
        }

        // Check for a team name.
        // Allow either a direct team name like `rustdoc` or a GitHub-style
        // team name of `rust-lang/rustdoc` (though this does not check if
        // that is a real GitHub team name).
        //
        // This ignores subteam relationships (it only uses direct members).
        if let Some(team) = maybe_team.and_then(|t| teams.teams.get(t)) {
            expansion_happened = true;
            expanded.extend(team.members.iter().map(|member| member.github.clone()));
            continue;
        }

        // Here we know it's not a known team nor a group.
        // If the username contains a slash, assume that it is an unknown team.
        if maybe_user.contains('/') {
            return Err(FindReviewerError::TeamNotFound(maybe_user.to_string()));
        }

        // Assume it is a user.
        expanded.insert(maybe_user.to_string());
    }

    Ok((expanded, expansion_happened))
}

/// Returns a list of candidate usernames (from relevant teams) to choose as a reviewer.
/// If not reviewer is available, returns an error.
fn candidate_reviewers_from_names<'a>(
    teams: &'a Teams,
    config: &'a AssignConfig,
    issue: &Issue,
    names: &'a [String],
) -> Result<HashSet<String>, FindReviewerError> {
    let (expanded, expansion_happened) = expand_teams_and_groups(teams, issue, config, names)?;
    let expanded_count = expanded.len();

    // Set of candidate usernames to choose from.
    // We go through each expanded candidate and store either success or an error for them.
    let mut candidates: Vec<Result<String, FindReviewerError>> = Vec::new();

    for candidate in expanded {
        let name_lower = candidate.to_lowercase();
        let is_pr_author = name_lower == issue.user.login.to_lowercase();
        let is_on_vacation = config.is_on_vacation(&candidate);
        let is_already_assigned = issue
            .assignees
            .iter()
            .any(|assignee| name_lower == assignee.login.to_lowercase());

        // Record the reason why the candidate was filtered out
        let reason = {
            if is_pr_author {
                Some(FindReviewerError::ReviewerIsPrAuthor {
                    username: candidate.clone(),
                })
            } else if is_on_vacation {
                Some(FindReviewerError::ReviewerOnVacation {
                    username: candidate.clone(),
                })
            } else if is_already_assigned {
                Some(FindReviewerError::ReviewerAlreadyAssigned {
                    username: candidate.clone(),
                })
            } else {
                None
            }
        };

        if let Some(error_reason) = reason {
            candidates.push(Err(error_reason));
        } else {
            candidates.push(Ok(candidate));
        }
    }
    assert_eq!(candidates.len(), expanded_count);

    let valid_candidates: HashSet<String> = candidates
        .iter()
        .filter_map(|res| res.as_ref().ok().cloned())
        .collect();

    if valid_candidates.is_empty() {
        // Was it a request for a single user, i.e. `r? @username`?
        let is_single_user = names.len() == 1 && !expansion_happened;

        // If we requested a single user for a review, we return a concrete error message
        // describing why they couldn't be assigned.
        if is_single_user {
            Err(candidates
                .pop()
                .unwrap()
                .expect_err("valid_candidates is empty, so this should be an error"))
        } else {
            // If it was a request for a team or a group, and no one is available, simply
            // return `NoReviewer`.
            log::warn!(
                "No valid candidates found for review request on {}. Reasons: {:?}",
                issue.global_id(),
                candidates
            );
            Err(FindReviewerError::NoReviewer {
                initial: names.to_vec(),
            })
        }
    } else {
        Ok(valid_candidates)
    }
}
