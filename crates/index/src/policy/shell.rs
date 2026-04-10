use serde_json::Value;

use super::file_access::{
    CodeChangeSummary, ToolAccessKind, derive_apply_patch_file_accesses, push_access,
    summarize_apply_patch_changes,
};

/// Stores one shell-like command decoded from one tool-call payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    pub command_text: String,
    pub workdir: Option<String>,
}

/// Derives file accesses from one shell-like tool invocation.
pub(super) fn derive_shell_file_accesses(arguments_text: &str) -> Vec<(ToolAccessKind, String)> {
    let Some(command) = parse_shell_command(arguments_text) else {
        return Vec::new();
    };

    let mut accesses = Vec::new();
    for fragment in split_shell_fragments(&command.command_text) {
        accesses.extend(derive_shell_fragment_file_accesses(
            &fragment,
            command.workdir.as_deref(),
        ));
    }
    accesses
}

/// Extracts one shell-like command from one tool name plus arguments payload.
pub fn extract_shell_command(tool_name: &str, arguments_text: &str) -> Option<ShellCommand> {
    is_shell_tool_name(tool_name).then_some(())?;
    parse_shell_command(arguments_text)
}

/// Summarizes every apply-patch fragment embedded in one shell-like tool payload.
pub fn summarize_shell_code_changes(arguments_text: &str) -> CodeChangeSummary {
    let Some(command) = parse_shell_command(arguments_text) else {
        return CodeChangeSummary::default();
    };

    let mut summary = CodeChangeSummary::default();
    for fragment in split_shell_fragments(&command.command_text) {
        if fragment.contains("*** Begin Patch") {
            summary = summary.saturating_add(summarize_apply_patch_changes(&fragment));
        }
    }
    summary
}

/// Returns whether one tool name carries a shell command payload.
pub(super) fn is_shell_tool_name(name: &str) -> bool {
    matches!(name, "exec_command" | "shell_command" | "shell" | "Bash")
}

/// Parses one shell-like tool payload into one command plus optional workdir.
fn parse_shell_command(arguments_text: &str) -> Option<ShellCommand> {
    let parsed = serde_json::from_str::<Value>(arguments_text).ok();
    match parsed {
        Some(Value::Object(object)) => Some(ShellCommand {
            command_text: shell_command_text_from_value(
                object
                    .get("cmd")
                    .or_else(|| object.get("command"))
                    .or_else(|| object.get("script"))
                    .or_else(|| object.get("command_string"))?,
            )?,
            workdir: object
                .get("workdir")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        Some(Value::Array(values)) => Some(ShellCommand {
            command_text: shell_command_text_from_array(&values)?,
            workdir: None,
        }),
        Some(Value::String(command_text)) => Some(ShellCommand {
            command_text,
            workdir: None,
        }),
        _ => Some(ShellCommand {
            command_text: arguments_text.trim().to_owned(),
            workdir: None,
        }),
    }
    .filter(|invocation| !invocation.command_text.trim().is_empty())
}

/// Decodes one JSON command value into shell text.
fn shell_command_text_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(values) => shell_command_text_from_array(values),
        _ => None,
    }
}

/// Decodes one JSON string-array command into one shell command string.
fn shell_command_text_from_array(values: &[Value]) -> Option<String> {
    let parts = values
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    match parts.as_slice() {
        [shell, "-lc", command, ..] if is_shell_executable(shell) => Some((*command).to_owned()),
        _ => Some(parts.join(" ")),
    }
}

