use super::*;

/// Stores the resolved output behavior for one query invocation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueryOutput {
    pub(crate) color: ColorArg,
}

impl QueryOutput {
    /// Builds one query output context from parsed CLI arguments.
    pub(crate) fn new(color: ColorArg) -> Self {
        Self { color }
    }

    /// Returns whether stdout JSON should be ANSI-colored.
    pub(crate) fn should_color_stdout(self) -> bool {
        should_color_output(
            self.color,
            io::stdout().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            env::var("TERM").ok().as_deref(),
        )
    }
}

/// Writes one machine-readable JSON envelope to stdout.
pub(crate) fn print_json_envelope<T: Serialize>(
    output: &QueryOutput,
    schema: &'static str,
    data: &T,
) -> Result<()> {
    let json = render_json_envelope(schema, data)?;
    print_query_json(output, &json);
    Ok(())
}

/// Returns one serialized machine-readable JSON envelope.
pub(crate) fn render_json_envelope<T: Serialize>(schema: &'static str, data: &T) -> Result<String> {
    let payload = JsonEnvelope {
        schema,
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        data,
    };
    serde_json::to_string_pretty(&payload).context("failed to serialize query response JSON")
}

/// Writes one rendered query JSON document to stdout.
pub(crate) fn print_query_json(output: &QueryOutput, json: &str) {
    if output.should_color_stdout() {
        println!("{}", color_json(json));
    } else {
        println!("{json}");
    }
}

/// Writes one search-turns envelope with optional snippet match highlighting.
pub(crate) fn print_search_turns_json_envelope(
    output: &QueryOutput,
    data: &SearchTurnsQueryData,
) -> Result<()> {
    let json = render_json_envelope("darc.query.search.turns.v1", data)?;
    if output.should_color_stdout() {
        println!("{}", color_search_turns_json(&json, data)?);
    } else {
        println!("{json}");
    }
    Ok(())
}

/// Writes one `darc.query.turns.v1` envelope, compacting rows when `view` is `oneline`.
pub(crate) fn print_turns_query_envelope(
    output: &QueryOutput,
    data: &darc_core::query::TurnsQueryData,
) -> Result<()> {
    match data.view {
        TurnsView::Full => print_json_envelope(output, "darc.query.turns.v1", data),
        TurnsView::Oneline => print_json_envelope(
            output,
            "darc.query.turns.v1",
            &TurnsOnelineQueryData::from_turns_query(data),
        ),
    }
}

pub(crate) const ANSI_RESET: &str = "\x1b[0m";
pub(crate) const ANSI_BOLD: &str = "\x1b[1m";

// JSON syntax colors intentionally stay separate from human report colors.
pub(crate) const ANSI_KEY: &str = "\x1b[1;34m";
pub(crate) const ANSI_STRING: &str = "\x1b[32m";
pub(crate) const ANSI_NUMBER: &str = "\x1b[33m";
pub(crate) const ANSI_BOOLEAN: &str = "\x1b[35m";
pub(crate) const ANSI_NULL: &str = "\x1b[36m";
pub(crate) const ANSI_MATCH: &str = "\x1b[1;95m";

// Runtime report colors keep structure quiet and reserve hues for state.
pub(crate) const ANSI_RED: &str = "\x1b[31m";
pub(crate) const ANSI_DIM: &str = "\x1b[2m";
pub(crate) const ANSI_GREEN: &str = ANSI_STRING;
pub(crate) const ANSI_YELLOW: &str = ANSI_NUMBER;
pub(crate) const ANSI_CYAN: &str = ANSI_NULL;

/// Stores whether human-oriented CLI output should use terminal styling.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HumanStyle {
    pub(crate) enabled: bool,
}

impl HumanStyle {
    /// Builds one style context for stdout.
    pub(crate) fn stdout() -> Self {
        Self::new(
            io::stdout().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            env::var("TERM").ok().as_deref(),
        )
    }

    /// Builds one style context for stderr.
    pub(crate) fn stderr() -> Self {
        Self::new(
            io::stderr().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            env::var("TERM").ok().as_deref(),
        )
    }

    /// Builds one style context from resolved terminal environment facts.
    pub(crate) fn new(is_terminal: bool, no_color: bool, term: Option<&str>) -> Self {
        Self {
            enabled: should_auto_color_output(is_terminal, no_color, term),
        }
    }

