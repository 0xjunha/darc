use std::collections::BTreeSet;

use serde_json::Value;

use super::file_access::{
    CodeChangeSummary, ToolAccessKind, apply_patch_changed_paths, derive_apply_patch_file_accesses,
    path_looks_directory_like, push_access, summarize_apply_patch_changes,
};

/// Stores one parsed shell redirection and how many following tokens it consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShellRedirection<'a> {
    access: Option<(ToolAccessKind, &'a str)>,
    consume_next: bool,
}

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
    let command_text = strip_shell_heredoc_bodies(&command.command_text);
    for fragment in split_shell_fragments(&command_text) {
        if shell_fragment_invokes_apply_patch(&fragment) {
            continue;
        }
        accesses.extend(derive_shell_fragment_file_accesses(
            &fragment,
            command.workdir.as_deref(),
        ));
    }
    accesses
}

/// Derives structured apply-patch file accesses embedded in one shell-like tool invocation.
pub(super) fn derive_shell_apply_patch_file_accesses(
    arguments_text: &str,
) -> Vec<(ToolAccessKind, String)> {
    let Some(command) = parse_shell_command(arguments_text) else {
        return Vec::new();
    };

    extract_apply_patch_heredoc_payloads(&command.command_text)
        .into_iter()
        .flat_map(|patch_payload| derive_apply_patch_file_accesses(&patch_payload))
        .chain(
            split_shell_fragments(&strip_shell_heredoc_bodies(&command.command_text))
                .into_iter()
                .filter(|fragment| shell_fragment_invokes_apply_patch(fragment))
                .flat_map(|fragment| derive_apply_patch_file_accesses(&fragment)),
        )
        .collect()
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

    let heredoc_payloads = extract_apply_patch_heredoc_payloads(&command.command_text);
    let mut summary = CodeChangeSummary::default();
    for patch_payload in &heredoc_payloads {
        summary = summary.saturating_add(summarize_apply_patch_changes(patch_payload));
    }
    for fragment in split_shell_fragments(&command.command_text) {
        if !heredoc_payloads.is_empty() && fragment.contains("<<") {
            continue;
        }
        if fragment.contains("*** Begin Patch") {
            summary = summary.saturating_add(summarize_apply_patch_changes(&fragment));
        }
    }
    summary
}

/// Returns the distinct patch-target paths observed in one shell-like tool payload.
pub fn shell_apply_patch_changed_paths(arguments_text: &str) -> Vec<String> {
    let Some(command) = parse_shell_command(arguments_text) else {
        return Vec::new();
    };

    let heredoc_payloads = extract_apply_patch_heredoc_payloads(&command.command_text);
    let mut paths = BTreeSet::new();
    for patch_payload in &heredoc_payloads {
        paths.extend(apply_patch_changed_paths(patch_payload));
    }
    for fragment in split_shell_fragments(&command.command_text) {
        if !heredoc_payloads.is_empty() && fragment.contains("<<") {
            continue;
        }
        if fragment.contains("*** Begin Patch") {
            paths.extend(apply_patch_changed_paths(&fragment));
        }
    }
    paths.into_iter().collect()
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
    let mut substitution_depth = 0_u32;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
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
            '<' | '>' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'(') => {
                let _ = chars.next();
                substitution_depth = substitution_depth.saturating_add(1);
                current.push(ch);
                current.push('(');
            }
            '$' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'(') => {
                let _ = chars.next();
                substitution_depth = substitution_depth.saturating_add(1);
                current.push(ch);
                current.push('(');
            }
            ')' if !in_single_quote && !in_double_quote && substitution_depth > 0 => {
                substitution_depth = substitution_depth.saturating_sub(1);
                current.push(ch);
            }
            '\n' | ';' if !in_single_quote && !in_double_quote && substitution_depth == 0 => {
                push_fragment(&mut fragments, &mut current);
            }
            '|' if !in_single_quote
                && !in_double_quote
                && substitution_depth == 0
                && current.ends_with('>') =>
            {
                current.push(ch);
            }
            '|' if !in_single_quote && !in_double_quote && substitution_depth == 0 => {
                if chars.peek() == Some(&'|') {
                    let _ = chars.next();
                }
                push_fragment(&mut fragments, &mut current);
            }
            '&' if !in_single_quote
                && !in_double_quote
                && substitution_depth == 0
                && chars.peek() == Some(&'&') =>
            {
                let _ = chars.next();
                push_fragment(&mut fragments, &mut current);
            }
            _ => current.push(ch),
        }
    }

    push_fragment(&mut fragments, &mut current);
    fragments
}