/// Splits one shell command string into top-level fragments.
fn split_shell_fragments(command_text: &str) -> Vec<String> {
    let mut fragments = Vec::new();
    let mut current = String::new();
    let mut chars = command_text.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    loop {
        let Some(ch) = chars.next() else {
            break;
        };
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single_quote => {
                current.push(ch);
                escaped = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                current.push(ch);
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                current.push(ch);
            }
            '\n' | ';' if !in_single_quote && !in_double_quote => {
                push_fragment(&mut fragments, &mut current);
            }
            '|' if !in_single_quote && !in_double_quote => {
                if chars.peek() == Some(&'|') {
                    let _ = chars.next();
                }
                push_fragment(&mut fragments, &mut current);
            }
            '&' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'&') => {
                let _ = chars.next();
                push_fragment(&mut fragments, &mut current);
            }
            _ => current.push(ch),
        }
    }

    push_fragment(&mut fragments, &mut current);
    fragments
}

/// Splits one shell fragment into shell words while preserving quoted text.
fn tokenize_shell_words(fragment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = fragment.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    loop {
        let Some(ch) = chars.next() else {
            break;
        };
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single_quote => escaped = true,
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            ch if ch.is_whitespace() && !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Derives file accesses from one already-split shell fragment.
fn derive_shell_fragment_file_accesses(
    fragment: &str,
    _workdir: Option<&str>,
) -> Vec<(ToolAccessKind, String)> {
    let fragment = fragment.trim();
    if fragment.is_empty() || fragment.starts_with('#') {
        return Vec::new();
    }
    if fragment.contains("apply_patch") && fragment.contains("*** Begin Patch") {
        return derive_apply_patch_file_accesses(fragment);
    }

    let tokens = tokenize_shell_words(fragment);
    let tokens = trim_shell_prefix_tokens(&tokens);
    if tokens.is_empty() {
        return Vec::new();
    }

    if is_shell_executable(tokens[0].as_str()) && tokens.get(1).is_some_and(|token| token == "-lc")
    {
        return tokens
            .get(2)
            .map(|command| derive_shell_file_accesses(command))
            .unwrap_or_default();
    }

    match tokens[0].as_str() {
        "apply_patch" => derive_apply_patch_file_accesses(fragment),
        "bash" | "sh" | "zsh" => extract_script_runner_file_accesses(tokens),
        "sed" => extract_sed_file_accesses(tokens),
        "rg" => extract_ripgrep_file_accesses(tokens),
        "grep" => extract_grep_file_accesses(tokens),
        "cat" => extract_cat_file_accesses(tokens),
        "ls" | "tree" => extract_simple_path_accesses(
            tokens,
            ToolAccessKind::List,
            &["-I", "-L", "--ignore", "--tree"],
        ),
        "find" => extract_find_file_accesses(tokens),
        "nl" => extract_simple_path_accesses(tokens, ToolAccessKind::Read, &["-s", "-w", "-v"]),
        "head" | "tail" => {
            extract_simple_path_accesses(tokens, ToolAccessKind::Read, &["-n", "-c"])
        }
        "cargo" => extract_cargo_file_accesses(tokens),
        "awk" | "jq" => extract_program_and_file_accesses(tokens, ToolAccessKind::Read),
        "node" | "python" | "python3" | "ruby" => extract_script_runner_file_accesses(tokens),
        "cp" => extract_copy_file_accesses(tokens),
        "mv" => extract_move_file_accesses(tokens),
        "rm" | "rmdir" | "chmod" | "chown" => {
            extract_simple_path_accesses(tokens, ToolAccessKind::Edit, &[])
        }
        "mkdir" | "touch" => extract_simple_path_accesses(tokens, ToolAccessKind::Write, &[]),
        "curl" => extract_output_option_file_accesses(tokens),
        "echo" | "printf" | ":" => extract_redirection_file_accesses(tokens),
        "source" | "." => tokens
            .get(1)
            .map(|path| vec![(ToolAccessKind::Read, path.clone())])
            .unwrap_or_default(),
        "test" | "[" => extract_test_file_accesses(tokens),
        "fd" => extract_fd_file_accesses(tokens),
        "wc" | "rustfmt" | "lsof" | "sort" | "stat" | "xxd" | "mdls" | "file" | "diff" => {
            extract_simple_path_accesses(tokens, ToolAccessKind::Read, &[])
        }
        "perl" => extract_perl_file_accesses(tokens),
        "ln" => extract_link_file_accesses(tokens),
        _ => Vec::new(),
    }
}

/// Trims wrapper tokens and shell keywords from the front of one shell fragment.
fn trim_shell_prefix_tokens(tokens: &[String]) -> &[String] {
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token.is_empty()
            || is_shell_keyword(token)
            || is_environment_assignment(token)
            || matches!(
                token,
                "env" | "command" | "builtin" | "noglob" | "nocorrect" | "time"
            )
        {
            index += 1;
            continue;
        }
        if token == "sudo" {
            index += 1;
            while index < tokens.len() && tokens[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        break;
    }
    &tokens[index..]
}

/// Extracts read or edit accesses from one sed command.
fn extract_sed_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut script_consumed = false;
    let mut skip_next = false;
    let access_type = if tokens
        .iter()
        .any(|token| token == "-i" || token.starts_with("-i"))
    {
        ToolAccessKind::Edit
    } else {
        ToolAccessKind::Read
    };

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-e" | "-f" => {
                skip_next = true;
            }
            "-i" => {
                skip_next = true;
            }
            _ if token.starts_with("-i") || token.starts_with('-') => {}
            _ if !script_consumed => {
                script_consumed = true;
            }
            _ => push_access(&mut accesses, access_type, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts read or list accesses from one ripgrep command.
fn extract_ripgrep_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let list_mode = tokens.iter().any(|token| token == "--files");
    let mut non_option_tokens = Vec::<&str>::new();
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "--glob" | "-g" | "--regexp" | "-e" | "--file" | "-f" | "--type" | "-t"
            | "--type-not" | "-T" | "--max-count" | "-m" | "--context" | "-C" | "-A" | "-B"
            | "--threads" | "-j" | "--sort" | "--sortr" => skip_next = true,
            _ if token.starts_with('-') => {}
            _ => non_option_tokens.push(token),
        }
        index += 1;
    }

    let path_tokens = if list_mode {
        non_option_tokens
    } else {
        non_option_tokens.into_iter().skip(1).collect()
    };
    let access_type = if list_mode {
        ToolAccessKind::List
    } else {
        ToolAccessKind::Read
    };
    for token in path_tokens {
        push_access(&mut accesses, access_type, token);
    }
    accesses
}