    /// Returns one string wrapped with an ANSI style when styling is enabled.
    pub(crate) fn color(self, code: &str, value: impl std::fmt::Display) -> String {
        if self.enabled {
            format!("{code}{value}{ANSI_RESET}")
        } else {
            value.to_string()
        }
    }

    /// Returns one bold display string.
    pub(crate) fn bold(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_BOLD, value)
    }

    /// Returns one field label display string.
    pub(crate) fn label(self, value: impl std::fmt::Display) -> String {
        self.bold(value)
    }

    /// Returns one success display string.
    pub(crate) fn ok(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_GREEN, value)
    }

    /// Returns one warning display string.
    pub(crate) fn warn(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_YELLOW, value)
    }

    /// Returns one error display string.
    pub(crate) fn error(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_RED, value)
    }

    /// Returns one lower-emphasis display string.
    pub(crate) fn muted(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_DIM, value)
    }

    /// Returns one path display string.
    pub(crate) fn path(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_CYAN, value)
    }

    /// Returns one count display string.
    pub(crate) fn count(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_BOLD, value)
    }
}

/// Prints a plain section heading.
pub(crate) fn print_section(style: HumanStyle, title: &str) {
    println!("{}", style.bold(title));
}

/// Prints one indented label/value field.
pub(crate) fn print_field(
    style: HumanStyle,
    indent: usize,
    label: &str,
    value: impl std::fmt::Display,
) {
    println!("{}{}: {}", " ".repeat(indent), style.label(label), value);
}

/// Prints one indented continuation line.
pub(crate) fn print_line(indent: usize, value: impl std::fmt::Display) {
    println!("{}{}", " ".repeat(indent), value);
}

/// Prints one warning to stderr using human-output styling when available.
pub(crate) fn print_warning(message: impl std::fmt::Display) {
    let style = HumanStyle::stderr();
    eprintln!("{}", style.warn(format!("warning: {message}")));
}

/// Prints one project-scoped warning to stderr using human-output styling when available.
pub(crate) fn print_project_warning(project_name: &str, message: impl std::fmt::Display) {
    let style = HumanStyle::stderr();
    eprintln!(
        "{}",
        style.warn(format!("warning [{project_name}]: {message}"))
    );
}

/// Returns a count phrase for one singular/plural noun pair.
pub(crate) fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

/// Returns whether automatic terminal color should be enabled.
pub(crate) fn should_auto_color_output(
    is_terminal: bool,
    no_color: bool,
    term: Option<&str>,
) -> bool {
    is_terminal && !no_color && term != Some("dumb")
}

/// Returns whether one query output stream should include ANSI color.
pub(crate) fn should_color_output(
    policy: ColorArg,
    stdout_is_terminal: bool,
    no_color: bool,
    term: Option<&str>,
) -> bool {
    match policy {
        ColorArg::Always => true,
        ColorArg::Never => false,
        ColorArg::Auto => should_auto_color_output(stdout_is_terminal, no_color, term),
    }
}

/// Adds ANSI syntax color to one pretty-printed JSON string.
pub(crate) fn color_json(json: &str) -> String {
    let mut output = String::with_capacity(json.len());
    let mut index = 0;
    while index < json.len() {
        let ch = json[index..]
            .chars()
            .next()
            .expect("index should be in bounds");
        if ch == '"' {
            let end = json_string_end(json, index);
            let color = if json_string_is_key(json, end) {
                ANSI_KEY
            } else {
                ANSI_STRING
            };
            push_colored(&mut output, color, &json[index..end]);
            index = end;
        } else if ch == '-' || ch.is_ascii_digit() {
            let end = json_number_end(json, index);
            push_colored(&mut output, ANSI_NUMBER, &json[index..end]);
            index = end;
        } else if json[index..].starts_with("true") {
            push_colored(&mut output, ANSI_BOOLEAN, "true");
            index += "true".len();
        } else if json[index..].starts_with("false") {
            push_colored(&mut output, ANSI_BOOLEAN, "false");
            index += "false".len();
        } else if json[index..].starts_with("null") {
            push_colored(&mut output, ANSI_NULL, "null");
            index += "null".len();
        } else if matches!(ch, '{' | '}' | '[' | ']' | ':' | ',') {
            push_colored(&mut output, ANSI_BOLD, &json[index..index + ch.len_utf8()]);
            index += ch.len_utf8();
        } else {
            output.push(ch);
            index += ch.len_utf8();
        }
    }
    output
}