/// Removes heredoc body lines so embedded scripts are not parsed as shell commands.
fn strip_shell_heredoc_bodies(command_text: &str) -> String {
    let mut stripped = Vec::new();
    let mut terminator = None::<String>;

    for line in command_text.lines() {
        if let Some(current_terminator) = terminator.as_deref() {
            if line.trim() == current_terminator {
                terminator = None;
            }
            continue;
        }

        stripped.push(line);
        if let Some(next_terminator) = shell_heredoc_terminator(line) {
            terminator = Some(next_terminator);
        }
    }

    stripped.join("\n")
}

/// Extracts every heredoc-backed `apply_patch` payload embedded in one shell command string.
fn extract_apply_patch_heredoc_payloads(command_text: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    let mut terminator = None::<String>;
    let mut current_payload = Vec::new();

    for line in command_text.lines() {
        if let Some(current_terminator) = terminator.as_deref() {
            if line.trim() == current_terminator {
                payloads.push(current_payload.join("\n"));
                current_payload.clear();
                terminator = None;
            } else {
                current_payload.push(line.to_owned());
            }
            continue;
        }

        if let Some(next_terminator) = apply_patch_heredoc_terminator(line) {
            terminator = Some(next_terminator);
        }
    }

    if terminator.is_some() && !current_payload.is_empty() {
        payloads.push(current_payload.join("\n"));
    }

    payloads
}

/// Returns the heredoc terminator for one `apply_patch <<...` shell line when present.
fn apply_patch_heredoc_terminator(line: &str) -> Option<String> {
    let line = line.trim();
    if !line.contains("apply_patch") || !line.contains("<<") {
        return None;
    }

    shell_heredoc_terminator(line)
}

/// Returns the generic heredoc terminator declared by one shell line.
fn shell_heredoc_terminator(line: &str) -> Option<String> {
    let heredoc_tail = unquoted_heredoc_tail(line)?;
    let heredoc_tail = heredoc_tail.trim();
    if heredoc_tail.starts_with('<') {
        return None;
    }
    let heredoc_tail = heredoc_tail.strip_prefix('-').unwrap_or(heredoc_tail);
    let terminator = normalize_heredoc_marker(heredoc_tail.split_whitespace().next()?);
    (!terminator.is_empty()).then_some(terminator)
}

/// Removes shell quoting from one heredoc delimiter marker.
fn normalize_heredoc_marker(marker: &str) -> String {
    let mut normalized = String::new();
    let mut chars = marker.chars();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if !in_single_quote => {
                if let Some(next) = chars.next() {
                    normalized.push(next);
                }
            }
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            _ => normalized.push(ch),
        }
    }

    normalized
}

/// Returns the text after one unquoted shell heredoc operator.
fn unquoted_heredoc_tail(line: &str) -> Option<&str> {
    let mut chars = line.char_indices().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    while let Some((_, ch)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single_quote => escaped = true,
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '<' if !in_single_quote && !in_double_quote => {
                if let Some((next_index, '<')) = chars.peek().copied() {
                    let _ = chars.next();
                    return Some(&line[next_index + '<'.len_utf8()..]);
                }
            }
            _ => {}
        }
    }

    None
}

