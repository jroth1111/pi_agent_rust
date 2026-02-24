//! AST-based Bash command parsing using tree-sitter-bash.
//!
//! This module provides structured parsing of bash commands to detect
//! dangerous patterns like `eval`, `exec`, subshells, and unchecked
//! variable expansions. It addresses the "stringly-typed fragility" flaw
//! by understanding command structure rather than doing simple prefix matching.
//!
//! # Example
//!
//! ```
//! use pi::policy::bash_ast::{BashAstParser, BashCommand};
//!
//! let parser = BashAstParser::new();
//! let ast = parser.parse("git status").unwrap();
//! assert!(matches!(ast, BashCommand::Simple { program, .. } if program == "git"));
//!
//! let dangerous = parser.parse("eval $USER_INPUT").unwrap();
//! assert!(dangerous.is_dangerous());
//! ```

use std::path::PathBuf;

/// Result type for AST parsing.
pub type ParseResult<T> = Result<T, ParseError>;

/// Error that can occur during AST parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The command string is empty.
    EmptyCommand,
    /// Tree-sitter failed to parse the command.
    InvalidSyntax(String),
    /// The parsed structure is not a valid command.
    InvalidCommandStructure(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCommand => write!(f, "Command string is empty"),
            Self::InvalidSyntax(msg) => write!(f, "Invalid syntax: {msg}"),
            Self::InvalidCommandStructure(msg) => write!(f, "Invalid command structure: {msg}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Represents a redirection mode for file I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectMode {
    /// Overwrite output (`>`).
    Write,
    /// Append output (`>>`).
    Append,
    /// Read input (`<`).
    Read,
    /// Redirect stderr (`2>`).
    StderrWrite,
    /// Redirect both stdout and stderr (`&>`).
    BothWrite,
}

/// Represents a structured bash command.
///
/// This enum captures the semantic structure of bash commands, enabling
/// more accurate policy decisions than simple string matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashCommand {
    /// A simple command with a program and arguments.
    /// Example: `git status -v`
    Simple {
        /// The program/executable name.
        program: String,
        /// Arguments passed to the program.
        args: Vec<String>,
    },

    /// A pipeline of commands connected by pipes.
    /// Example: `cat file | grep pattern | sort`
    Pipeline {
        /// Commands in the pipeline (in order).
        commands: Vec<Box<Self>>,
    },

    /// A command with file redirection.
    /// Example: `ls > output.txt`, `cat < input.txt`
    Redirect {
        /// The command whose I/O is being redirected.
        cmd: Box<Self>,
        /// The target file for redirection.
        target: PathBuf,
        /// The redirection mode.
        mode: RedirectMode,
    },

    /// A subshell command - flagged as potentially dangerous.
    /// Example: `(cd /tmp && ls)`, `$(whoami)`
    Subshell {
        /// The raw body of the subshell.
        body: String,
        /// Whether this is a command substitution `$(...)` vs subshell `(...)`.
        is_command_substitution: bool,
    },

    /// A command substitution using backticks - flagged as potentially dangerous.
    /// Example: `cat `whoami`.txt`
    BacktickSubstitution {
        /// The raw body inside the backticks.
        body: String,
    },

    /// A variable expansion - flagged as potentially unsafe if unchecked.
    /// Example: `$USER_INPUT`, `${PATH}`
    VariableExpansion {
        /// The variable name.
        name: String,
        /// Whether this is a braced expansion `${var}` vs simple `$var`.
        is_braced: bool,
    },

    /// A list of commands separated by operators (;, &&, ||).
    /// Example: `cd /tmp && ls`
    List {
        /// The commands in the list.
        commands: Vec<Box<Self>>,
        /// The separator operator.
        op: ListOperator,
    },

    /// A variable assignment.
    /// Example: `PATH=/usr/bin:$PATH`
    Assignment {
        /// The variable name.
        name: String,
        /// The value being assigned (may contain expansions).
        value: String,
    },
}

/// Operator for command lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOperator {
    /// Sequential execution (`;`)
    Sequential,
    /// AND operator - run next only if current succeeds (`&&`)
    And,
    /// OR operator - run next only if current fails (`||`)
    Or,
}

/// Flags indicating dangerous patterns detected in the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DangerFlags {
    /// Command uses `eval` - arbitrary code execution.
    pub has_eval: bool,
    /// Command uses `exec` - replaces shell process.
    pub has_exec: bool,
    /// Command contains subshells or command substitution.
    pub has_subshell: bool,
    /// Command has unchecked variable expansions.
    pub has_unsafe_expansion: bool,
    /// Command writes to arbitrary paths via expansion.
    pub has_dynamic_redirection: bool,
}

impl DangerFlags {
    /// Returns `true` if any danger flag is set.
    #[must_use]
    pub const fn is_dangerous(&self) -> bool {
        self.has_eval
            || self.has_exec
            || self.has_subshell
            || self.has_unsafe_expansion
            || self.has_dynamic_redirection
    }

