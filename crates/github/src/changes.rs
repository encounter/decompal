use std::{cmp::Ordering, collections::BTreeMap};

use anyhow::{Context, Result};
use decomp_dev_core::{
    models::{Commit, Project, PullReportStyle},
    util::format_percent,
};
use objdiff_core::bindings::report::{
    ChangeItem, ChangeItemInfo, ChangeUnit, Changes, Report, ReportItem, ReportUnit,
};
use octocrab::{
    Octocrab,
    models::{RepositoryId, pulls::PullRequest},
};

pub fn generate_changes(previous: &Report, current: &Report) -> Result<Changes> {
    let mut changes = Changes { from: previous.measures, to: current.measures, units: vec![] };
    for prev_unit in &previous.units {
        let curr_unit = current.units.iter().find(|u| u.name == prev_unit.name);
        let sections = process_items(prev_unit, curr_unit, |u| &u.sections, false);
        let functions = process_items(prev_unit, curr_unit, |u| &u.functions, true);

        let prev_measures = prev_unit.measures;
        let curr_measures = curr_unit.and_then(|u| u.measures);
        if !functions.is_empty() || prev_measures != curr_measures {
            changes.units.push(ChangeUnit {
                name: prev_unit.name.clone(),
                from: prev_measures,
                to: curr_measures,
                sections,
                functions,
                metadata: curr_unit
                    .as_ref()
                    .and_then(|u| u.metadata.clone())
                    .or_else(|| prev_unit.metadata.clone()),
            });
        }
    }
    for curr_unit in &current.units {
        if !previous.units.iter().any(|u| u.name == curr_unit.name) {
            changes.units.push(ChangeUnit {
                name: curr_unit.name.clone(),
                from: None,
                to: curr_unit.measures,
                sections: process_new_items(&curr_unit.sections),
                functions: process_new_items(&curr_unit.functions),
                metadata: curr_unit.metadata.clone(),
            });
        }
    }
    Ok(changes)
}

fn process_items<F: Fn(&ReportUnit) -> &Vec<ReportItem>>(
    prev_unit: &ReportUnit,
    curr_unit: Option<&ReportUnit>,
    getter: F,
    pair_by_virtual_address: bool,
) -> Vec<ChangeItem> {
    let prev_items = getter(prev_unit);
    let mut items = vec![];
    if let Some(curr_unit) = curr_unit {
        let curr_items = getter(curr_unit);
        for prev_item in prev_items {
            let prev_item_info = ChangeItemInfo::from(prev_item);
            let prev_item_address = prev_item.metadata.as_ref().and_then(|m| m.virtual_address);
            let curr_item = curr_items.iter().find(|f| {
                f.name == prev_item.name
                    || (pair_by_virtual_address
                        && prev_item_address.is_some_and(|a| {
                            f.metadata
                                .as_ref()
                                .and_then(|m| m.virtual_address)
                                .is_some_and(|b| a == b)
                        }))
            });
            if let Some(curr_item) = curr_item {
                let curr_item_info = ChangeItemInfo::from(curr_item);
                if prev_item_info != curr_item_info {
                    items.push(ChangeItem {
                        name: curr_item.name.clone(),
                        from: Some(prev_item_info),
                        to: Some(curr_item_info),
                        metadata: curr_item.metadata.clone(),
                    });
                }
            } else {
                items.push(ChangeItem {
                    name: prev_item.name.clone(),
                    from: Some(prev_item_info),
                    to: None,
                    metadata: prev_item.metadata.clone(),
                });
            }
        }
        for curr_item in curr_items {
            let curr_item_address = curr_item.metadata.as_ref().and_then(|m| m.virtual_address);
            if !prev_items.iter().any(|f| {
                f.name == curr_item.name
                    || (pair_by_virtual_address
                        && curr_item_address.is_some_and(|a| {
                            f.metadata
                                .as_ref()
                                .and_then(|m| m.virtual_address)
                                .is_some_and(|b| a == b)
                        }))
            }) {
                items.push(ChangeItem {
                    name: curr_item.name.clone(),
                    from: None,
                    to: Some(ChangeItemInfo::from(curr_item)),
                    metadata: curr_item.metadata.clone(),
                });
            }
        }
    } else {
        for prev_item in prev_items {
            items.push(ChangeItem {
                name: prev_item.name.clone(),
                from: Some(ChangeItemInfo::from(prev_item)),
                to: None,
                metadata: prev_item.metadata.clone(),
            });
        }
    }
    items
}