/// Splits one shell fragment into shell words while preserving quoted text.
fn tokenize_shell_words(fragment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = fragment.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut substitution_depth = 0_u32;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single_quote => escaped = true,
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '<' | '>' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'(') => {
                let _ = chars.next();
                substitution_depth = substitution_depth.saturating_add(1);
                current.push(ch);
                current.push('(');
            }
            '$' if !in_single_quote && !in_double_quote && chars.peek() == Some(&'(') => {
                let _ = chars.next();
                substitution_depth = substitution_depth.saturating_add(1);
                current.push(ch);
                current.push('(');
            }
            ')' if !in_single_quote && !in_double_quote && substitution_depth > 0 => {
                substitution_depth = substitution_depth.saturating_sub(1);
                current.push(ch);
            }
            ch if ch.is_whitespace()
                && !in_single_quote
                && !in_double_quote
                && substitution_depth == 0 =>
            {
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
        "awk" => extract_awk_file_accesses(tokens),
        "jq" => extract_jq_file_accesses(tokens),
        "node" | "python" | "python3" | "ruby" => extract_script_runner_file_accesses(tokens),
        "cp" => extract_copy_file_accesses(tokens),
        "mv" => extract_move_file_accesses(tokens),
        "rm" => extract_rm_file_accesses(tokens),
        "chmod" => extract_chmod_file_accesses(tokens),
        "chown" | "chgrp" => extract_owner_change_file_accesses(tokens),
        "rmdir" => extract_directory_only_edit_accesses(tokens),
        "mkdir" => extract_directory_only_write_accesses(tokens),
        "touch" => extract_touch_file_accesses(tokens),
        "curl" => extract_output_option_file_accesses(tokens),
        "echo" | "printf" | ":" => extract_redirection_file_accesses(tokens),
        "source" | "." => extract_source_file_accesses(tokens),
        "test" | "[" => extract_test_file_accesses(tokens),
        "fd" => extract_fd_file_accesses(tokens),
        "stat" => extract_stat_file_accesses(tokens),
        "rustfmt" => extract_rustfmt_file_accesses(tokens),
        "lsof" => extract_redirection_file_accesses(tokens),
        "xxd" => extract_xxd_file_accesses(tokens),
        "wc" | "sort" | "mdls" | "file" => {
            extract_simple_path_accesses(tokens, ToolAccessKind::Read, &[])
        }
        "diff" => extract_diff_file_accesses(tokens),
        "perl" => extract_perl_file_accesses(tokens),
        "ln" => extract_link_file_accesses(tokens),
        _ => Vec::new(),
    }
}

/// Returns whether one shell fragment is an apply_patch invocation or payload.
fn shell_fragment_invokes_apply_patch(fragment: &str) -> bool {
    if fragment.contains("apply_patch") && fragment.contains("*** Begin Patch") {
        return true;
    }
    let tokens = tokenize_shell_words(fragment);
    let tokens = trim_shell_prefix_tokens(&tokens);
    tokens.first().is_some_and(|token| token == "apply_patch")
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
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
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
    let mut pattern_from_option = false;
    let mut non_option_tokens = Vec::<&str>::new();
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        if (list_mode || pattern_from_option || !non_option_tokens.is_empty())
            && let Some(next_index) = consume_redirection_token(tokens, index, &mut accesses)
        {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "--regexp" | "-e" | "--file" | "-f" => {
                pattern_from_option = true;
                skip_next = true;
            }
            "--glob" | "-g" | "--type" | "-t" | "--type-not" | "-T" | "--max-count" | "-m"
            | "--context" | "-C" | "-A" | "-B" | "--threads" | "-j" | "--sort" | "--sortr" => {
                skip_next = true;
            }
            _ if token.starts_with('-') => {}
            _ => non_option_tokens.push(token),
        }
        index += 1;
    }

    let path_tokens = if list_mode || pattern_from_option {
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
        push_file_like_access(&mut accesses, access_type, token);
    }
    accesses
}

/// Extracts read accesses from one grep command.
fn extract_grep_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = Vec::new();
    let mut pattern_from_option = false;
    let mut non_option_tokens = Vec::<&str>::new();
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        if (pattern_from_option || !non_option_tokens.is_empty())
            && let Some(next_index) = consume_redirection_token(tokens, index, &mut accesses)
        {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-e" | "-f" => {
                pattern_from_option = true;
                skip_next = true;
            }
            "-m" | "-A" | "-B" | "-C" | "--include" | "--exclude" => {
                skip_next = true;
            }
            _ if token.starts_with('-') => {}
            _ => non_option_tokens.push(token),
        }
        index += 1;
    }

    let path_tokens = if pattern_from_option {
        non_option_tokens
    } else {
        non_option_tokens.into_iter().skip(1).collect()
    };
    for token in path_tokens {
        push_file_like_access(&mut accesses, ToolAccessKind::Read, token);
    }
    accesses
}