/// Adds ANSI match highlighting to mode-specific search result strings.
pub(crate) fn color_search_turns_json(json: &str, data: &SearchTurnsQueryData) -> Result<String> {
    let mut colored = color_json(json);
    let matcher = SearchSnippetMatcher::new(data.mode, &data.query)?;
    match data.mode {
        SearchMode::Keyword => {
            color_search_snippets(&mut colored, data, &matcher);
        }
        SearchMode::Literal | SearchMode::Regex => {
            color_search_match_snippets(&mut colored, data, &matcher);
        }
        SearchMode::FileName | SearchMode::PathFragment => {
            color_search_matched_paths(&mut colored, data, &matcher);
        }
        SearchMode::FilePath => {
            color_search_matched_path_items(&mut colored, data, |path| Some(0..path.len()));
        }
    }
    Ok(colored)
}

/// Highlights top-level keyword search snippets where a visible query term appears.
pub(crate) fn color_search_snippets(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    matcher: &SearchSnippetMatcher,
) {
    let mut cursor = 0;
    for hit in &data.hits {
        let Some(snippet) = &hit.snippet else {
            continue;
        };
        let Some(range) = non_empty_match(matcher.find(snippet)) else {
            continue;
        };
        let Some((value_start, token_len)) = find_colored_snippet_value(colored, snippet, cursor)
        else {
            continue;
        };
        let highlighted = color_json_string_with_match(snippet, range);
        colored.replace_range(value_start..value_start + token_len, &highlighted);
        cursor = value_start + highlighted.len();
    }
}

/// Highlights nested exact-search match snippets where the exact matcher still finds the term.
pub(crate) fn color_search_match_snippets(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    matcher: &SearchSnippetMatcher,
) {
    let mut cursor = 0;
    for hit in &data.hits {
        for matched in &hit.matches {
            let Some(range) = non_empty_match(matcher.find(&matched.snippet)) else {
                continue;
            };
            let Some((value_start, token_len)) =
                find_colored_snippet_value(colored, &matched.snippet, cursor)
            else {
                continue;
            };
            let highlighted = color_json_string_with_match(&matched.snippet, range);
            colored.replace_range(value_start..value_start + token_len, &highlighted);
            cursor = value_start + highlighted.len();
        }
    }
}

/// Highlights matched file path strings for file-search modes with literal display spans.
pub(crate) fn color_search_matched_paths(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    matcher: &SearchSnippetMatcher,
) {
    color_search_matched_path_items(colored, data, |path| matcher.find(path));
}

/// Highlights matched path items with ranges selected by the caller.
pub(crate) fn color_search_matched_path_items(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    path_range: impl Fn(&str) -> Option<std::ops::Range<usize>>,
) {
    let mut cursor = 0;
    for hit in &data.hits {
        let Some(mut path_cursor) = find_colored_array_start(colored, "matched_paths", cursor)
        else {
            continue;
        };
        for path in &hit.matched_paths {
            let Some(range) = non_empty_match(path_range(path)) else {
                continue;
            };
            let Some((value_start, token_len)) =
                find_colored_string_value(colored, path, path_cursor)
            else {
                continue;
            };
            let highlighted = color_json_string_with_match(path, range);
            colored.replace_range(value_start..value_start + token_len, &highlighted);
            path_cursor = value_start + highlighted.len();
        }
        cursor = path_cursor;
    }
}

/// Drops empty presentation matches before rendering highlight escape codes.
pub(crate) fn non_empty_match(
    range: Option<std::ops::Range<usize>>,
) -> Option<std::ops::Range<usize>> {
    range.filter(|range| !range.is_empty())
}

/// Appends one ANSI-colored JSON token to the rendered output.
pub(crate) fn push_colored(output: &mut String, color: &str, token: &str) {
    output.push_str(color);
    output.push_str(token);
    output.push_str(ANSI_RESET);
}

/// Returns the next colored `snippet` string value from one colored JSON document.
pub(crate) fn find_colored_snippet_value(
    colored: &str,
    snippet: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    let key_prefix = format!("{ANSI_KEY}\"snippet\"{ANSI_RESET}{ANSI_BOLD}:{ANSI_RESET} ");
    let token = color_json_string(snippet);
    let target = format!("{key_prefix}{token}");
    let value_start = cursor + colored.get(cursor..)?.find(&target)? + key_prefix.len();
    Some((value_start, token.len()))
}

