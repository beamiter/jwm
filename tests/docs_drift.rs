//! Drift checks between the user-facing documentation and the code it
//! describes.
//!
//! The docs are copied into scripts and bar configs verbatim, so an example
//! that names a client binary that does not ship, an IPC name the server does
//! not register, a `jwm-tool perf` flag the CLI does not take, or a chord no
//! default binding uses fails for the reader the first time it is tried.
//! These checks read README.md and every `docs/*.md` page and hold them to
//! what the crate actually exposes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use jwm::config::Config;
use jwm::ipc::IPC_REGISTRY;

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A user-facing Markdown page and its text.
struct Doc {
    path: PathBuf,
    text: String,
}

impl Doc {
    fn name(&self) -> String {
        self.path
            .strip_prefix(repository_root())
            .unwrap_or(&self.path)
            .display()
            .to_string()
    }

    /// Every line with its 1-based number, for pointing at a violation.
    fn lines(&self) -> impl Iterator<Item = (usize, &str)> {
        self.text
            .lines()
            .enumerate()
            .map(|(index, line)| (index + 1, line))
    }
}

fn read_doc(path: &Path) -> Doc {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    Doc {
        path: path.to_path_buf(),
        text,
    }
}

/// README.md plus every `docs/*.md`, in a stable order.
fn user_docs() -> Vec<Doc> {
    let root = repository_root();
    let mut paths = vec![root.join("README.md")];
    let mut pages: Vec<PathBuf> = fs::read_dir(root.join("docs"))
        .expect("the docs directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    pages.sort();
    paths.extend(pages);
    let docs: Vec<Doc> = paths.iter().map(|path| read_doc(path)).collect();
    assert!(
        docs.len() > 10,
        "expected README.md and the docs pages, found {}",
        docs.len()
    );
    docs
}

fn doc(relative: &str) -> Doc {
    read_doc(&repository_root().join(relative))
}

/// Every name the IPC server answers: bindable dispatch commands, the
/// commands `handle_ipc_command` implements itself, and queries.
fn registered_ipc_names() -> BTreeSet<&'static str> {
    IPC_REGISTRY
        .dispatch_commands
        .iter()
        .chain(IPC_REGISTRY.special_commands)
        .chain(IPC_REGISTRY.queries)
        .copied()
        .collect()
}

/// The text of the `## heading` section, up to the next `## ` heading.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let marker = format!("\n## {heading}\n");
    let (_, rest) = text
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing section `## {heading}`"));
    rest.split_once("\n## ").map_or(rest, |(body, _)| body)
}

/// Everything outside the `## heading` section, which is dropped.
fn without_section(text: &str, heading: &str) -> String {
    let marker = format!("\n## {heading}\n");
    match text.split_once(&marker) {
        Some((before, rest)) => match rest.split_once("\n## ") {
            Some((_, after)) => format!("{before}\n## {after}"),
            None => before.to_string(),
        },
        None => text.to_string(),
    }
}