/// Extracts read or write accesses from one cat command.
fn extract_cat_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;

    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            _ if token.starts_with('-') => {}
            _ => push_access(&mut accesses, ToolAccessKind::Read, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts list accesses from one find command.
fn extract_find_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut saw_expression = false;
    let mut index = 1;

    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        if token.starts_with('-') || matches!(token, "!" | "(" | ")" | "\\(" | "\\)") {
            saw_expression = true;
        }
        if saw_expression {
            index += 1;
            continue;
        }
        push_file_like_access(&mut accesses, ToolAccessKind::List, token);
        index += 1;
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
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        if options_with_values.contains(&token) {
            skip_next = true;
        } else if token.starts_with('-') {
            // Ignore option flags.
        } else if access_type == ToolAccessKind::List {
            push_file_like_access(&mut accesses, access_type, token);
        } else {
            push_access(&mut accesses, access_type, token);
        }
        index += 1;
    }

    accesses
}

/// Extracts touch targets while reading reference operands.
fn extract_touch_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-r" | "--reference" => {
                if let Some(path) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                skip_next = true;
            }
            _ if token.starts_with("--reference=") => {
                if let Some(path) = token.split_once('=').map(|(_, path)| path) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
            }
            "-A" | "-d" | "-t" | "--date" | "--time" => skip_next = true,
            _ if token.starts_with("--date=") || token.starts_with("--time=") => {}
            _ if token.starts_with('-') => {}
            _ => push_access(&mut accesses, ToolAccessKind::Write, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts chmod target edits while skipping mode operands such as `+x`.
fn extract_chmod_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut mode_consumed = false;
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "--reference" => {
                if let Some(path) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                skip_next = true;
                mode_consumed = true;
            }
            _ if token.starts_with("--reference=") => {
                if let Some(path) = token.split_once('=').map(|(_, path)| path) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                mode_consumed = true;
            }
            "--" => {}
            _ if token.starts_with('-') && !looks_like_symbolic_chmod_mode(token) => {}
            _ if !mode_consumed => mode_consumed = true,
            _ => push_access(&mut accesses, ToolAccessKind::Edit, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts chown and chgrp target edits while skipping owner/group operands.
fn extract_owner_change_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut owner_consumed = false;
    let mut index = 1;
    let mut skip_next = false;

    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "--reference" => {
                if let Some(path) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                skip_next = true;
                owner_consumed = true;
            }
            _ if token.starts_with("--reference=") => {
                if let Some(path) = token.split_once('=').map(|(_, path)| path) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                owner_consumed = true;
            }
            "--from" => skip_next = true,
            _ if token.starts_with("--from=") => {}
            "--" => {}
            _ if token.starts_with('-') => {}
            _ if !owner_consumed => owner_consumed = true,
            _ => push_access(&mut accesses, ToolAccessKind::Edit, token),
        }
        index += 1;
    }

    accesses
}

/// Preserves redirection writes while dropping directory-only `mkdir` operands.
fn extract_directory_only_write_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    extract_redirection_file_accesses(tokens)
}

/// Preserves redirection writes while dropping directory-only `rmdir` operands.
fn extract_directory_only_edit_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    extract_redirection_file_accesses(tokens)
}

/// Preserves file edits while dropping recursive `rm` directory operands.
fn extract_rm_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let recursive = tokens
        .iter()
        .any(|token| token == "--recursive" || short_flag_contains(token, 'r'));
    let mut accesses = extract_redirection_file_accesses(tokens);
    for path in collect_non_option_tokens(tokens) {
        if recursive {
            push_file_like_access(&mut accesses, ToolAccessKind::Edit, path);
        } else {
            push_access(&mut accesses, ToolAccessKind::Edit, path);
        }
    }
    accesses
}

/// Extracts script-file reads from shell and interpreter entrypoints.
fn extract_script_runner_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        if token == "-" || token.starts_with("<<") {
            return accesses;
        }
        if token.starts_with('-') {
            if matches!(token, "-c" | "-m") {
                return accesses;
            }
            index += 1;
            continue;
        }
        accesses.push((ToolAccessKind::Read, token.to_owned()));
        return accesses;
    }
    accesses
}