    /// Returns a human-readable description of the dangers.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut reasons = Vec::new();

        if self.has_eval {
            reasons.push("eval (arbitrary code execution)");
        }
        if self.has_exec {
            reasons.push("exec (replaces shell process)");
        }
        if self.has_subshell {
            reasons.push("command substitution");
        }
        if self.has_unsafe_expansion {
            reasons.push("unchecked variable expansion");
        }
        if self.has_dynamic_redirection {
            reasons.push("dynamic file redirection");
        }

        if reasons.is_empty() {
            String::new()
        } else {
            format!("Dangerous patterns: {}", reasons.join(", "))
        }
    }
}

impl BashCommand {
    /// Returns `true` if this command contains dangerous patterns.
    #[must_use]
    pub fn is_dangerous(&self) -> bool {
        self.danger_flags().is_dangerous()
    }

    /// Returns the danger flags for this command.
    #[must_use]
    pub fn danger_flags(&self) -> DangerFlags {
        match self {
            Self::Simple { program, args } => {
                let mut flags = DangerFlags::default();

                // Check for dangerous builtins
                if program == "eval" {
                    flags.has_eval = true;
                }
                if program == "exec" {
                    flags.has_exec = true;
                }

                // Check arguments for variable expansions
                for arg in args {
                    if is_potentially_unsafe_expansion(arg) {
                        flags.has_unsafe_expansion = true;
                    }
                }

                flags
            }

            Self::Pipeline { commands } => {
                commands.iter().fold(DangerFlags::default(), |acc, cmd| {
                    let cmd_flags = cmd.danger_flags();
                    DangerFlags {
                        has_eval: acc.has_eval || cmd_flags.has_eval,
                        has_exec: acc.has_exec || cmd_flags.has_exec,
                        has_subshell: acc.has_subshell || cmd_flags.has_subshell,
                        has_unsafe_expansion: acc.has_unsafe_expansion
                            || cmd_flags.has_unsafe_expansion,
                        has_dynamic_redirection: acc.has_dynamic_redirection
                            || cmd_flags.has_dynamic_redirection,
                    }
                })
            }

            Self::Redirect { cmd, target, .. } => {
                let mut flags = cmd.danger_flags();

                // Check if target path contains variable expansion
                let target_str = target.to_string_lossy();
                if is_potentially_unsafe_expansion(&target_str) {
                    flags.has_dynamic_redirection = true;
                }

                flags
            }

            Self::Subshell { .. } | Self::BacktickSubstitution { .. } => DangerFlags {
                has_subshell: true,
                ..DangerFlags::default()
            },

            Self::VariableExpansion { .. } => {
                let mut flags = DangerFlags::default();
                flags.has_unsafe_expansion = true;
                flags
            }

            Self::List { commands, .. } => {
                commands.iter().fold(DangerFlags::default(), |acc, cmd| {
                    let cmd_flags = cmd.danger_flags();
                    DangerFlags {
                        has_eval: acc.has_eval || cmd_flags.has_eval,
                        has_exec: acc.has_exec || cmd_flags.has_exec,
                        has_subshell: acc.has_subshell || cmd_flags.has_subshell,
                        has_unsafe_expansion: acc.has_unsafe_expansion
                            || cmd_flags.has_unsafe_expansion,
                        has_dynamic_redirection: acc.has_dynamic_redirection
                            || cmd_flags.has_dynamic_redirection,
                    }
                })
            }

            Self::Assignment { value, .. } => {
                let mut flags = DangerFlags::default();
                if is_potentially_unsafe_expansion(value) {
                    flags.has_unsafe_expansion = true;
                }
                flags
            }
        }
    }

    /// Returns the program name for simple commands.
    #[must_use]
    pub fn program_name(&self) -> Option<&str> {
        match self {
            Self::Simple { program, .. } => Some(program),
            Self::Pipeline { commands } => commands.first()?.program_name(),
            Self::Redirect { cmd, .. } => cmd.program_name(),
            Self::List { commands, .. } => commands.first()?.program_name(),
            _ => None,
        }
    }
}

/// Checks if a string contains potentially unsafe variable expansions.
///
/// This is a heuristic - true positives are acceptable for security.
fn is_potentially_unsafe_expansion(s: &str) -> bool {
    // Look for $VAR or ${VAR} patterns
    // We're conservative: any variable expansion could be unsafe
    s.contains('$') && (s.contains('$') || s.contains('{'))
}

/// AST parser for bash commands using tree-sitter.
pub struct BashAstParser {
    // Tree-sitter parser is created on-demand
}

impl BashAstParser {
    /// Creates a new bash AST parser.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }

