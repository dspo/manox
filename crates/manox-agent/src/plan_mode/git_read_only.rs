//! Read-only `git` classification for plan mode's Bash arm.
//!
//! Plan mode blocks Bash wholesale except single read-only `git`
//! invocations. Classification is syntactic, via brush's shell parser — not
//! string-prefix matching — so `git -C <path> log` and
//! `git --git-dir=<path> status` classify exactly like their bare forms.
//! The policy is a closed allowlist: any construct not explicitly admitted
//! is rejected. Rejected structurally: command lists (`;`, `&&`, `||`,
//! newlines), pipelines, redirections, background `&`, subshells and
//! compounds, function definitions, env-assignment prefixes, process
//! substitution, and any word carrying expansion or quoting (`$`, backtick,
//! quotes, backslash) so no substitution can smuggle execution into an
//! argument. Aliases never classify: only literal subcommand names are
//! admitted, and user aliases cannot shadow them.

use brush_parser::ast::{Command, CommandPrefixOrSuffixItem, Program, SeparatorOperator};
use brush_parser::{Parser, ParserOptions};

/// Characters that disqualify a word outright. `$` and backtick enable
/// expansion and command substitution (i.e. execution); quotes and
/// backslashes mean the raw word text no longer maps one-to-one onto the
/// argv the shell would build. Admissible git arguments (paths, revisions,
/// flags) never need them.
const FORBIDDEN_WORD_CHARS: [char; 5] = ['$', '`', '"', '\'', '\\'];

/// git subcommands whose entire flag space is read-only. `reflog` is
/// deliberately absent (its `expire`/`delete` subverbs write) — it is
/// constrained below alongside `stash`.
const READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "log",
    "whatchanged",
    "show",
    "diff",
    "status",
    "blame",
    "annotate",
    "rev-parse",
    "rev-list",
    "cat-file",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "describe",
    "shortlog",
    "merge-base",
    "for-each-ref",
    "grep",
    "var",
    "check-ignore",
    "name-rev",
];

/// Flag-only forms admitted for `git branch` (value-taking filters like
/// `--contains <ref>` are out; `for-each-ref` covers them). Editor-spawning
/// flags such as `--edit-description` never appear here.
const BRANCH_LIST_FLAGS: &[&str] = &[
    "-a",
    "--all",
    "-v",
    "-vv",
    "--verbose",
    "-r",
    "--remotes",
    "--show-current",
];

/// True when `command` is a single read-only `git` invocation.
pub fn is_read_only_git_command(command: &str) -> bool {
    if command.trim().is_empty() {
        return false;
    }
    let program = match Parser::new(command.as_bytes(), &ParserOptions::default()).parse_program() {
        Ok(program) => program,
        Err(_) => return false,
    };
    let Some(argv) = simple_command_argv(&program) else {
        return false;
    };
    classify_git_argv(&argv)
}

/// Extract the argv of the program's single simple command, or `None` for
/// any compound structure. This is the structural gate: everything the
/// shell could execute beyond one plain command (lists, pipelines,
/// redirections, prefixes, substitutions) collapses to `None`.
fn simple_command_argv(program: &Program) -> Option<Vec<String>> {
    // Exactly one complete command of exactly one and-or item, sequenced
    // (`&` backgrounds are rejected along with every other Async separator).
    let [complete] = program.complete_commands.as_slice() else {
        return None;
    };
    let [item] = complete.0.as_slice() else {
        return None;
    };
    if !matches!(item.1, SeparatorOperator::Sequence) {
        return None;
    }
    let and_or = &item.0;
    if !and_or.additional.is_empty() {
        return None;
    }
    let pipeline = &and_or.first;
    if pipeline.bang || pipeline.timed.is_some() {
        return None;
    }
    let [command] = pipeline.seq.as_slice() else {
        return None;
    };
    let Command::Simple(simple) = command else {
        return None;
    };
    Some(simple_command_words(simple))
}

/// Collect the word list of a simple command; every non-word prefix/suffix
/// item (assignment, redirect, process substitution) disqualifies by
/// yielding an empty vec, which no `git` argv can be.
fn simple_command_words(simple: &brush_parser::ast::SimpleCommand) -> Vec<String> {
    let mut words = Vec::new();
    if let Some(name) = &simple.word_or_name {
        words.push(name.value.clone());
    }
    let prefix_items = simple.prefix.iter().map(|p| &p.0);
    let suffix_items = simple.suffix.iter().map(|s| &s.0);
    for group in prefix_items.chain(suffix_items) {
        for item in group {
            match item {
                CommandPrefixOrSuffixItem::Word(word) => words.push(word.value.clone()),
                CommandPrefixOrSuffixItem::IoRedirect(_)
                | CommandPrefixOrSuffixItem::AssignmentWord(_, _)
                | CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                    return Vec::new();
                }
            }
        }
    }
    words
}