/// Extracts manifest and fmt target accesses from cargo commands.
fn extract_cargo_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut saw_fmt = false;
    let mut after_double_dash = false;

    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
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
        if after_double_dash && saw_fmt && !token.starts_with('-') {
            push_access(&mut accesses, ToolAccessKind::Edit, token);
        }
        index += 1;
    }

    accesses
}

/// Extracts read accesses from one awk command.
fn extract_awk_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
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
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-v" | "-F" => skip_next = true,
            _ if token.starts_with("-F") => {}
            "-f" => {
                if let Some(path) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                program_consumed = true;
                skip_next = true;
            }
            _ if token.starts_with('-') => {}
            _ if !program_consumed => {
                program_consumed = true;
            }
            _ => push_access(&mut accesses, ToolAccessKind::Read, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts read accesses from one jq command.
fn extract_jq_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    let mut program_consumed = false;

    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "--arg" | "--argjson" => {
                index += 3;
                continue;
            }
            "--rawfile" | "--slurpfile" => {
                if let Some(path) = tokens.get(index + 2) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                index += 3;
                continue;
            }
            "-f" | "--from-file" => {
                if let Some(path) = tokens.get(index + 1) {
                    push_access(&mut accesses, ToolAccessKind::Read, path);
                }
                program_consumed = true;
                index += 2;
                continue;
            }
            _ if token.starts_with("--arg=")
                || token.starts_with("--argjson=")
                || token.starts_with("--rawfile=")
                || token.starts_with("--slurpfile=") => {}
            _ if let Some(path) = token.strip_prefix("--from-file=") => {
                push_access(&mut accesses, ToolAccessKind::Read, path);
                program_consumed = true;
            }
            _ if token.starts_with('-') => {}
            _ if !program_consumed => program_consumed = true,
            _ => push_access(&mut accesses, ToolAccessKind::Read, token),
        }
        index += 1;
    }

    accesses
}

/// Extracts read accesses from one stat command while skipping format options.
fn extract_stat_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    extract_simple_path_accesses(
        tokens,
        ToolAccessKind::Read,
        &["-f", "-t", "-c", "--format", "--printf"],
    )
}

/// Extracts rustfmt file operands while skipping configuration option values.
fn extract_rustfmt_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    extract_simple_path_accesses(
        tokens,
        ToolAccessKind::Read,
        &[
            "--color",
            "--config",
            "--config-path",
            "--edition",
            "--emit",
            "--error-on-line-overflow",
            "--error-on-unformatted",
            "--files-with-diff",
            "--print-config",
            "--style-edition",
        ],
    )
}

/// Extracts xxd input operands while skipping numeric formatting options.
fn extract_xxd_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    extract_simple_path_accesses(
        tokens,
        ToolAccessKind::Read,
        &[
            "-c",
            "-cols",
            "-g",
            "-groupsize",
            "-l",
            "-len",
            "-o",
            "-s",
            "-seek",
        ],
    )
}

/// Extracts write accesses from commands that use `-o` or `--output`.
fn extract_output_option_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;

    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
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
    let mut accesses = extract_redirection_file_accesses(tokens);
    if paths.len() < 2 {
        return accesses;
    }

    let destination = paths.pop().expect("checked above");
    for source in paths {
        push_access(&mut accesses, ToolAccessKind::Read, source);
    }
    push_access(&mut accesses, ToolAccessKind::Write, destination);
    accesses
}

/// Extracts source edits and destination writes from one mv command.
fn extract_move_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut paths = collect_non_option_tokens(tokens);
    let mut accesses = extract_redirection_file_accesses(tokens);
    if paths.len() < 2 {
        return accesses;
    }

    let destination = paths.pop().expect("checked above");
    for source in paths {
        push_access(&mut accesses, ToolAccessKind::Edit, source);
    }
    push_access(&mut accesses, ToolAccessKind::Write, destination);
    accesses
}