fn process_new_items(items: &[ReportItem]) -> Vec<ChangeItem> {
    items
        .iter()
        .map(|item| ChangeItem {
            name: item.name.clone(),
            from: None,
            to: Some(ChangeItemInfo::from(item)),
            metadata: item.metadata.clone(),
        })
        .collect()
}

fn measure_line_matched(
    name: &str,
    from: u64,
    from_percent: f32,
    to: u64,
    to_percent: f32,
) -> String {
    let emoji = if to > from { "📈" } else { "📉" };
    let to_percent_str = format_percent(to_percent);
    let percent_diff = to_percent - from_percent;
    let percent_diff_str = if percent_diff < 0.0 {
        format!("{percent_diff:.2}%")
    } else {
        format!("+{percent_diff:.2}%")
    };
    let bytes_diff = to as i64 - from as i64;
    let bytes_str = match bytes_diff.cmp(&0) {
        Ordering::Less => bytes_diff.to_string(),
        Ordering::Equal | Ordering::Greater => format!("+{bytes_diff}"),
    };
    format!("{emoji} **{name}**: {to_percent_str} ({percent_diff_str}, {bytes_str} bytes)\n")
}

fn measure_line_bytes(name: &str, from: u64, to: u64) -> String {
    let diff = to as i64 - from as i64;
    let diff_str = match diff.cmp(&0) {
        Ordering::Less => diff.to_string(),
        Ordering::Equal | Ordering::Greater => format!("+{diff}"),
    };
    format!("**{name}**: {to} bytes ({diff_str} bytes)\n")
}

fn measure_line_simple(name: &str, from: u64, to: u64) -> String {
    let diff = to as i64 - from as i64;
    let diff_str = match diff.cmp(&0) {
        Ordering::Less => diff.to_string(),
        Ordering::Equal | Ordering::Greater => format!("+{diff}"),
    };
    format!("**{name}**: {to} ({diff_str})\n")
}

const MAX_CHANGE_LINES: usize = 30;

// Note: The order the tables are printed in is determined by the order of the variants in this enum.
#[derive(PartialEq, Eq, PartialOrd, Ord, Copy, Clone, Debug)]
enum ChangeKind {
    NewMatch,
    BrokenMatch,
    Improvement,
    Regression,
}

impl ChangeKind {
    fn emoji(self) -> &'static str {
        match self {
            ChangeKind::NewMatch => "✅",
            ChangeKind::BrokenMatch => "🥀",
            ChangeKind::Improvement => "📈",
            ChangeKind::Regression => "📉",
        }
    }

    fn singular_description(self) -> &'static str {
        match self {
            ChangeKind::NewMatch => "new match",
            ChangeKind::BrokenMatch => "broken match",
            ChangeKind::Improvement => "improvement in an unmatched item",
            ChangeKind::Regression => "regression in an unmatched item",
        }
    }

    fn plural_description(self) -> &'static str {
        match self {
            ChangeKind::NewMatch => "new matches",
            ChangeKind::BrokenMatch => "broken matches",
            ChangeKind::Improvement => "improvements in unmatched items",
            ChangeKind::Regression => "regressions in unmatched items",
        }
    }
}

struct ChangeLine {
    kind: ChangeKind,
    unit_name: String,
    item_name: String,
    from_fuzzy_match_percent: f32,
    to_fuzzy_match_percent: f32,
    bytes_diff: i64,
}