/// Admission rules over the parsed argv.
fn classify_git_argv(argv: &[String]) -> bool {
    if argv.first().map(String::as_str) != Some("git") {
        return false;
    }
    if argv
        .iter()
        .any(|word| word.chars().any(|c| FORBIDDEN_WORD_CHARS.contains(&c)))
    {
        return false;
    }
    // Global flags before the subcommand: repository selection and pager
    // opt-out only. Everything else — notably `-c` (config injection can
    // spawn configured helpers) and `--exec-path` (relocates subcommand
    // lookup to an arbitrary directory) — is rejected.
    let mut idx = 1;
    while let Some(arg) = argv.get(idx).map(String::as_str) {
        if !arg.starts_with('-') {
            break;
        }
        match arg {
            "-C" => {
                idx += 2; // consumes the following path word
            }
            "--no-pager" | "--literal-pathspecs" => {
                idx += 1;
            }
            _ if arg.starts_with("-C")
                || arg.starts_with("--git-dir=")
                || arg.starts_with("--work-tree=") =>
            {
                idx += 1;
            }
            _ => return false,
        }
    }
    let Some(subcommand) = argv.get(idx).map(String::as_str) else {
        return false;
    };
    let rest = &argv[idx + 1..];
    match subcommand {
        s if READ_ONLY_SUBCOMMANDS.contains(&s) => {
            // `git diff --output=<file>` and friends write to an arbitrary
            // path even though the subcommand itself reads.
            !rest.iter().any(|arg| arg.starts_with("--output"))
        }
        // `git branch <name>` creates a branch; only flag-only listing
        // forms classify.
        "branch" => rest
            .iter()
            .all(|arg| BRANCH_LIST_FLAGS.contains(&arg.as_str())),
        // `git tag <name>` creates a tag; `-l`/`--list` forces list mode.
        "tag" => rest.iter().any(|arg| arg == "-l" || arg == "--list"),
        // Bare `git stash` stashes; `expire`/`drop`/`pop` write.
        "stash" => matches!(
            rest.first().map(String::as_str),
            Some("list") | Some("show")
        ),
        // Only listing forms; `add`/`prune`/`remove` write.
        "worktree" => rest.first().map(String::as_str) == Some("list"),
        // Flag-only listing (`-v`) or the read-only `show`/`get-url`
        // subverbs; `add`/`rename`/`set-url` write config.
        "remote" => {
            rest.iter().all(|arg| arg.starts_with('-'))
                || matches!(
                    rest.first().map(String::as_str),
                    Some("show") | Some("get-url")
                )
        }
        // `git config <k> <v>` writes; a leading `--get*`/`-l`/`--list`
        // puts git into read mode for the whole invocation.
        "config" => matches!(
            rest.first().map(String::as_str),
            Some("--get") | Some("--get-all") | Some("--get-regexp") | Some("-l") | Some("--list")
        ),
        // `git reflog expire`/`delete` prune; bare/show/exists read.
        "reflog" => match rest.first().map(String::as_str) {
            None => true,
            Some("show") | Some("exists") => true,
            Some(_) => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_read_only_git_command;

    #[test]
    fn admits_read_only_git() {
        let cases = [
            "git log",
            "git log --oneline -n 20",
            "git log --oneline -- crates/manox",
            "git -C ../manox log",
            "git -C../manox log",
            "git -C ~/proj status",
            "git --git-dir=/x/.git status",
            "git --work-tree=/x --no-pager diff",
            "git --literal-pathspecs grep TODO",
            "git status",
            "git diff HEAD~1",
            "git show abc123",
            "git blame src/main.rs",
            "git rev-parse HEAD",
            "git cat-file -p HEAD:README.md",
            "git stash list",
            "git stash show -p",
            "git config --get user.name",
            "git config -l",
            "git branch -a",
            "git branch -vv",
            "git tag -l",
            "git tag --list",
            "git remote -v",
            "git remote show origin",
            "git remote get-url origin",
            "git worktree list",
            "git reflog",
            "git reflog show HEAD",
            // Cosmetic shell forms: leading whitespace, trailing separator,
            // trailing newline, and a trailing comment.
            "  git status",
            "git log;",
            "git log\n",
            "git log # survey recent history",
        ];
        for case in cases {
            assert!(is_read_only_git_command(case), "should admit: {case:?}");
        }
    }

    #[test]
    fn rejects_write_and_probe_forms() {
        let cases = [
            "",
            "   ",
            "ls",
            "/usr/bin/git log",
            "env git log",
            // Env-assignment prefix.
            "GIT_DIR=/x git log",
            // No subcommand / dangling -C.
            "git",
            "git -C",
            // Mutating subcommands.
            "git commit -m x",
            "git push",
            "git checkout -b topic",
            "git -C /x stash",
            // Disallowed global flags: -c config injection, --exec-path
            // relocation, unknown flags, forced pager.
            "git -c core.pager=cat log",
            "git --exec-path=/tmp log",
            "git --paginate log",
            // Argument-sensitive subcommands in their writing forms.
            "git branch new-branch",
            "git branch --contains main",
            "git tag v1.0",
            "git stash",
            "git stash pop",
            "git config user.name New",
            "git config a b",
            "git worktree add ../x",
            "git remote add origin url",
            "git remote update",
            "git reflog expire --all",
            "git reflog delete HEAD@{0}",
            // File-writing flags on read subcommands.
            "git diff --output=/tmp/x",
            "git log --output=/tmp/x",
            // Compound constructs.
            "git log | head -5",
            "git log > out.txt",
            "git log; rm -rf /",
            "git log && git status",
            "git log &",
            "git log\ncat /etc/passwd",
            "(git log)",
            "! git log",
            "cd /x && git log",
            // Expansion / substitution / quoting inside words.
            "git log $(rm -rf /)",
            "git log `rm -rf /`",
            "git \"log\"",
            "git 'log'",
            "git log --pretty=format:\"%h\"",
        ];
        for case in cases {
            assert!(!is_read_only_git_command(case), "should reject: {case:?}");
        }
    }
}