/// Extracts read checks from one `test` or `[` command.
fn extract_test_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        if is_file_test_unary_operator(token)
            && let Some(path) = tokens.get(index + 1)
        {
            if token == "-d" {
                push_file_like_access(&mut accesses, ToolAccessKind::Read, path);
            } else {
                push_access(&mut accesses, ToolAccessKind::Read, path);
            }
            index += 2;
            continue;
        }
        if index > 1
            && is_file_test_binary_operator(token)
            && let Some(right) = tokens.get(index + 1)
        {
            push_access(&mut accesses, ToolAccessKind::Read, &tokens[index - 1]);
            push_access(&mut accesses, ToolAccessKind::Read, right);
            index += 2;
            continue;
        }
        index += 1;
    }
    accesses
}

/// Preserves file reads while dropping recursive `diff` directory operands.
fn extract_diff_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let recursive = tokens
        .iter()
        .any(|token| token == "--recursive" || short_flag_contains(token, 'r'));
    let mut accesses = extract_redirection_file_accesses(tokens);
    for path in collect_non_option_tokens(tokens) {
        if recursive {
            push_file_like_access(&mut accesses, ToolAccessKind::Read, path);
        } else {
            push_access(&mut accesses, ToolAccessKind::Read, path);
        }
    }
    accesses
}

/// Extracts list accesses from one `fd` command.
fn extract_fd_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let paths = collect_non_option_tokens(tokens);
    let mut accesses = extract_redirection_file_accesses(tokens);
    if paths.len() < 2 {
        return accesses;
    }

    for path in paths.into_iter().skip(1) {
        push_file_like_access(&mut accesses, ToolAccessKind::List, path);
    }
    accesses
}

/// Appends one extracted access path when the operand still looks file-like.
fn push_file_like_access(
    accesses: &mut Vec<(ToolAccessKind, String)>,
    access_type: ToolAccessKind,
    path: &str,
) {
    if !path_looks_directory_like(path) {
        push_access(accesses, access_type, path);
    }
}

/// Returns whether one combined short-option token contains the requested flag.
fn short_flag_contains(token: &str, flag: char) -> bool {
    token.starts_with('-') && !token.starts_with("--") && token.chars().skip(1).any(|ch| ch == flag)
}

/// Returns whether one token is a chmod symbolic mode rather than an option.
fn looks_like_symbolic_chmod_mode(token: &str) -> bool {
    token.chars().all(|ch| {
        matches!(
            ch,
            'u' | 'g' | 'o' | 'a' | 'r' | 'w' | 'x' | 'X' | 's' | 't' | '+' | '-' | '='
        )
    }) && token.chars().any(|ch| matches!(ch, '+' | '-' | '='))
}

/// Returns whether one `test` unary operator takes a file path operand.
fn is_file_test_unary_operator(token: &str) -> bool {
    matches!(
        token,
        "-a" | "-b"
            | "-c"
            | "-d"
            | "-e"
            | "-f"
            | "-g"
            | "-G"
            | "-h"
            | "-k"
            | "-L"
            | "-N"
            | "-O"
            | "-p"
            | "-r"
            | "-s"
            | "-S"
            | "-u"
            | "-w"
            | "-x"
    )
}

/// Returns whether one `test` binary operator compares two file path operands.
fn is_file_test_binary_operator(token: &str) -> bool {
    matches!(token, "-ef" | "-nt" | "-ot")
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
    let mut index = 1;
    while index < tokens.len() {
        if skip_next {
            skip_next = false;
            index += 1;
            continue;
        }
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        let token = tokens[index].as_str();
        match token {
            "-e" | "-f" | "-i" => skip_next = true,
            _ if token.starts_with("-i") || token.starts_with('-') => {}
            _ if !script_consumed => script_consumed = true,
            _ => push_access(&mut accesses, ToolAccessKind::Edit, token),
        }
        index += 1;
    }
    accesses
}

/// Extracts the sourced file path while preserving any output redirection.
fn extract_source_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let mut accesses = extract_redirection_file_accesses(tokens);
    let mut index = 1;
    while index < tokens.len() {
        if let Some(next_index) = skip_redirection_token(tokens, index) {
            index = next_index;
            continue;
        }
        push_access(&mut accesses, ToolAccessKind::Read, &tokens[index]);
        break;
    }
    accesses
}

