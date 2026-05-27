//! Approval 决策 — 见 §4.2。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Read,
    Write,
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    AlwaysAllow,
    PerCall,
    #[default]
    ReadOnlyAutoAllow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Ask,
    Deny { reason: String },
}

pub fn decide(mode: ApprovalMode, category: ToolCategory) -> ApprovalDecision {
    use ApprovalDecision::*;
    use ApprovalMode::*;
    use ToolCategory::*;
    match (mode, category) {
        (AlwaysAllow, _) => Allow,
        (PerCall, _) => Ask,
        (ReadOnlyAutoAllow, Read) => Allow,
        (ReadOnlyAutoAllow, _) => Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_allow_passes_everything() {
        assert_eq!(
            decide(ApprovalMode::AlwaysAllow, ToolCategory::Read),
            ApprovalDecision::Allow
        );
        assert_eq!(
            decide(ApprovalMode::AlwaysAllow, ToolCategory::Write),
            ApprovalDecision::Allow
        );
        assert_eq!(
            decide(ApprovalMode::AlwaysAllow, ToolCategory::Execute),
            ApprovalDecision::Allow
        );
    }

    #[test]
    fn per_call_asks_everything() {
        assert_eq!(
            decide(ApprovalMode::PerCall, ToolCategory::Read),
            ApprovalDecision::Ask
        );
        assert_eq!(
            decide(ApprovalMode::PerCall, ToolCategory::Write),
            ApprovalDecision::Ask
        );
    }

    #[test]
    fn read_only_auto_allows_reads_only() {
        assert_eq!(
            decide(ApprovalMode::ReadOnlyAutoAllow, ToolCategory::Read),
            ApprovalDecision::Allow
        );
        assert_eq!(
            decide(ApprovalMode::ReadOnlyAutoAllow, ToolCategory::Write),
            ApprovalDecision::Ask
        );
        assert_eq!(
            decide(ApprovalMode::ReadOnlyAutoAllow, ToolCategory::Execute),
            ApprovalDecision::Ask
        );
    }
}