/// Returns the byte index after one colored array key prefix.
pub(crate) fn find_colored_array_start(colored: &str, key: &str, cursor: usize) -> Option<usize> {
    let key_prefix =
        format!("{ANSI_KEY}\"{key}\"{ANSI_RESET}{ANSI_BOLD}:{ANSI_RESET} {ANSI_BOLD}[{ANSI_RESET}");
    Some(cursor + colored.get(cursor..)?.find(&key_prefix)? + key_prefix.len())
}

/// Returns the next colored string value matching `value`.
pub(crate) fn find_colored_string_value(
    colored: &str,
    value: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    let token = color_json_string(value);
    let value_start = cursor + colored.get(cursor..)?.find(&token)?;
    Some((value_start, token.len()))
}

/// Returns one syntax-colored JSON string literal.
pub(crate) fn color_json_string(value: &str) -> String {
    format!("{ANSI_STRING}{}{ANSI_RESET}", json_string_literal(value))
}

/// Returns one syntax-colored JSON string literal with a highlighted inner byte range.
pub(crate) fn color_json_string_with_match(value: &str, range: std::ops::Range<usize>) -> String {
    let prefix = json_string_inner(&value[..range.start]);
    let matched = json_string_inner(&value[range.clone()]);
    let suffix = json_string_inner(&value[range.end..]);
    format!(
        "{ANSI_STRING}\"{prefix}{ANSI_MATCH}{matched}{ANSI_RESET}{ANSI_STRING}{suffix}\"{ANSI_RESET}"
    )
}

/// Returns one JSON string literal for a known UTF-8 string.
pub(crate) fn json_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string should not fail")
}

/// Returns the unquoted escaped content for one JSON string literal.
pub(crate) fn json_string_inner(value: &str) -> String {
    let literal = json_string_literal(value);
    literal[1..literal.len() - 1].to_owned()
}

/// Returns the byte index after one JSON string literal.
pub(crate) fn json_string_end(json: &str, start: usize) -> usize {
    let mut escaped = false;
    for (offset, ch) in json[start + 1..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return start + 1 + offset + ch.len_utf8();
        }
    }
    json.len()
}

/// Returns whether one JSON string literal is followed by an object-key colon.
pub(crate) fn json_string_is_key(json: &str, end: usize) -> bool {
    json[end..]
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch == ':')
}