/// Extracts read and write accesses from one link command.
fn extract_link_file_accesses(tokens: &[String]) -> Vec<(ToolAccessKind, String)> {
    let paths = collect_non_option_tokens(tokens);
    let mut accesses = extract_redirection_file_accesses(tokens);
    if paths.len() < 2 {
        return accesses;
    }

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

    while index < tokens.len() {
        let token = tokens[index].as_str();
        match token {
            "--" => {
                index += 1;
                while index < tokens.len() {
                    if let Some(next_index) = skip_redirection_token(tokens, index) {
                        index = next_index;
                        continue;
                    }
                    paths.push(tokens[index].as_str());
                    index += 1;
                }
                break;
            }
            _ if parse_redirection_token(token, tokens.get(index + 1).map(String::as_str))
                .is_some() =>
            {
                if let Some(next_index) = skip_redirection_token(tokens, index) {
                    index = next_index;
                    continue;
                }
            }
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
        if let Some(next_index) = consume_redirection_token(tokens, index, &mut accesses) {
            index = next_index;
            continue;
        }
        index += 1;
    }

    accesses
}

/// Consumes one redirection token and records a target file access when present.
fn consume_redirection_token(
    tokens: &[String],
    index: usize,
    accesses: &mut Vec<(ToolAccessKind, String)>,
) -> Option<usize> {
    let redirection = parse_redirection_token(
        tokens.get(index)?.as_str(),
        tokens.get(index + 1).map(String::as_str),
    )?;
    if let Some((access_type, target)) = redirection.access {
        push_access(accesses, access_type, target);
    }
    Some(index + 1 + usize::from(redirection.consume_next))
}

/// Returns the next token index when the current token is shell redirection syntax.
fn skip_redirection_token(tokens: &[String], index: usize) -> Option<usize> {
    parse_redirection_token(
        tokens.get(index)?.as_str(),
        tokens.get(index + 1).map(String::as_str),
    )
    .map(|redirection| index + 1 + usize::from(redirection.consume_next))
}

/// Parses one token as shell redirection syntax without treating fd duplication as a file.
fn parse_redirection_token<'a>(
    token: &'a str,
    next_token: Option<&'a str>,
) -> Option<ShellRedirection<'a>> {
    let body = token.trim_start_matches(|ch: char| ch.is_ascii_digit());
    if body.is_empty() {
        return None;
    }
    if matches!(body, "<<" | "<<-" | "<<<") {
        return Some(ShellRedirection {
            access: None,
            consume_next: true,
        });
    }
    if body.starts_with("<<") || body.starts_with("<<<") {
        return Some(ShellRedirection {
            access: None,
            consume_next: false,
        });
    }
    if let Some(access_type) = separate_redirection_access_type(body) {
        return Some(ShellRedirection {
            access: next_token.and_then(|target| redirection_target_access(access_type, target)),
            consume_next: true,
        });
    }
    for (operator, access_type) in [
        ("&>>", ToolAccessKind::Write),
        ("&>", ToolAccessKind::Write),
        (">>", ToolAccessKind::Write),
        (">|", ToolAccessKind::Write),
        (">", ToolAccessKind::Write),
        ("<>", ToolAccessKind::Edit),
        ("<", ToolAccessKind::Read),
    ] {
        if let Some(target) = body.strip_prefix(operator)
            && !target.is_empty()
        {
            return Some(ShellRedirection {
                access: redirection_target_access(access_type, target),
                consume_next: false,
            });
        }
    }
    None
}

/// Returns the access kind for a redirection operator that takes its target from the next token.
fn separate_redirection_access_type(token: &str) -> Option<ToolAccessKind> {
    match token {
        ">" | ">>" | ">|" | "&>" | "&>>" => Some(ToolAccessKind::Write),
        "<>" => Some(ToolAccessKind::Edit),
        "<" => Some(ToolAccessKind::Read),
        _ => None,
    }
}

/// Returns the file target for one redirection when the target names a real path.
fn redirection_target_access(
    access_type: ToolAccessKind,
    target: &str,
) -> Option<(ToolAccessKind, &str)> {
    let target = target.trim();
    (!target.is_empty() && !is_fd_duplication_target(target)).then_some((access_type, target))
}

/// Returns whether one redirection target is an fd duplication or close operation.
fn is_fd_duplication_target(target: &str) -> bool {
    let Some(rest) = target.strip_prefix('&') else {
        return false;
    };
    rest == "-" || (!rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
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