fn output_line(line: &ChangeLine, out: &mut String) {
    let bytes_str = match line.bytes_diff.cmp(&0) {
        Ordering::Less => line.bytes_diff.to_string(),
        Ordering::Equal => "0".to_string(),
        Ordering::Greater => format!("+{}", line.bytes_diff),
    };

    out.push_str(&format!(
        "| `{}` | `{}` | {} | {} | {} |\n",
        line.unit_name,
        line.item_name,
        bytes_str,
        format_percent(line.from_fuzzy_match_percent),
        format_percent(line.to_fuzzy_match_percent),
    ));
}

fn generate_changes_list(changes: Vec<ChangeLine>, out: &mut String) {
    let mut changes_by_kind = BTreeMap::new();
    for change in changes {
        changes_by_kind.entry(change.kind).or_insert(vec![]).push(change);
    }
    for (change_kind, mut changes) in changes_by_kind {
        let total_changes = changes.len();
        let description = if total_changes == 0 {
            out.push_str(&format!("No {}.\n", change_kind.plural_description()));
            continue;
        } else if total_changes == 1 {
            change_kind.singular_description()
        } else {
            change_kind.plural_description()
        };

        if change_kind == ChangeKind::BrokenMatch {
            out.push_str("<details open>\n");
        } else {
            out.push_str("<details>\n");
        }
        out.push_str(&format!(
            "<summary>{} {total_changes} {description}</summary>\n",
            change_kind.emoji()
        ));
        out.push('\n'); // Must include a blank line before a table
        out.push_str("| Unit | Item | Bytes | Before | After |\n");
        out.push_str("| - | - | - | - | - |\n");

        // Sort to show the biggest changes first.
        match change_kind {
            ChangeKind::NewMatch | ChangeKind::Improvement => {
                changes.sort_by_key(|item| -item.bytes_diff)
            }
            ChangeKind::BrokenMatch | ChangeKind::Regression => {
                changes.sort_by_key(|item| item.bytes_diff)
            }
        }

        let mut shown_changes = 0;
        for line in changes.iter().take(MAX_CHANGE_LINES) {
            output_line(line, out);
            shown_changes += 1;
        }

        out.push('\n'); // Must include a blank line after a table

        let remaining = total_changes - shown_changes;
        if remaining > 0 {
            out.push_str(&format!("...and {remaining} more {description}\n"));
        }
        out.push_str("</details>\n");
        out.push('\n');
    }
}

pub fn generate_missing_report_comment(
    version: &str,
    from_commit: Option<&Commit>,
    to_commit: Option<&Commit>,
    base_missing: bool,
) -> String {
    let error_msg = if base_missing {
        "Base report not found. Either the build or webhook failed on the base branch or this version was just added in this PR."
    } else {
        "PR report not found. The build or webhook failed on this PR."
    };
    format!(
        "### Report for {} ({} - {})\n\n[!] {}\n\n",
        version,
        from_commit.map_or("<none>", |c| &c.sha[..7]),
        to_commit.map_or("<none>", |c| &c.sha[..7]),
        error_msg
    )
}

pub fn generate_combined_comment(version_comments: Vec<String>) -> String {
    version_comments.join("---\n\n")
}

