//! Shell-free command-line parsing shared by configuration and launchers.
//!
//! JWM stores some commands as strings for human-friendly configuration, but
//! always executes them as an argv vector.  Keeping this parser small and
//! explicit preserves quoted arguments without accidentally enabling pipes,
//! substitutions, redirects, or any other shell behavior.

use std::fmt;

/// Why a command string could not be converted into argv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandLineParseError {
    NulCharacter,
    TrailingEscape,
    UnterminatedQuote(char),
}

impl fmt::Display for CommandLineParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NulCharacter => f.write_str("command contains a NUL character"),
            Self::TrailingEscape => f.write_str("command ends with an unfinished escape"),
            Self::UnterminatedQuote(quote) => {
                write!(f, "command has an unterminated {quote} quote")
            }
        }
    }
}

impl std::error::Error for CommandLineParseError {}

/// Check argv before passing it to a process. Empty trailing arguments are valid.
pub(crate) fn valid_command_argv(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|program| !program.trim().is_empty())
        && argv.iter().all(|argument| !argument.contains('\0'))
}

/// Split one command line into argv without invoking or emulating a shell.
///
/// Single and double quotes preserve whitespace; backslashes escape the next
/// character outside quotes and inside double quotes. Shell operators remain
/// ordinary argv characters.
///
/// # Errors
///
/// Returns [`CommandLineParseError`] when a quote or trailing escape is left
/// unfinished, or when an argument contains a NUL that cannot be passed to a
/// process.
pub fn split_command_line(input: &str) -> Result<Vec<String>, CommandLineParseError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut token_started = false;

    for ch in input.chars() {
        if ch == '\0' {
            return Err(CommandLineParseError::NulCharacter);
        }
        if escaped {
            current.push(ch);
            escaped = false;
            token_started = true;
            continue;
        }

        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
                token_started = true;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            } else {
                current.push(ch);
                token_started = true;
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                token_started = true;
            }
            '\\' => {
                escaped = true;
                token_started = true;
            }
            ch if ch.is_whitespace() => {
                if token_started {
                    args.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            _ => {
                current.push(ch);
                token_started = true;
            }
        }
    }

    if escaped {
        return Err(CommandLineParseError::TrailingEscape);
    }
    if let Some(quote) = quote {
        return Err(CommandLineParseError::UnterminatedQuote(quote));
    }
    if token_started {
        args.push(current);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_characters_are_rejected_before_process_launch() {
        for command in [
            "to\0ol",
            "tool value\0",
            "tool 'val\0ue'",
            "tool \"val\0ue\"",
            "tool \\\0",
        ] {
            assert_eq!(
                split_command_line(command),
                Err(CommandLineParseError::NulCharacter)
            );
        }
        assert_eq!(
            CommandLineParseError::NulCharacter.to_string(),
            "command contains a NUL character"
        );
    }

    #[test]
    fn quotes_escapes_and_empty_arguments_preserve_argv_boundaries() {
        assert_eq!(
            split_command_line(r#"tool "two words" 'three words' four\ five """#),
            Ok(vec![
                "tool".into(),
                "two words".into(),
                "three words".into(),
                "four five".into(),
                String::new(),
            ])
        );
    }

    #[test]
    fn shell_operators_are_only_arguments() {
        assert_eq!(
            split_command_line("tool | sh -c 'echo unsafe' > output"),
            Ok(vec![
                "tool".into(),
                "|".into(),
                "sh".into(),
                "-c".into(),
                "echo unsafe".into(),
                ">".into(),
                "output".into(),
            ])
        );
    }

    #[test]
    fn malformed_input_reports_the_exact_boundary_error() {
        assert_eq!(
            split_command_line("tool value\\"),
            Err(CommandLineParseError::TrailingEscape)
        );
        assert_eq!(
            split_command_line("tool 'value"),
            Err(CommandLineParseError::UnterminatedQuote('\''))
        );
        assert_eq!(split_command_line("   "), Ok(Vec::new()));
    }
}
