// Branch summarization — summarize changes made on a git branch.
//
// When the agent switches to a new branch or continues work on an existing
// one, a branch summary provides context about what was done. This is
// referenced by `AgentHarnessPhase::BranchSummary` in the harness.

use serde::{Deserialize, Serialize};

/// A summary of changes made on a git branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSummary {
    /// The branch name.
    pub branch_name: String,
    /// Summary of the work done on this branch.
    pub summary: String,
    /// Files modified, created, or deleted.
    pub files_changed: Vec<String>,
    /// The commit SHA at the tip of the branch.
    pub tip_commit: Option<String>,
    /// Whether the summary is stale (more commits since summarization).
    pub is_stale: bool,
}

/// Configuration for branch summarization.
#[derive(Debug, Clone)]
pub struct BranchSummarizationConfig {
    /// Maximum number of files to list in the summary.
    pub max_files: usize,
    /// Whether to include the diff in the summary prompt.
    pub include_diff: bool,
}

impl Default for BranchSummarizationConfig {
    fn default() -> Self {
        BranchSummarizationConfig {
            max_files: 50,
            include_diff: false,
        }
    }
}

/// Build a prompt for the LLM to generate a branch summary.
///
/// The prompt asks the model to summarize the work done on the branch
/// based on the git log and changed files.
pub fn build_branch_summary_prompt(
    branch_name: &str,
    git_log: &str,
    changed_files: &[String],
    existing_summary: Option<&BranchSummary>,
) -> String {
    let files_list = if changed_files.len() > 50 {
        let mut truncated = changed_files[..50].to_vec();
        truncated.push(format!("... and {} more files", changed_files.len() - 50));
        truncated.join("\n")
    } else {
        changed_files.join("\n")
    };

    let existing_context = if let Some(existing) = existing_summary {
        format!(
            "Here is an existing summary of this branch:\n<summary>\n{}\n</summary>\n\n\
             Update and extend this summary with the new changes below.\n\n",
            existing.summary
        )
    } else {
        String::new()
    };

    format!(
        "{existing_context}\
        Summarize the work done on the git branch \"{branch_name}\".\n\n\
        The summary should be concise (≤300 words) and cover:\n\
        1. The main goal or feature being implemented\n\
        2. Key architectural decisions and trade-offs\n\
        3. Notable files created, modified, or deleted\n\
        4. Any unfinished work or known issues\n\n\
        <git_log>\n{git_log}\n</git_log>\n\n\
        <changed_files>\n{files_list}\n</changed_files>"
    )
}

/// Merge a new branch summary into an existing one.
///
/// The new summary's content replaces the old, but the files list is merged.
pub fn merge_summaries(
    existing: Option<&BranchSummary>,
    new_summary: String,
    new_files: Vec<String>,
    tip_commit: Option<String>,
) -> BranchSummary {
    let mut files = match existing {
        Some(e) => {
            let mut merged: Vec<String> = e.files_changed.clone();
            for f in new_files {
                if !merged.contains(&f) {
                    merged.push(f);
                }
            }
            merged
        }
        None => new_files,
    };

    files.sort();
    files.dedup();

    BranchSummary {
        branch_name: existing.map(|e| e.branch_name.clone()).unwrap_or_default(),
        summary: new_summary,
        files_changed: files,
        tip_commit,
        is_stale: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_prompt_without_existing() {
        let prompt = build_branch_summary_prompt(
            "feat/my-feature",
            "abc123 Add feature X\n",
            &["src/main.rs".into(), "src/lib.rs".into()],
            None,
        );
        assert!(prompt.contains("feat/my-feature"));
        assert!(prompt.contains("src/main.rs"));
        assert!(prompt.contains("abc123"));
        assert!(!prompt.contains("existing summary"));
    }

    #[test]
    fn test_build_prompt_with_existing() {
        let existing = BranchSummary {
            branch_name: "feat/my-feature".into(),
            summary: "Added feature X".into(),
            files_changed: vec!["src/main.rs".into()],
            tip_commit: Some("abc123".into()),
            is_stale: false,
        };
        let prompt = build_branch_summary_prompt(
            "feat/my-feature",
            "def456 Refine feature X\n",
            &["src/lib.rs".into()],
            Some(&existing),
        );
        assert!(prompt.contains("existing summary"));
        assert!(prompt.contains("Added feature X"));
        assert!(prompt.contains("def456"));
    }

    #[test]
    fn test_merge_summaries() {
        let existing = BranchSummary {
            branch_name: "feat/my-feature".into(),
            summary: "Old summary".into(),
            files_changed: vec!["src/a.rs".into()],
            tip_commit: Some("abc".into()),
            is_stale: false,
        };

        let merged = merge_summaries(
            Some(&existing),
            "New summary".into(),
            vec!["src/b.rs".into()],
            Some("def".into()),
        );

        assert_eq!(merged.summary, "New summary");
        assert_eq!(merged.files_changed, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(merged.tip_commit, Some("def".into()));
    }
}