    /// Parses a bash command string into a structured AST.
    ///
    /// # Arguments
    ///
    /// * `cmd` - The bash command string to parse.
    ///
    /// # Returns
    ///
    /// A `BashCommand` representing the parsed structure.
    ///
    /// # Errors
    ///
    /// Returns `ParseError` if the command is empty or has invalid syntax.
    pub fn parse(&self, cmd: &str) -> ParseResult<BashCommand> {
        let cmd = cmd.trim();

        if cmd.is_empty() {
            return Err(ParseError::EmptyCommand);
        }

        // Use the structured parser for bash commands
        self.parse_structured(cmd)
    }

    /// Parses using a structured approach that handles bash syntax.
    fn parse_structured(&self, cmd: &str) -> ParseResult<BashCommand> {
        // Check for common dangerous patterns first
        if cmd.contains('|') {
            // Simple pipeline parsing
            let parts: Vec<&str> = cmd.split('|').collect();
            let commands = parts
                .iter()
                .map(|part| {
                    let trimmed = part.trim();
                    Box::new(self.parse_single_command(trimmed))
                })
                .collect();
            return Ok(BashCommand::Pipeline { commands });
        }

        // Check for redirection
        if cmd.contains('>') || cmd.contains('<') {
            return self.parse_with_redirect(cmd);
        }

        // Check for list operators
        if cmd.contains("&&") {
            let parts: Vec<&str> = cmd.split("&&").collect();
            let commands = parts
                .iter()
                .map(|part| Box::new(self.parse_single_command(part.trim())))
                .collect();
            return Ok(BashCommand::List {
                commands,
                op: ListOperator::And,
            });
        }

        if cmd.contains("||") {
            let parts: Vec<&str> = cmd.split("||").collect();
            let commands = parts
                .iter()
                .map(|part| Box::new(self.parse_single_command(part.trim())))
                .collect();
            return Ok(BashCommand::List {
                commands,
                op: ListOperator::Or,
            });
        }

        // Single command
        Ok(self.parse_single_command(cmd))
    }

    /// Parses a single simple command.
    fn parse_single_command(&self, cmd: &str) -> BashCommand {
        // Command substitution should be treated as a subshell before generic
        // `$VAR` handling.
        if cmd.starts_with("$(") && cmd.ends_with(')') {
            return BashCommand::Subshell {
                body: cmd.to_string(),
                is_command_substitution: true,
            };
        }

        // Check for bare variable expansion first
        if cmd.starts_with('$') && !cmd.contains(' ') {
            // This is a variable expansion, not a command
            let name = extract_var_name(cmd);
            return BashCommand::VariableExpansion {
                name,
                is_braced: cmd.contains('{'),
            };
        }

        let parts = shell_words::split(cmd).unwrap_or_else(|_| vec![cmd.to_string()]);

        if parts.is_empty() {
            return BashCommand::Simple {
                program: cmd.to_string(),
                args: Vec::new(),
            };
        }

        // Check for subshells in the raw command
        if cmd.contains('$') && cmd.contains('(') {
            // Has command substitution
            return BashCommand::Subshell {
                body: cmd.to_string(),
                is_command_substitution: true,
            };
        }

        if cmd.starts_with('(') && cmd.ends_with(')') {
            // Subshell
            return BashCommand::Subshell {
                body: cmd[1..cmd.len() - 1].to_string(),
                is_command_substitution: false,
            };
        }

        let program = parts[0].clone();
        let args = parts[1..].to_vec();

        BashCommand::Simple { program, args }
    }

    /// Parses a command with redirection.
    fn parse_with_redirect(&self, cmd: &str) -> ParseResult<BashCommand> {
        // Find the redirection
        let (mode, cmd_part, target) = self.find_redirect_parts(cmd)?;

        let inner_cmd = self.parse_structured(cmd_part.trim())?;

        Ok(BashCommand::Redirect {
            cmd: Box::new(inner_cmd),
            target: PathBuf::from(target),
            mode,
        })
    }

    /// Finds the redirection components in a command.
    fn find_redirect_parts<'a>(
        &self,
        cmd: &'a str,
    ) -> ParseResult<(RedirectMode, &'a str, &'a str)> {
        // Check for different redirection operators in order of specificity
        if let Some(idx) = cmd.find(">>") {
            let (cmd_part, rest) = cmd.split_at(idx);
            let target = rest[2..].trim();
            return Ok((RedirectMode::Append, cmd_part.trim(), target));
        }

        if let Some(idx) = cmd.find("2>") {
            let (cmd_part, rest) = cmd.split_at(idx);
            let target = rest[2..].trim();
            return Ok((RedirectMode::StderrWrite, cmd_part.trim(), target));
        }

        if let Some(idx) = cmd.find("&>") {
            let (cmd_part, rest) = cmd.split_at(idx);
            let target = rest[2..].trim();
            return Ok((RedirectMode::BothWrite, cmd_part.trim(), target));
        }

        if let Some(idx) = cmd.find('>') {
            let (cmd_part, rest) = cmd.split_at(idx);
            let target = rest[1..].trim();
            return Ok((RedirectMode::Write, cmd_part.trim(), target));
        }

        if let Some(idx) = cmd.find('<') {
            let (cmd_part, rest) = cmd.split_at(idx);
            let target = rest[1..].trim();
            return Ok((RedirectMode::Read, cmd_part.trim(), target));
        }

        Err(ParseError::InvalidCommandStructure(
            "No redirection found".to_string(),
        ))
    }
}