/// Backticked spans that consist of one `get_…`/`set_…` identifier — the
/// shape every IPC query and most IPC setters take.
fn backticked_accessors(text: &str) -> Vec<&str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| {
            (span.starts_with("get_") || span.starts_with("set_"))
                && span
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

#[test]
fn no_doc_invokes_a_client_binary_that_does_not_ship() {
    // Only jwm-tool, jwm-support and jwm-remote are built; the IPC client is
    // `jwm-tool msg`.
    let mut violations = Vec::new();
    for doc in user_docs() {
        for (number, line) in doc.lines() {
            if line.contains("jwm-msg") {
                violations.push(format!("{}:{number}: {}", doc.name(), line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "`jwm-msg` does not exist; use `jwm-tool msg <name> --args '<json>'`:\n{}",
        violations.join("\n")
    );
}

#[test]
fn ipc_queries_are_documented_through_jwm_tool_msg() {
    // `jwm-tool get_tearing_hints` is an unrecognized clap subcommand; IPC
    // queries go through the `msg` subcommand.
    let mut violations = Vec::new();
    for doc in user_docs() {
        for (number, line) in doc.lines() {
            if line.contains("jwm-tool get_") {
                violations.push(format!("{}:{number}: {}", doc.name(), line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "IPC queries must be spelled `jwm-tool msg get_…`:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_jwm_tool_msg_example_names_a_registered_ipc_command_or_query() {
    let registered = registered_ipc_names();
    let mut checked = 0;
    let mut violations = Vec::new();
    for doc in user_docs() {
        for (number, line) in doc.lines() {
            for (at, needle) in line.match_indices("jwm-tool msg") {
                let rest = &line[at + needle.len()..];
                if !rest.starts_with(char::is_whitespace) {
                    continue;
                }
                let Some(token) = rest.split_whitespace().next() else {
                    continue;
                };
                let name = token.trim_matches(|c: char| "`'\",.;:()".contains(c));
                // `''` is the subscribe form, `<name>` a placeholder, `--…` a
                // flag, and a trailing `\` a shell line continuation.
                if name.is_empty()
                    || name.starts_with('<')
                    || name.starts_with('-')
                    || name.starts_with('\\')
                {
                    continue;
                }
                checked += 1;
                if !registered.contains(name) {
                    violations.push(format!("{}:{number}: `{name}`", doc.name()));
                }
            }
        }
    }
    assert!(
        checked > 20,
        "expected the docs to carry jwm-tool msg examples, checked {checked}"
    );
    assert!(
        violations.is_empty(),
        "these `jwm-tool msg` examples name no registered IPC command or query:\n{}",
        violations.join("\n")
    );
}

#[test]
fn vrr_and_hdr_docs_name_only_registered_ipc_accessors() {
    // `set_vrr_enabled` and `get_outputs` were documented as IPC calls that
    // never existed. The code map at the end of hdr.md names Rust functions,
    // not IPC, and is left out.
    let registered = registered_ipc_names();
    let compatibility = doc("docs/compatibility.md");
    let hdr = doc("docs/hdr.md");
    let hdr_prose = without_section(&hdr.text, "Where it lives");
    let scopes = [
        (
            "docs/compatibility.md (VRR)",
            section(&compatibility.text, "Variable refresh rate (VRR)"),
        ),
        ("docs/hdr.md", hdr_prose.as_str()),
    ];
    let mut violations = Vec::new();
    for (name, text) in scopes {
        for accessor in backticked_accessors(text) {
            if !registered.contains(accessor) {
                violations.push(format!("{name}: `{accessor}`"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "these backticked accessors are not registered IPC names:\n{}",
        violations.join("\n")
    );
}

/// What `jwm-tool perf <action>` accepts, as its clap help states it.
#[derive(Debug, Default)]
struct PerfUsage {
    /// Every long and short flag, and whether it takes a value.
    flags: BTreeMap<String, bool>,
    required_positionals: usize,
    /// `usize::MAX` when the last positional is variadic.
    max_positionals: usize,
}

/// Read the `Usage:` line and the option list of a clap help page.
fn parse_perf_usage(help: &str) -> PerfUsage {
    let mut usage = PerfUsage::default();
    for line in help.lines() {
        let line = line.trim();
        if let Some(synopsis) = line.strip_prefix("Usage:") {
            for token in synopsis.split_whitespace() {
                let variadic = token.ends_with("...");
                if token.starts_with('<') {
                    usage.required_positionals += 1;
                } else if !token.starts_with('[') || token == "[OPTIONS]" {
                    continue;
                }
                usage.max_positionals = if variadic {
                    usize::MAX
                } else {
                    usage.max_positionals.saturating_add(1)
                };
            }
        } else if line.starts_with('-') {
            // `-h, --help  Print help`, `--out <FILE>  …`: the flag spec ends
            // at the first run of two spaces, where the description starts.
            let spec = line.split("  ").next().unwrap_or(line);
            let tokens: Vec<&str> = spec.split_whitespace().collect();
            for (index, token) in tokens.iter().enumerate() {
                if token.starts_with('-') {
                    let takes_value = tokens
                        .get(index + 1)
                        .is_some_and(|next| next.starts_with('<') || next.starts_with('['));
                    usage
                        .flags
                        .insert(token.trim_end_matches(',').to_string(), takes_value);
                }
            }
        }
    }
    usage
}

/// `jwm-tool perf <action> --help`. clap answers `--help` before `main` reads
/// anything else, and the runtime directory points below a regular file, so
/// no change to the CLI can make this reach a live session's IPC socket.
///
/// The help is read as plain text: clap styles it even into a pipe when the
/// caller's environment forces colour (`CLICOLOR_FORCE`), and the escape
/// codes hide the `Usage:` line and every flag from [`parse_perf_usage`].
/// `NO_COLOR` wins over a forced colour.
fn perf_help_command(action: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jwm-tool"));
    command
        .args(["perf", action, "--help"])
        .env("XDG_RUNTIME_DIR", "/dev/null/jwm-docs-drift")
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

/// What `command` prints, or `None` when it fails (clap rejects the action).
fn help_output(mut command: Command) -> Option<String> {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run jwm-tool: {error}"));
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The clap help of `jwm-tool perf <action>`, or `None` when clap rejects the
/// action.
fn perf_action_help(action: &str) -> Option<String> {
    help_output(perf_help_command(action))
}

/// One `jwm-tool perf <action> …` a doc spells out.
#[derive(Debug, PartialEq, Eq)]
struct PerfInvocation {
    line: usize,
    /// A fenced code block is a command to run; a backticked span in prose
    /// may just name the action.
    in_code_block: bool,
    action: String,
    args: Vec<String>,
}

/// Where a shell command line ends: a comment, a pipe, a list operator or a
/// redirection.
fn ends_shell_command(token: &str) -> bool {
    token.starts_with('#')
        || token.starts_with('>')
        || token.starts_with("2>")
        || matches!(token, "|" | "||" | "&&" | ";" | "&" | "<")
}

/// Every `jwm-tool perf <action>` in `text`, with the arguments that follow it
/// up to the end of its backticked span or its shell command. A fenced line
/// ending in `\` continues on the next line.
fn perf_invocations(text: &str) -> Vec<PerfInvocation> {
    let lines: Vec<&str> = text.lines().collect();
    let mut invocations = Vec::new();
    let mut in_code_block = false;
    let mut index = 0;
    while index < lines.len() {
        let number = index + 1;
        let mut line = lines[index].to_string();
        index += 1;
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        while in_code_block && line.trim_end().ends_with('\\') && index < lines.len() {
            let continued = line.trim_end().len() - 1;
            line.truncate(continued);
            line.push(' ');
            line.push_str(lines[index]);
            index += 1;
        }
        for (at, needle) in line.match_indices("jwm-tool perf") {
            let rest = &line[at + needle.len()..];
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let in_span = line[..at].matches('`').count() % 2 == 1;
            let rest = if in_span {
                rest.split('`').next().unwrap_or_default()
            } else {
                rest
            };
            let mut tokens = rest
                .split_whitespace()
                .take_while(|token| !ends_shell_command(token));
            let Some(action) = tokens.next() else {
                continue;
            };
            let action = action.trim_matches(|c: char| "'\",.;:()".contains(c));
            // `<action>` is a placeholder.
            if action.is_empty() || action.starts_with('<') {
                continue;
            }
            invocations.push(PerfInvocation {
                line: number,
                in_code_block,
                action: action.to_string(),
                args: tokens.map(str::to_string).collect(),
            });
        }
    }
    invocations
}

/// Why `usage` rejects `invocation`, if it does.
fn perf_invocation_errors(invocation: &PerfInvocation, usage: &PerfUsage) -> Vec<String> {
    let mut errors = Vec::new();
    let mut positionals = 0;
    let mut args = invocation.args.iter();
    while let Some(arg) = args.next() {
        if !arg.starts_with('-') || arg == "-" {
            positionals += 1;
            continue;
        }
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg.as_str(), false),
        };
        match usage.flags.get(name) {
            None => errors.push(format!("unknown flag `{name}`")),
            Some(true) if !inline_value => {
                args.next();
            }
            Some(_) => {}
        }
    }
    if positionals > usage.max_positionals {
        errors.push(format!(
            "{positionals} positional argument(s), at most {} accepted",
            usage.max_positionals
        ));
    }
    // A prose span that names only the action is a mention, not a command.
    let is_command = invocation.in_code_block || !invocation.args.is_empty();
    if is_command && positionals < usage.required_positionals {
        errors.push(format!(
            "{positionals} positional argument(s), {} required",
            usage.required_positionals
        ));
    }
    errors
}

#[test]
fn every_jwm_tool_perf_example_parses_against_the_cli() {
    // `perf record --output` and `perf compare --baseline … --candidate …`
    // were documented although clap takes only `--out` and two positional
    // paths, so the copied command failed with "unexpected argument".
    let mut usages: BTreeMap<String, Option<PerfUsage>> = BTreeMap::new();
    let mut checked = 0;
    let mut violations = Vec::new();
    for doc in user_docs() {
        for invocation in perf_invocations(&doc.text) {
            checked += 1;
            let usage = usages.entry(invocation.action.clone()).or_insert_with(|| {
                perf_action_help(&invocation.action).map(|help| parse_perf_usage(&help))
            });
            let location = format!("{}:{}", doc.name(), invocation.line);
            let Some(usage) = usage else {
                violations.push(format!(
                    "{location}: `jwm-tool perf {}` is not a perf subcommand",
                    invocation.action
                ));
                continue;
            };
            for error in perf_invocation_errors(&invocation, usage) {
                violations.push(format!(
                    "{location}: `jwm-tool perf {} {}`: {error}",
                    invocation.action,
                    invocation.args.join(" ")
                ));
            }
        }
    }
    assert!(
        checked > 5,
        "expected the docs to carry jwm-tool perf examples, checked {checked}"
    );
    assert!(
        usages
            .values()
            .flatten()
            .any(|usage| usage.flags.contains_key("--help")),
        "the perf help pages were not parsed: {usages:?}"
    );
    assert!(
        violations.is_empty(),
        "these `jwm-tool perf` examples do not parse (see `jwm-tool perf <action> --help`):\n{}",
        violations.join("\n")
    );
}

/// Regression: the help inherited the caller's colour settings, and with
/// `CLICOLOR_FORCE=1` exported (common for coloured CI logs) clap wrapped
/// `Usage:` and every flag in escape codes, so nothing parsed and the perf
/// example check failed although the docs were right.
#[test]
fn perf_help_is_read_as_plain_text_even_when_colour_is_forced() {
    let mut command = perf_help_command("compare");
    command.env("CLICOLOR_FORCE", "1");
    let help = help_output(command).expect("`jwm-tool perf compare --help` succeeds");
    assert!(!help.contains('\x1b'), "the help is styled: {help:?}");
    let usage = parse_perf_usage(&help);
    assert_eq!(usage.flags.get("--help"), Some(&false), "{help}");
    assert_eq!(usage.required_positionals, 2, "{help}");
}

#[test]
fn perf_example_checks_read_clap_help_and_doc_spellings() {
    let usage = parse_perf_usage(
        "Compare a candidate against a baseline\n\
         \n\
         Usage: jwm-tool perf compare [OPTIONS] <BASELINE> <CANDIDATE>\n\
         \n\
         Arguments:\n  <BASELINE>   Baseline JSON\n  <CANDIDATE>  Candidate JSON\n\
         \n\
         Options:\n      --out <FILE>  Output file\n      --json  Emit JSON\n  -h, --help  Print help\n",
    );
    assert_eq!(usage.required_positionals, 2);
    assert_eq!(usage.max_positionals, 2);
    assert_eq!(usage.flags.get("--out"), Some(&true));
    assert_eq!(usage.flags.get("--json"), Some(&false));
    assert_eq!(usage.flags.get("-h"), Some(&false));
    assert_eq!(usage.flags.get("--help"), Some(&false));
    assert!(!usage.flags.contains_key("--output"));

    let doc = "Run `jwm-tool perf compare` after `jwm-tool perf <action>`.\n\
               \n\
               ```bash\n\
               jwm-tool perf compare --baseline a.json \\\n  --candidate b.json\n\
               jwm-tool perf compare a.json b.json --json  # gate\n\
               jwm-tool perf compare --out=x.json a.json | tee log\n\
               jwm-tool perf compare\n\
               ```\n";
    let invocations = perf_invocations(doc);
    let strings = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    assert_eq!(
        invocations,
        vec![
            PerfInvocation {
                line: 1,
                in_code_block: false,
                action: "compare".into(),
                args: Vec::new(),
            },
            PerfInvocation {
                line: 4,
                in_code_block: true,
                action: "compare".into(),
                args: strings(&["--baseline", "a.json", "--candidate", "b.json"]),
            },
            PerfInvocation {
                line: 6,
                in_code_block: true,
                action: "compare".into(),
                args: strings(&["a.json", "b.json", "--json"]),
            },
            PerfInvocation {
                line: 7,
                in_code_block: true,
                action: "compare".into(),
                args: strings(&["--out=x.json", "a.json"]),
            },
            PerfInvocation {
                line: 8,
                in_code_block: true,
                action: "compare".into(),
                args: Vec::new(),
            },
        ]
    );
    let errors: Vec<Vec<String>> = invocations
        .iter()
        .map(|invocation| perf_invocation_errors(invocation, &usage))
        .collect();
    // The prose mention and the valid command pass.
    assert!(errors[0].is_empty(), "{:?}", errors[0]);
    assert!(errors[2].is_empty(), "{:?}", errors[2]);
    assert_eq!(
        errors[1],
        vec![
            "unknown flag `--baseline`".to_string(),
            "unknown flag `--candidate`".to_string(),
        ]
    );
    // `--out=x.json` consumes no positional; one path is missing.
    assert_eq!(
        errors[3],
        vec!["1 positional argument(s), 2 required".to_string()]
    );
    assert_eq!(
        errors[4],
        vec!["0 positional argument(s), 2 required".to_string()]
    );
}

/// A chord as the docs spell it (`Ctrl+Alt+Shift+B`) or as the config does
/// (`["Mod1", "Control", "Shift"]` + `b`), normalized for comparison.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Chord {
    modifiers: BTreeSet<&'static str>,
    key: String,
}

fn modifier_name(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "alt" | "mod1" => Some("alt"),
        "ctrl" | "control" => Some("ctrl"),
        "shift" => Some("shift"),
        "super" | "mod4" => Some("super"),
        _ => None,
    }
}

/// Parse a backticked doc span such as `Alt+F12`; `None` for anything that is
/// not a modifier chord.
fn doc_chord(span: &str) -> Option<Chord> {
    let parts: Vec<&str> = span.split('+').collect();
    let (key, modifiers) = parts.split_last()?;
    if modifiers.is_empty() || key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let modifiers = modifiers
        .iter()
        .map(|name| modifier_name(name))
        .collect::<Option<BTreeSet<_>>>()?;
    Some(Chord {
        modifiers,
        key: key.to_ascii_lowercase(),
    })
}

fn default_binding_chords() -> BTreeSet<Chord> {
    Config::default()
        .key_configs()
        .iter()
        .filter_map(|binding| {
            let modifiers = binding
                .modifier
                .iter()
                .map(|name| modifier_name(name))
                .collect::<Option<BTreeSet<_>>>()?;
            Some(Chord {
                modifiers,
                key: binding.key.to_ascii_lowercase(),
            })
        })
        .collect()
}

#[test]
fn control_center_chords_are_default_bindings() {
    let control_center = doc("docs/control-center.md");
    // The Bluetooth picker once documented `Alt+Ctrl+F12` as its close key;
    // no default binding uses that chord in either spelling.
    for stale in ["Alt+Ctrl+F12", "Ctrl+Alt+F12"] {
        assert!(
            !control_center.text.contains(stale),
            "docs/control-center.md names `{stale}`, which no default binding uses"
        );
    }

    // Every window-manager chord the page names (one with Alt, Ctrl or Super;
    // `Shift+Tab` alone is in-panel navigation) must be a default binding.
    let bindings = default_binding_chords();
    assert!(
        !bindings.is_empty(),
        "the default config carries key bindings"
    );
    let mut checked = 0;
    let mut violations = Vec::new();
    for span in control_center.text.split('`').skip(1).step_by(2) {
        let Some(chord) = doc_chord(span) else {
            continue;
        };
        if chord.modifiers.iter().all(|modifier| *modifier == "shift") {
            continue;
        }
        checked += 1;
        if !bindings.contains(&chord) {
            violations.push(format!("`{span}`"));
        }
    }
    assert!(
        checked > 3,
        "expected docs/control-center.md to name its chords, checked {checked}"
    );
    assert!(
        violations.is_empty(),
        "docs/control-center.md names chords no default binding uses: {}",
        violations.join(", ")
    );
}

#[test]
fn chord_parsing_matches_doc_and_config_spellings() {
    let doc = doc_chord("Ctrl+Alt+Shift+B").expect("a chord");
    let config = Chord {
        modifiers: ["Mod1", "Control", "Shift"]
            .iter()
            .filter_map(|name| modifier_name(name))
            .collect(),
        key: "b".into(),
    };
    assert_eq!(doc, config);
    assert_eq!(doc_chord("Esc"), None);
    assert_eq!(doc_chord("a+b"), None);
    assert_eq!(doc_chord("Alt+"), None);
    assert_ne!(doc_chord("Alt+Ctrl+F12"), doc_chord("Alt+F12"));
}