/// Extracts read accesses from one grep command.
fn extract_grep_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let mut non_option_tokens = Vec::<&str>::new();
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-e" | "-f" | "-m" | "-A" | "-B" | "-C" | "--include" | "--exclude" => {
                skip_next = true;
            }
            _ if token.starts_with('-') => {}
            _ => non_option_tokens.push(token),
        }
        index += 1;
    }

    for token in non_option_tokens.into_iter().skip(1) {
        push_access(&mut accesses, ToolAccessKind::Read, token);
    }
    accesses
}

/// Extracts read or write accesses from one cat command.
fn extract_cat_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            ">" | ">>" | "1>" | "1>>" | "<" | "<<" => {
                skip_next = true;
            }
            _ if token.starts_with(">")
                || token.starts_with(">>")
                || token.starts_with('<')
                || token.starts_with("<<") => {}
            _ if token.starts_with('-') => {}
            _ => push_access(&mut accesses, ToolAccessKind::Read, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts list accesses from one find command.
fn extract_find_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let mut saw_expression = false;

    for token in &tokens[1..] {
        let token = token.as_str();
        if token.starts_with('-') || matches!(token, "!" | "(" | ")" | "\\(" | "\\)") {
            saw_expression = true;
        }
        if saw_expression {
            continue;
        }
        push_access(&mut accesses, ToolAccessKind::List, token);
    }

    accesses
}