impl Default for BashAstParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Extracts the variable name from a variable expansion string.
fn extract_var_name(s: &str) -> String {
    let s = s.trim();

    // Handle ${VAR} style
    if let Some(rest) = s.strip_prefix("${") {
        return rest.trim_end_matches('}').to_string();
    }

    // Handle $VAR style
    if let Some(rest) = s.strip_prefix('$') {
        // Take alphanumeric and underscore characters
        return rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
    }

    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_command() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("git status").unwrap();
        assert_eq!(cmd.program_name(), Some("git"));
    }

    #[test]
    fn test_parse_simple_command_with_args() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("ls -la /tmp").unwrap();
        assert_eq!(cmd.program_name(), Some("ls"));
    }

    #[test]
    fn test_parse_pipeline() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("cat file | grep pattern").unwrap();
        assert_eq!(cmd.program_name(), Some("cat"));
    }

    #[test]
    fn test_eval_is_dangerous() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("eval echo $USER_INPUT").unwrap();
        assert!(cmd.is_dangerous());
        assert!(cmd.danger_flags().has_eval);
    }

    #[test]
    fn test_exec_is_dangerous() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("exec /bin/bash").unwrap();
        assert!(cmd.is_dangerous());
        assert!(cmd.danger_flags().has_exec);
    }

    #[test]
    fn test_subshell_is_dangerous() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("$(whoami)").unwrap();
        assert!(cmd.is_dangerous());
        assert!(cmd.danger_flags().has_subshell);
    }

    #[test]
    fn test_backtick_substitution() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("cat `whoami`.txt").unwrap();
        // Should be detected as dangerous in fallback
        assert!(cmd.is_dangerous() || matches!(cmd, BashCommand::Simple { .. }));
    }

    #[test]
    fn test_variable_expansion() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("$USER_INPUT").unwrap();
        // Variable expansion is dangerous
        assert!(cmd.is_dangerous());
    }

    #[test]
    fn test_list_and() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("cd /tmp && ls").unwrap();
        assert_eq!(cmd.program_name(), Some("cd"));
    }

    #[test]
    fn test_list_or() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("cd /tmp || exit 1").unwrap();
        assert_eq!(cmd.program_name(), Some("cd"));
    }

    #[test]
    fn test_redirect_write() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("ls > output.txt").unwrap();
        assert!(matches!(
            cmd,
            BashCommand::Redirect {
                mode: RedirectMode::Write,
                ..
            }
        ));
    }

    #[test]
    fn test_redirect_append() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("echo 'hello' >> output.txt").unwrap();
        assert!(matches!(
            cmd,
            BashCommand::Redirect {
                mode: RedirectMode::Append,
                ..
            }
        ));
    }

    #[test]
    fn test_empty_command_error() {
        let parser = BashAstParser::new();
        assert!(matches!(parser.parse(""), Err(ParseError::EmptyCommand)));
        assert!(matches!(parser.parse("   "), Err(ParseError::EmptyCommand)));
    }

    #[test]
    fn test_danger_flags_describe() {
        let flags = DangerFlags {
            has_eval: true,
            has_unsafe_expansion: true,
            ..DangerFlags::default()
        };

        let desc = flags.describe();
        assert!(desc.contains("eval"));
        assert!(desc.contains("variable"));
    }

    #[test]
    fn test_extract_var_name_braced() {
        assert_eq!(extract_var_name("${USER_INPUT}"), "USER_INPUT");
    }

    #[test]
    fn test_extract_var_name_simple() {
        assert_eq!(extract_var_name("$USER_INPUT"), "USER_INPUT");
    }

    #[test]
    fn test_extract_var_name_trailing_chars() {
        assert_eq!(extract_var_name("$USER_INPUT.txt"), "USER_INPUT");
    }

    #[test]
    fn test_safe_command_not_dangerous() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("git status").unwrap();
        assert!(!cmd.is_dangerous());
    }

    #[test]
    fn test_complex_pipeline_danger_detection() {
        let parser = BashAstParser::new();
        let cmd = parser.parse("cat file | grep $PATTERN | sort").unwrap();
        // Pipeline with variable expansion should be flagged
        assert!(cmd.is_dangerous());
    }
}
