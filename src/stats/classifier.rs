/// Task categories for deterministic classification (no LLM needed).
#[derive(Debug, Clone, PartialEq)]
pub enum TaskCategory {
    Coding,
    Debugging,
    Refactoring,
    Testing,
    Exploration,
    Planning,
    Git,
    BuildDeploy,
    Documentation,
    Conversation,
    General,
}

impl std::fmt::Display for TaskCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coding => write!(f, "coding"),
            Self::Debugging => write!(f, "debugging"),
            Self::Refactoring => write!(f, "refactoring"),
            Self::Testing => write!(f, "testing"),
            Self::Exploration => write!(f, "exploration"),
            Self::Planning => write!(f, "planning"),
            Self::Git => write!(f, "git"),
            Self::BuildDeploy => write!(f, "build_deploy"),
            Self::Documentation => write!(f, "documentation"),
            Self::Conversation => write!(f, "conversation"),
            Self::General => write!(f, "general"),
        }
    }
}

/// Classify a set of tool names into a task category.
/// Uses deterministic tool pattern matching (codeburn approach).
pub fn classify(tool_names: &[String]) -> TaskCategory {
    let has = |name: &str| tool_names.iter().any(|t| t.contains(name));

    // Priority order: most specific patterns first
    if has("Edit") || has("Write") || has("NotebookEdit") {
        return TaskCategory::Coding;
    }

    if tool_names.iter().any(|t| {
        t.contains("test") || t.contains("Test")
    }) {
        return TaskCategory::Testing;
    }

    if has("Bash") && has("Read") && !has("Edit") && !has("Write") {
        return TaskCategory::Exploration;
    }

    if has("Bash") && tool_names.iter().any(|t| {
        t.contains("git") || t.contains("gh ")
    }) {
        return TaskCategory::Git;
    }

    if has("Read") && !has("Bash") && !has("Edit") {
        return TaskCategory::Exploration;
    }

    if has("Bash") {
        return TaskCategory::BuildDeploy;
    }

    if tool_names.is_empty() {
        return TaskCategory::Conversation;
    }

    TaskCategory::General
}

/// Classify from a task description string using keyword matching.
pub fn classify_from_text(text: &str) -> TaskCategory {
    let lower = text.to_lowercase();

    if lower.contains("refactor") {
        return TaskCategory::Refactoring;
    }
    if lower.contains("test") || lower.contains("spec") {
        return TaskCategory::Testing;
    }
    if lower.contains("debug") || lower.contains("fix bug") || lower.contains("error") {
        return TaskCategory::Debugging;
    }
    if lower.contains("doc") || lower.contains("readme") {
        return TaskCategory::Documentation;
    }
    if lower.contains("plan") || lower.contains("design") || lower.contains("architect") {
        return TaskCategory::Planning;
    }
    if lower.contains("deploy") || lower.contains("build") || lower.contains("ci") {
        return TaskCategory::BuildDeploy;
    }
    if lower.contains("git") || lower.contains("commit") || lower.contains("merge") || lower.contains("pr ") {
        return TaskCategory::Git;
    }

    TaskCategory::Coding
}