pub fn generate_comment(
    from: &Report,
    to: &Report,
    version: Option<&str>,
    from_commit: Option<&Commit>,
    to_commit: Option<&Commit>,
    changes: Changes,
) -> String {
    let mut comment = format!(
        "### Report for {} ({} - {})\n\n",
        version.unwrap_or("unknown"),
        from_commit.map_or("<none>", |c| &c.sha[..7]),
        to_commit.map_or("<none>", |c| &c.sha[..7])
    );
    let mut measure_written = false;
    let from_measures = from.measures.unwrap_or_default();
    let to_measures = to.measures.unwrap_or_default();
    if from_measures.total_code != to_measures.total_code {
        comment.push_str(&measure_line_bytes(
            "Total code",
            from_measures.total_code,
            to_measures.total_code,
        ));
        measure_written = true;
    }
    if from_measures.total_functions != to_measures.total_functions {
        comment.push_str(&measure_line_simple(
            "Total functions",
            from_measures.total_functions as u64,
            to_measures.total_functions as u64,
        ));
        measure_written = true;
    }
    if from_measures.matched_code != to_measures.matched_code {
        comment.push_str(&measure_line_matched(
            "Matched code",
            from_measures.matched_code,
            from_measures.matched_code_percent,
            to_measures.matched_code,
            to_measures.matched_code_percent,
        ));
        measure_written = true;
    }
    if from_measures.complete_code != to_measures.complete_code {
        comment.push_str(&measure_line_matched(
            "Linked code",
            from_measures.complete_code,
            from_measures.complete_code_percent,
            to_measures.complete_code,
            to_measures.complete_code_percent,
        ));
        measure_written = true;
    }
    if from_measures.total_data != to_measures.total_data {
        comment.push_str(&measure_line_bytes(
            "Total data",
            from_measures.total_data,
            to_measures.total_data,
        ));
        measure_written = true;
    }
    if from_measures.matched_data != to_measures.matched_data {
        comment.push_str(&measure_line_matched(
            "Matched data",
            from_measures.matched_data,
            from_measures.matched_data_percent,
            to_measures.matched_data,
            to_measures.matched_data_percent,
        ));
        measure_written = true;
    }
    if from_measures.complete_data != to_measures.complete_data {
        comment.push_str(&measure_line_matched(
            "Linked data",
            from_measures.complete_data,
            from_measures.complete_data_percent,
            to_measures.complete_data,
            to_measures.complete_data_percent,
        ));
        measure_written = true;
    }
    if measure_written {
        comment.push('\n');
    }
    let mut iter = changes.units.into_iter().flat_map(|mut unit| {
        let sections = core::mem::take(&mut unit.sections);
        let functions = core::mem::take(&mut unit.functions);
        sections
            .into_iter()
            .filter(|s| s.name != ".text")
            .chain(functions)
            .map(move |f| (unit.clone(), f))
    });

    let mut changes = vec![];

    for (unit, item) in iter.by_ref() {
        let (from, to) = match (item.from, item.to) {
            (Some(from), Some(to)) => (from, to),
            (None, Some(to)) => (ChangeItemInfo::default(), to),
            (Some(from), None) => (from, ChangeItemInfo::default()),
            (None, None) => continue,
        };
        let kind = if to.fuzzy_match_percent == 100.0 {
            ChangeKind::NewMatch
        } else if from.fuzzy_match_percent == 100.0 {
            ChangeKind::BrokenMatch
        } else if to.fuzzy_match_percent > from.fuzzy_match_percent {
            ChangeKind::Improvement
        } else if from.fuzzy_match_percent > 0.0 {
            ChangeKind::Regression
        } else {
            continue; // No change
        };
        let from_bytes = ((from.fuzzy_match_percent as f64 / 100.0) * from.size as f64) as u64;
        let to_bytes = ((to.fuzzy_match_percent as f64 / 100.0) * to.size as f64) as u64;
        let bytes_diff = to_bytes as i64 - from_bytes as i64;
        let name =
            item.metadata.as_ref().and_then(|m| m.demangled_name.as_deref()).unwrap_or(&item.name);

        let change = ChangeLine {
            kind,
            unit_name: unit.name.to_owned(),
            item_name: name.to_owned(),
            bytes_diff,
            from_fuzzy_match_percent: from.fuzzy_match_percent,
            to_fuzzy_match_percent: to.fuzzy_match_percent,
        };

        changes.push(change);
    }
    if !changes.is_empty() {
        generate_changes_list(changes, &mut comment);
    } else {
        comment.push_str("No changes\n");
    }
    comment
}