/// Extracts file accesses from commands whose non-option tokens are all paths.
fn extract_simple_path_accesses(
    tokens: &[String],
    access_type: ToolAccessKind,
    options_with_values: &[&str],
) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        if options_with_values.contains(&token) {
            skip_next = true;
        } else if token.starts_with('-') {
            // Ignore option flags.
        } else {
            push_access(&mut accesses, access_type, token);
        }
        index += 1;
    }

    accesses
}

/// Extracts script-file reads from shell and interpreter entrypoints.
fn extract_script_runner_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut index = 1;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token == "-" || token.starts_with("<<") {
            return Vec::new();
        }
        if token.starts_with('-') {
            if matches!(token, "-c" | "-m") {
                return Vec::new();
            }
            index += 1;
            continue;
        }
        return vec![(ToolAccessKind::Read, token.to_owned())];
    }
    Vec::new()
}

/// Extracts manifest and fmt target accesses from cargo commands.
fn extract_cargo_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let mut index = 1;
    let mut saw_fmt = false;
    let mut after_double_dash = false;

    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token == "fmt" {
            saw_fmt = true;
            index += 1;
            continue;
        }
        if token == "--manifest-path"
            && let Some(path) = tokens.get(index + 1)
        {
            push_access(&mut accesses, ToolAccessKind::Read, path);
            index += 2;
            continue;
        }
        if token == "--" {
            after_double_dash = true;
            index += 1;
            continue;
        }
        if after_double_dash && saw_fmt {
            push_access(&mut accesses, ToolAccessKind::Edit, token);
        }
        index += 1;
    }

    accesses
}

/// Extracts read accesses from commands that take a program plus path operands.
fn extract_program_and_file_accesses(
    tokens: &[String],
    access_type: ToolAccessKind,
) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut skip_next = false;
    let mut program_consumed = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-f" => {
                skip_next = true;
            }
            _ if token.starts_with('-') => {}
            _ if !program_consumed => {
                program_consumed = true;
            }
            _ => push_access(&mut accesses, access_type, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts write accesses from commands that use `-o` or `--output`.
fn extract_output_option_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let mut index = 1;

    while index < tokens.len() {
        let token = tokens[index].as_str();
        if matches!(token, "-o" | "--output")
            && let Some(path) = tokens.get(index + 1)
        {
            push_access(&mut accesses, ToolAccessKind::Write, path);
            index += 2;
            continue;
        }
        index += 1;
    }

    accesses
}

/// Extracts read and write accesses from one cp command.
fn extract_copy_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut paths = collect_non_option_tokens(tokens);
    if paths.len() < 2 {
        return Vec::new();
    }

    let destination = paths.pop().expect("checked above");
    let mut accesses = Vec::new();
    for source in paths {
        push_access(&mut accesses, ToolAccessKind::Read, source);
    }
    push_access(&mut accesses, ToolAccessKind::Write, destination);
    accesses
}

/// Extracts source edits and destination writes from one mv command.
fn extract_move_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut paths = collect_non_option_tokens(tokens);
    if paths.len() < 2 {
        return Vec::new();
    }

    let destination = paths.pop().expect("checked above");
    let mut accesses = Vec::new();
    for source in paths {
        push_access(&mut accesses, ToolAccessKind::Edit, source);
    }
    push_access(&mut accesses, ToolAccessKind::Write, destination);
    accesses
}

/// Extracts read checks from one `test` or `[` command.
fn extract_test_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    for token in &tokens[1..] {
        if !token.starts_with('-') {
            push_access(&mut accesses, ToolAccessKind::Read, token);
        }
    }
    accesses
}

/// Extracts list accesses from one `fd` command.
fn extract_fd_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let paths = collect_non_option_tokens(tokens);
    if paths.len() < 2 {
        return Vec::new();
    }

    let mut accesses = Vec::new();
    for path in paths.into_iter().skip(1) {
        push_access(&mut accesses, ToolAccessKind::List, path);
    }
    accesses
}