/// Returns the byte index after one JSON number token.
pub(crate) fn json_number_end(json: &str, start: usize) -> usize {
    let mut end = start;
    for ch in json[start..].chars() {
        if matches!(ch, '-' | '+' | '.' | 'e' | 'E') || ch.is_ascii_digit() {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Returns one machine-readable JSON error envelope string.
pub(crate) fn format_query_error(error: &anyhow::Error) -> String {
    let causes = error
        .chain()
        .skip(1)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let structured = error.downcast_ref::<QueryProtocolError>();
    let read_validation = error.downcast_ref::<ReadValidationError>();
    let status_json = error.downcast_ref::<StatusJsonError>();
    let payload = QueryErrorEnvelope {
        schema: "darc.error.v1",
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        error: QueryErrorData {
            message: error.to_string(),
            code: structured
                .map(QueryProtocolError::code)
                .or_else(|| read_validation.map(|error| error.code))
                .or_else(|| status_json.map(|error| error.code)),
            details: structured
                .map(QueryProtocolError::details)
                .or_else(|| read_validation.map(|error| error.details.clone()))
                .or_else(|| status_json.map(|error| error.details.clone())),
            causes,
        },
    };
    serde_json::to_string_pretty(&payload).unwrap_or_else(|serialization_error| {
        format!(r#"{{"schema":"darc.error.v1","error":"{serialization_error}"}}"#)
    })
}

/// Returns one machine-readable JSON error envelope string for JSON parse failures.
pub(crate) fn format_json_clap_error(error: &clap::Error, args: &[OsString]) -> String {
    let message = normalize_json_clap_error_message(error.to_string().trim_end().to_owned(), args);
    render_clap_error_envelope(error, message)
}

/// Returns one machine-readable JSON error envelope string for query parse failures.
#[cfg(test)]
pub(crate) fn format_query_clap_error(error: &clap::Error) -> String {
    render_clap_error_envelope(error, error.to_string().trim_end().to_owned())
}

/// Renders one Clap parse error as a Darc JSON error envelope.
pub(crate) fn render_clap_error_envelope(error: &clap::Error, message: String) -> String {
    let payload = QueryErrorEnvelope {
        schema: "darc.error.v1",
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        error: QueryErrorData {
            message,
            code: Some("invalid_arguments"),
            details: Some(json!({
                "clap_kind": format!("{:?}", error.kind()),
            })),
            causes: Vec::new(),
        },
    };
    serde_json::to_string_pretty(&payload).unwrap_or_else(|serialization_error| {
        format!(r#"{{"schema":"darc.error.v1","error":"{serialization_error}"}}"#)
    })
}

/// Normalizes parse-error text for JSON surfaces with implied required flags.
pub(crate) fn normalize_json_clap_error_message(message: String, args: &[OsString]) -> String {
    if !is_upgrade_json_invocation_without_check(args) {
        return message;
    }
    message.replace(
        "Usage: darc upgrade --json",
        "Usage: darc upgrade --check --json",
    )
}

/// Returns whether the raw args target upgrade JSON without its required check flag.
pub(crate) fn is_upgrade_json_invocation_without_check(args: &[OsString]) -> bool {
    args.get(1).and_then(|arg| arg.to_str()) == Some("upgrade")
        && args.iter().any(|arg| arg == "--json")
        && !args.iter().any(|arg| arg == "--check")
}

/// Stores one machine-readable query success envelope.
#[derive(Debug, Serialize)]
pub(crate) struct JsonEnvelope<'a, T> {
    pub(crate) schema: &'a str,
    pub(crate) generated_at: String,
    pub(crate) darc_version: &'a str,
    pub(crate) data: &'a T,
}

/// Stores one machine-readable query error envelope.
#[derive(Debug, Serialize)]
pub(crate) struct QueryErrorEnvelope<'a> {
    pub(crate) schema: &'a str,
    pub(crate) generated_at: String,
    pub(crate) darc_version: &'a str,
    pub(crate) error: QueryErrorData,
}

/// Stores one machine-readable query error payload.
#[derive(Debug, Serialize)]
pub(crate) struct QueryErrorData {
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) details: Option<JsonValue>,
    pub(crate) causes: Vec<String>,
}

/// Stores one structured validation error raised by canonical JSON read commands.
#[derive(Debug)]
pub(crate) struct ReadValidationError {
    pub(crate) message: String,
    pub(crate) code: &'static str,
    pub(crate) details: JsonValue,
}

impl ReadValidationError {
    /// Builds one missing identity error for a read command.
    pub(crate) fn missing_required_identity(
        value_label: &str,
        flag_name: &str,
        positional_name: &str,
    ) -> Self {
        let message =
            format!("read command requires {value_label} as {positional_name} or {flag_name}");
        Self {
            message,
            code: "missing_required_identity",
            details: json!({
                "value": value_label,
                "flag": flag_name,
                "positional": positional_name,
            }),
        }
    }

    /// Builds one missing turn identity error for a read command.
    pub(crate) fn missing_turn_identity(message: &'static str, missing: &[&str]) -> Self {
        Self {
            message: message.to_owned(),
            code: "missing_required_identity",
            details: json!({ "missing": missing }),
        }
    }

    /// Builds one conflicting identity error for a read command.
    pub(crate) fn conflicting_identity_arguments(
        message: impl Into<String>,
        conflicts: &[&str],
    ) -> Self {
        Self {
            message: message.into(),
            code: "conflicting_identity_arguments",
            details: json!({ "conflicts": conflicts }),
        }
    }
}

impl std::fmt::Display for ReadValidationError {
    /// Writes the user-facing validation message.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReadValidationError {}

/// Stores one structured status JSON error.
#[derive(Debug)]
pub(crate) struct StatusJsonError {
    pub(crate) message: String,
    pub(crate) code: &'static str,
    pub(crate) details: JsonValue,
}

impl StatusJsonError {
    /// Builds one failed status check error.
    pub(crate) fn check_failed(scope: &'static str, message: &'static str) -> Self {
        Self {
            message: message.to_owned(),
            code: "status_check_failed",
            details: json!({
                "scope": scope,
                "check": true,
            }),
        }
    }
}

impl std::fmt::Display for StatusJsonError {
    /// Writes the user-facing status error message.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StatusJsonError {}