/// Post or update a PR comment with the report.
pub async fn post_pr_comment(
    client: &Octocrab,
    project: &Project,
    repository_id: RepositoryId,
    pull: &PullRequest,
    combined_comment: &str,
) -> Result<()> {
    if project.pr_report_style == PullReportStyle::Description {
        let start_marker = "<!-- decomp.dev report start -->";
        let end_marker = "<!-- decomp.dev report end -->";
        let new_section = format!("{start_marker}\n{combined_comment}\n{end_marker}");
        let existing_body = pull.body.as_deref().unwrap_or_default();
        let new_body = if let Some(start_idx) = existing_body.find(start_marker) {
            if let Some(end_rel) = existing_body[start_idx..].find(end_marker) {
                let end_idx = start_idx + end_rel + end_marker.len();
                format!(
                    "{}{}{}",
                    &existing_body[..start_idx],
                    new_section,
                    &existing_body[end_idx..]
                )
            } else {
                format!("{existing_body}\n\n---\n\n{new_section}")
            }
        } else if existing_body.trim().is_empty() {
            new_section
        } else {
            format!("{}\n\n---\n\n{}", existing_body.trim(), new_section)
        };

        client
            .pulls(&project.owner, &project.repo)
            .update(pull.number)
            .body(new_body)
            .send()
            .await
            .context("Failed to update pull request body")?;
    } else {
        let issues = client.issues_by_id(repository_id);
        // Only fetch first page for now
        let existing_comments = issues.list_comments(pull.number).send().await?;

        // Find existing report comments
        let existing_report_comments: Vec<_> = existing_comments
            .items
            .iter()
            .filter(|comment| {
                comment.body.as_ref().is_some_and(|body| body.contains("### Report for "))
            })
            .collect();

        if let Some(first_comment) = existing_report_comments.first() {
            // Update the first comment
            issues
                .update_comment(first_comment.id, combined_comment.to_string())
                .await
                .context("Failed to update existing comment")?;

            // Delete any additional report comments
            for comment in existing_report_comments.iter().skip(1) {
                if let Err(e) = issues.delete_comment(comment.id).await {
                    tracing::warn!("Failed to delete old comment {}: {}", comment.id, e);
                }
            }
        } else {
            // Create new comment
            issues
                .create_comment(pull.number, combined_comment.to_string())
                .await
                .context("Failed to create comment")?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use decomp_dev_core::models::Commit;
    use time::UtcDateTime;

    use super::*;

    #[test]
    fn test_generate_missing_report_comment() {
        let commit = Commit {
            sha: "abc1234567890".to_string(),
            message: Some("Test commit".to_string()),
            timestamp: UtcDateTime::UNIX_EPOCH,
        };
        let comment =
            generate_missing_report_comment("GALE01", Some(&commit), Some(&commit), false);
        assert_eq!(
            comment,
            "### Report for GALE01 (abc1234 - abc1234)\n\n[!] PR report not found. The build or webhook failed on this PR.\n\n"
        );
        let comment = generate_missing_report_comment("GALE01", Some(&commit), Some(&commit), true);
        assert_eq!(
            comment,
            "### Report for GALE01 (abc1234 - abc1234)\n\n[!] Base report not found. Either the build or webhook failed on the base branch or this version was just added in this PR.\n\n"
        );
    }

    #[test]
    fn test_commit_sha_truncation() {
        let long_commit = Commit {
            sha: "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            message: Some("Long commit SHA".to_string()),
            timestamp: UtcDateTime::UNIX_EPOCH,
        };
        let comment = generate_missing_report_comment(
            "GALE01",
            Some(&long_commit),
            Some(&long_commit),
            false,
        );
        // Should truncate SHA to 7 characters
        assert!(comment.contains("(abcdef1 - abcdef1)"));
        assert!(!comment.contains("abcdef1234567890"));
    }
}