/// Extracts edit accesses from one in-place perl command.
fn extract_perl_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let in_place = tokens
        .iter()
        .any(|token| token == "-i" || token.starts_with("-i"));
    if !in_place {
        return accesses;
    }

    let mut skip_next = false;
    let mut script_consumed = false;
    for token in &tokens[1..] {
        let token = token.as_str();
        if skip_next {
            skip_next = false;
            continue;
        }
        match token {
            "-e" | "-f" | "-i" => skip_next = true,
            _ if token.starts_with("-i") || token.starts_with('-') => {}
            _ if !script_consumed => script_consumed = true,
            _ => push_access(&mut accesses, ToolAccessKind::Edit, token),
        }
    }
    accesses
}

/// Extracts read and write accesses from one link command.
fn extract_link_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let paths = collect_non_option_tokens(tokens);
    if paths.len() < 2 {
        return Vec::new();
    }

    let mut accesses = Vec::new();
    push_access(&mut accesses, ToolAccessKind::Read, paths[0]);
    push_access(
        &mut accesses,
        ToolAccessKind::Write,
        paths.last().expect("checked above"),
    );
    accesses
}

/// Collects path-like operands from one option-bearing command.
fn collect_non_option_tokens(tokens: &[String]) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "--" => {
                for token in &tokens[index + 1..] {
                    paths.push(token.as_str());
                }
                break;
            }
            _ if token == ">" || token == ">>" || token == "1>" || token == "1>>" => {
                skip_next = true;
            }
            _ if token == "<" || token == "<<" => {
                skip_next = true;
            }
            _ if token.starts_with('>')
                || token.starts_with(">>")
                || token.starts_with('<')
                || token.starts_with("<<") => {}
            _ if token.starts_with('-') => {}
            _ => paths.push(token),
        }
        index += 1;
    }

    paths
}

/// Extracts redirection-based file writes and reads from one tokenized command.
fn extract_redirection_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let mut index = 1;

    while index < tokens.len() {
        let token = tokens[index].as_str();
        match token {
            ">" | ">>" | "1>" | "1>>" => {
                if let Some(target) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Write, target);
                    index += 1;
                }
            }
            "<" => {
                if let Some(target) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Read, target);
                    index += 1;
                }
            }
            _ if token.starts_with("<<") => {}
            _ if matches!(token.strip_prefix(">>"), Some(path) if !path.is_empty()) => {
                let path = token.strip_prefix(">>").expect("checked above");
                push_access(&mut accesses, ToolAccessKind::Write, path);
            }
            _ if matches!(token.strip_prefix('>'), Some(path) if !path.is_empty()) => {
                let path = token.strip_prefix('>').expect("checked above");
                push_access(&mut accesses, ToolAccessKind::Write, path);
            }
            _ if matches!(token.strip_prefix('<'), Some(path) if !path.is_empty()) => {
                let path = token.strip_prefix('<').expect("checked above");
                push_access(&mut accesses, ToolAccessKind::Read, path);
            }
            _ => {}
        }
        index += 1;
    }

    accesses
}

/// Returns whether one program name is one of the supported shell interpreters.
fn is_shell_executable(name: &str) -> bool {
    matches!(
        name,
        "bash" | "sh" | "zsh" | "/bin/bash" | "/bin/sh" | "/bin/zsh"
    )
}

/// Returns whether one token is a shell control keyword instead of a command name.
fn is_shell_keyword(token: &str) -> bool {
    matches!(
        token,
        "then" | "do" | "done" | "fi" | "else" | "elif" | "if" | "{" | "}" | "(" | ")"
    )
}

/// Returns whether one leading token is a plain shell environment assignment.
fn is_environment_assignment(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

/// Pushes one accumulated shell fragment if it is not empty.
fn push_fragment(fragments: &mut Vec<String>, current: &mut String) {
    let fragment = current.trim();
    if !fragment.is_empty() {
        fragments.push(fragment.to_owned());
    }
    current.clear();
}
