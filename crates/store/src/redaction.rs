use std::sync::LazyLock;

use darc_rollout::model::{NormalizedTurn, NormalizedTurnStep};
use regex::Regex;
use serde_json::Value;

const REDACTED_BLOB: &str = "[REDACTED_BLOB]";
const REDACTED_CREDENTIALS: &str = "[REDACTED_CREDENTIALS]";
const REDACTED_HOME: &str = "[REDACTED_HOME]";
const REDACTED_SECRET: &str = "[REDACTED_SECRET]";
const SECRET_KEY_REGEX_FRAGMENT: &str = r"(?i-u:(?:[A-Za-z0-9]+[_-])*(?:openai[_-]?api[_-]?key|anthropic[_-]?api[_-]?key|aws[_-]?secret[_-]?access[_-]?key|aws[_-]?access[_-]?key(?:[_-]?id)?|aws[_-]?session[_-]?token|github[_-]?token|personal[_-]?access[_-]?token|api[_-]?token|auth[_-]?token|bearer[_-]?token|session[_-]?token|api[_-]?key|access[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|private[_-]?key|client[_-]?secret|secret[_-]?key|proxy[_-]?authorization|authorization|credentials?|password|passwd|secret|token|cookie)|[A-Za-z0-9]*(?:apiKey|privateKey|clientSecret|accessKey|accessToken|refreshToken|idToken|authToken|bearerToken|sessionToken|personalAccessToken|secretKey|proxyAuthorization|authorization|credentials?|password|passwd|secret|token|cookie))";
const SECRET_FLAG_REGEX_FRAGMENT: &str = r"(?i-u:(?:[A-Za-z0-9]+-)*(?:openai-api-key|anthropic-api-key|aws-secret-access-key|aws-access-key(?:-id)?|aws-session-token|github-token|personal-access-token|api-token|auth-token|bearer-token|session-token|api-key|access-key|access-token|refresh-token|id-token|private-key|client-secret|secret-key|proxy-authorization|authorization|credentials?|password|passwd|secret|token|cookie))";

static PRIVATE_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
    )
});
static DATA_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r"[dD][aA][tT][aA]:[A-Za-z0-9.+/-]+/[^;\s]+;[bB][aA][sS][eE]64,[A-Za-z0-9+/=_-]+")
});
static BASE64_BLOB_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"[A-Za-z0-9+/=_-]{512,}"));
static OPENAI_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bsk-[A-Za-z0-9_-]{16,}\b"));
static ANTHROPIC_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bsk-ant-[A-Za-z0-9_-]{16,}\b"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"));
static GITHUB_TOKEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,})\b")
});
static NPM_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bnpm_[A-Za-z0-9]{20,}\b"));
static SLACK_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bxox[baprs]-[A-Za-z0-9-]{20,}\b"));
static JWT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b")
});
static BEARER_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\b[Bb][Ee][Aa][Rr][Ee][Rr]\s+[A-Za-z0-9._~+/=-]{12,}\b"));
static BASIC_AUTH_CONTEXT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r#"(?i-u)((?:(?:-H|--header)\s+["']?(?:authorization|proxy-authorization):\s*|["']?(?:authorization|proxy-authorization)["']?\s*[:=]\s*["']?|headers?\s*(?:\.\s*(?:authorization|proxy-authorization)|\[\s*["'](?:authorization|proxy-authorization)["']\s*\])\s*[:=]\s*["']?)Basic\s+)([A-Za-z0-9+/=]{4,})"#,
    )
});
static SECRET_ASSIGNMENT_DOUBLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"\b({SECRET_KEY_REGEX_FRAGMENT})\b(\s*[:=]\s*)"([^\s"\r\n][^"\r\n]{{3,}})""#
    ))
});
static SECRET_ASSIGNMENT_SINGLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"\b({SECRET_KEY_REGEX_FRAGMENT})\b(\s*[:=]\s*)'([^\s'\r\n][^'\r\n]{{3,}})'"#
    ))
});
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"\b({SECRET_KEY_REGEX_FRAGMENT})\b(\s*[:=]\s*)([^\s"',;}}{{]{{4,}})"#
    ))
});
static SECRET_FLAG_EQUALS_DOUBLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}=)"([^"\r\n]{{4,}})""#
    ))
});
static SECRET_FLAG_EQUALS_SINGLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}=)'([^'\r\n]{{4,}})'"#
    ))
});
static SECRET_FLAG_EQUALS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}=)([^\s"',;}}{{]+)"#
    ))
});
static SECRET_FLAG_VALUE_DOUBLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}\s+)"([^"\r\n]{{4,}})""#
    ))
});
static SECRET_FLAG_VALUE_SINGLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}\s+)'([^'\r\n]{{4,}})'"#
    ))
});
static SECRET_FLAG_VALUE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}\s+)([^\s"',;}}{{]+)"#
    ))
});
static URL_CREDENTIALS_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\b([A-Za-z][A-Za-z0-9+.-]*://)([^/\s:@]+):([^/\s@]+)@"));
static SIGNED_QUERY_PARAM_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"([?&](?:[Xx]-[Aa]mz-[Ss]ignature|[Xx]-[Aa]mz-[Cc]redential|[Xx]-[Aa]mz-[Ss]ecurity-[Tt]oken|[Ss]ignature|sig|[Aa]ccess[_-]?[Tt]oken|[Aa]ccess[Tt]oken|[Rr]efresh[_-]?[Tt]oken|[Rr]efresh[Tt]oken|[Ii]d[_-]?[Tt]oken|[Ii]d[Tt]oken|[Tt]oken|[Aa]pi[_-]?[Kk]ey|[Aa]pi[Kk]ey|[Kk]ey|[Pp]assword|[Cc]lient[_-]?[Ss]ecret|[Cc]lient[Ss]ecret)=)[^&\s]+",
    )
});
static POSIX_HOME_PATH_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r#"(?s)(^|.)/(Users|home)/[^/\s:'")\[\]}]+"#));
static WINDOWS_HOME_PATH_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r#"\b([A-Za-z]:\\Users\\)[^\\\s:'")\[\]}]+"#));

/// Redacts sensitive values from one normalized turn before it is indexed.
pub(crate) fn redact_normalized_turn(turn: &mut NormalizedTurn) {
    turn.user_message = redact_text(&turn.user_message);
    if let Some(final_answer) = turn.final_answer.as_mut() {
        final_answer.text = redact_text(&final_answer.text);
    }
    redact_normalized_steps(&mut turn.steps);
}

/// Redacts sensitive values from normalized turn steps before they are indexed.
pub(crate) fn redact_normalized_steps(steps: &mut [NormalizedTurnStep]) {
    for step in steps {
        redact_step(step);
    }
}

/// Redacts sensitive values from one plain text payload.
pub(crate) fn redact_text(input: &str) -> String {
    let redacted = PRIVATE_KEY_REGEX.replace_all(input, REDACTED_SECRET);
    let redacted = DATA_URL_REGEX.replace_all(&redacted, REDACTED_BLOB);
    let redacted = OPENAI_KEY_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = ANTHROPIC_KEY_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = AWS_ACCESS_KEY_ID_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = GITHUB_TOKEN_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = NPM_TOKEN_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = SLACK_TOKEN_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = JWT_REGEX.replace_all(&redacted, REDACTED_SECRET);
    let redacted = BEARER_TOKEN_REGEX.replace_all(&redacted, "Bearer [REDACTED_SECRET]");
    let redacted = redact_basic_auth(&redacted);
    let redacted = redact_secret_assignment(
        &redacted,
        &SECRET_ASSIGNMENT_DOUBLE_QUOTED_REGEX,
        "\"[REDACTED_SECRET]\"",
    );
    let redacted = redact_secret_assignment(
        &redacted,
        &SECRET_ASSIGNMENT_SINGLE_QUOTED_REGEX,
        "'[REDACTED_SECRET]'",
    );
    let redacted =
        redact_secret_assignment(&redacted, &SECRET_ASSIGNMENT_REGEX, "[REDACTED_SECRET]");
    let redacted =
        redact_quoted_secret_flag(&redacted, &SECRET_FLAG_EQUALS_DOUBLE_QUOTED_REGEX, '"');
    let redacted =
        redact_quoted_secret_flag(&redacted, &SECRET_FLAG_EQUALS_SINGLE_QUOTED_REGEX, '\'');
    let redacted = redact_unquoted_secret_flag(&redacted, &SECRET_FLAG_EQUALS_REGEX);
    let redacted =
        redact_quoted_secret_flag(&redacted, &SECRET_FLAG_VALUE_DOUBLE_QUOTED_REGEX, '"');
    let redacted =
        redact_quoted_secret_flag(&redacted, &SECRET_FLAG_VALUE_SINGLE_QUOTED_REGEX, '\'');
    let redacted = redact_unquoted_secret_flag(&redacted, &SECRET_FLAG_VALUE_REGEX);
    let credentials_replacement = format!("$1{REDACTED_CREDENTIALS}@");
    let redacted = URL_CREDENTIALS_REGEX.replace_all(&redacted, credentials_replacement.as_str());
    let redacted = SIGNED_QUERY_PARAM_REGEX.replace_all(&redacted, "$1[REDACTED_SECRET]");
    let redacted = BASE64_BLOB_REGEX.replace_all(&redacted, REDACTED_BLOB);
    let redacted = redact_posix_home_paths(&redacted);
    let windows_home_replacement = format!("$1{REDACTED_HOME}");
    let redacted =
        WINDOWS_HOME_PATH_REGEX.replace_all(&redacted, windows_home_replacement.as_str());
    redacted.to_string()
}

/// Redacts Basic auth credentials only when they appear in header contexts.
fn redact_basic_auth(input: &str) -> String {
    BASIC_AUTH_CONTEXT_REGEX
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let Some(prefix) = captures.get(1) else {
                return captures[0].to_owned();
            };
            let Some(payload) = captures.get(2) else {
                return captures[0].to_owned();
            };
            if !looks_like_basic_auth_payload(payload.as_str()) {
                return captures[0].to_owned();
            }
            format!("{}{REDACTED_SECRET}", prefix.as_str())
        })
        .into_owned()
}

/// Redacts generic secret assignments unless a long CLI flag should handle them.
fn redact_secret_assignment(input: &str, regex: &Regex, replacement_value: &str) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let Some(secret_key) = captures.get(1) else {
                return captures[0].to_owned();
            };
            let Some(separator) = captures.get(2) else {
                return captures[0].to_owned();
            };
            let Some(value) = captures.get(3) else {
                return captures[0].to_owned();
            };
            let Some(full_match) = captures.get(0) else {
                return captures[0].to_owned();
            };
            if is_embedded_long_flag_assignment(input, full_match.start()) {
                return captures[0].to_owned();
            }
            if is_authorization_scheme_assignment(
                secret_key.as_str(),
                separator.as_str(),
                value.as_str(),
            ) {
                return captures[0].to_owned();
            }
            format!(
                "{}{}{replacement_value}",
                secret_key.as_str(),
                separator.as_str()
            )
        })
        .into_owned()
}

/// Returns whether an assignment match is the key suffix of a `--flag=value`.
fn is_embedded_long_flag_assignment(input: &str, match_start: usize) -> bool {
    let bytes = input.as_bytes();
    match_start >= 2
        && bytes.get(match_start - 1).is_some_and(|byte| *byte == b'-')
        && bytes.get(match_start - 2).is_some_and(|byte| *byte == b'-')
}

/// Returns whether an Authorization header match only captured the auth scheme.
fn is_authorization_scheme_assignment(key: &str, separator: &str, value: &str) -> bool {
    separator.contains(':')
        && matches!(
            key.to_ascii_lowercase().as_str(),
            "authorization" | "proxy-authorization" | "proxy_authorization"
        )
        && matches!(
            value.to_ascii_lowercase().as_str(),
            "basic" | "bearer" | "digest" | "negotiate" | "ntlm" | "token"
        )
}

/// Redacts quoted CLI secret flag values while leaving wildcard-only examples intact.
fn redact_quoted_secret_flag(input: &str, regex: &Regex, quote: char) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let Some(prefix) = captures.get(1) else {
                return captures[0].to_owned();
            };
            let Some(value) = captures.get(2) else {
                return captures[0].to_owned();
            };
            if is_wildcard_only_secret_value(value.as_str()) {
                return captures[0].to_owned();
            }
            format!("{}{quote}{REDACTED_SECRET}{quote}", prefix.as_str())
        })
        .into_owned()
}

/// Redacts unquoted CLI secret flag values while leaving wildcard-only examples intact.
fn redact_unquoted_secret_flag(input: &str, regex: &Regex) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let Some(prefix) = captures.get(1) else {
                return captures[0].to_owned();
            };
            let Some(value) = captures.get(2) else {
                return captures[0].to_owned();
            };
            if is_wildcard_only_secret_value(value.as_str()) {
                return captures[0].to_owned();
            }
            format!("{}{REDACTED_SECRET}", prefix.as_str())
        })
        .into_owned()
}

/// Returns whether a secret-looking flag value is only a shell, SQL, or glob wildcard.
fn is_wildcard_only_secret_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| matches!(byte, b'%' | b'_' | b'*' | b'?'))
}

/// Redacts POSIX home path usernames without treating relative synthetic paths as homes.
fn redact_posix_home_paths(input: &str) -> String {
    POSIX_HOME_PATH_REGEX
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let prefix = captures.get(1).map_or("", |value| value.as_str());
            if is_synthetic_posix_home_prefix(prefix) {
                return captures[0].to_owned();
            }
            let Some(root) = captures.get(2) else {
                return captures[0].to_owned();
            };
            format!("{prefix}/{}/{REDACTED_HOME}", root.as_str())
        })
        .into_owned()
}

/// Returns whether the character before `/home` or `/Users` makes the path relative.
fn is_synthetic_posix_home_prefix(prefix: &str) -> bool {
    prefix.as_bytes().last().is_some_and(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'$')
    })
}

/// Returns whether one Basic auth payload decodes to colon-separated credentials.
fn looks_like_basic_auth_payload(payload: &str) -> bool {
    let Some(decoded) = decode_base64_payload(payload) else {
        return false;
    };
    decoded.contains(&b':')
}

/// Decodes one standard base64 payload with optional trailing padding.
fn decode_base64_payload(payload: &str) -> Option<Vec<u8>> {
    if payload.len() < 4 || payload.len() % 4 == 1 {
        return None;
    }

    let mut normalized = payload.to_owned();
    while !normalized.len().is_multiple_of(4) {
        normalized.push('=');
    }

    let mut output = Vec::with_capacity(normalized.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0_u8;
    let mut seen_padding = false;
    for byte in normalized.bytes() {
        if byte == b'=' {
            seen_padding = true;
            continue;
        }
        if seen_padding {
            return None;
        }
        let value = base64_value(byte)?;
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

/// Returns the six-bit value for one standard base64 character.
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Redacts sensitive values from one JSON string, preserving JSON shape when parseable.
pub(crate) fn redact_json_text(input: &str) -> String {
    match serde_json::from_str::<Value>(input) {
        Ok(mut value) => {
            redact_json_value(None, &mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| redact_text(input))
        }
        Err(_) => redact_text(input),
    }
}

/// Redacts one step in place while preserving stable structural metadata.
fn redact_step(step: &mut NormalizedTurnStep) {
    match step {
        NormalizedTurnStep::Reasoning { summary, .. } => {
            for item in summary {
                *item = redact_text(item);
            }
        }
        NormalizedTurnStep::Commentary { text, .. } => {
            *text = redact_text(text);
        }
        NormalizedTurnStep::ToolCall {
            call_id,
            name,
            arguments,
            ..
        } => {
            *call_id = redact_text(call_id);
            *name = redact_text(name);
            *arguments = redact_json_text(arguments);
        }
        NormalizedTurnStep::ToolCallOutput {
            call_id, output, ..
        } => {
            *call_id = redact_text(call_id);
            *output = redact_json_text(output);
        }
        NormalizedTurnStep::Attachment {
            attachment_type,
            payload_json,
            ..
        } => {
            *attachment_type = redact_text(attachment_type);
            *payload_json = redact_json_text(payload_json);
        }
        NormalizedTurnStep::Delegation {
            call_id,
            task_id,
            event,
            agent_id,
            agent_type,
            status,
            summary,
            payload_json,
            ..
        } => {
            redact_optional_text(call_id);
            redact_optional_text(task_id);
            *event = redact_text(event);
            redact_optional_text(agent_id);
            redact_optional_text(agent_type);
            redact_optional_text(status);
            redact_optional_text(summary);
            *payload_json = redact_json_text(payload_json);
        }
        NormalizedTurnStep::HookSummary {
            call_id,
            level,
            payload_json,
            ..
        } => {
            redact_optional_text(call_id);
            redact_optional_text(level);
            *payload_json = redact_json_text(payload_json);
        }
        NormalizedTurnStep::ProviderResponseItem {
            item_type,
            payload_json,
            ..
        } => {
            *item_type = redact_text(item_type);
            *payload_json = redact_json_text(payload_json);
        }
    }
}

/// Redacts one optional string value in place.
fn redact_optional_text(value: &mut Option<String>) {
    if let Some(value) = value {
        *value = redact_text(value);
    }
}

/// Recursively redacts secret-like JSON values.
fn redact_json_value(key: Option<&str>, value: &mut Value) {
    if key.is_some_and(secret_like_key) {
        *value = Value::String(REDACTED_SECRET.to_owned());
        return;
    }

    match value {
        Value::String(text) => {
            *text = redact_json_string_value(text);
        }
        Value::Array(values) => {
            for value in values {
                redact_json_value(None, value);
            }
        }
        Value::Object(object) => {
            let mut redacted_object = serde_json::Map::new();
            for (key, mut value) in std::mem::take(object) {
                redact_json_value(Some(&key), &mut value);
                redacted_object.insert(redact_text(&key), value);
            }
            *object = redacted_object;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Redacts one JSON string value, including embedded blobs.
fn redact_json_string_value(value: &str) -> String {
    let trimmed = value.trim_start();
    if trimmed.starts_with("data:") && trimmed.contains(";base64,") {
        return REDACTED_BLOB.to_owned();
    }
    if looks_like_base64_blob(trimmed) {
        return REDACTED_BLOB.to_owned();
    }
    redact_text(value)
}

/// Returns whether one JSON key conventionally carries secret material.
fn secret_like_key(key: &str) -> bool {
    let parts = secret_key_parts(key);
    if is_secret_metadata_key(&parts) {
        return false;
    }

    has_secret_subject_parts(&parts)
}

/// Returns whether one key describes secret metadata rather than secret material.
fn is_secret_metadata_key(parts: &[String]) -> bool {
    let Some(last_part) = parts.last() else {
        return false;
    };
    if !matches!(last_part.as_str(), "source" | "provider" | "type" | "mode") {
        return false;
    }

    let subject_parts = &parts[..parts.len().saturating_sub(1)];
    has_secret_subject_parts(subject_parts)
}

/// Returns whether normalized key parts describe secret-bearing data.
fn has_secret_subject_parts(parts: &[String]) -> bool {
    if parts.iter().any(|part| {
        matches!(
            part.as_str(),
            "authorization"
                | "cookie"
                | "credential"
                | "credentials"
                | "passwd"
                | "password"
                | "secret"
                | "signature"
                | "token"
        )
    }) {
        return true;
    }

    contains_part_sequence(parts, &["api", "key"])
        || contains_part_sequence(parts, &["private", "key"])
        || contains_part_sequence(parts, &["client", "secret"])
        || contains_part_sequence(parts, &["access", "key"])
        || contains_part_sequence(parts, &["security", "token"])
}

/// Returns whether one string is probably a bulky encoded blob.
fn looks_like_base64_blob(value: &str) -> bool {
    value.len() >= 512
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'-')
        })
}

/// Splits one key name into lowercase semantic parts, including camelCase boundaries.
fn secret_key_parts(key: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(key.len());
    let mut previous_lower_or_digit = false;
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && previous_lower_or_digit && !normalized.ends_with('_') {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
            previous_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !normalized.ends_with('_') {
                normalized.push('_');
            }
            previous_lower_or_digit = false;
        }
    }
    normalized
        .split('_')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Returns whether one normalized key contains a consecutive part sequence.
fn contains_part_sequence(parts: &[String], sequence: &[&str]) -> bool {
    parts.windows(sequence.len()).any(|window| {
        window
            .iter()
            .map(String::as_str)
            .eq(sequence.iter().copied())
    })
}

/// Compiles one static redaction regex.
fn compile_regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid regex pattern `{pattern}`: {error}"))
}

#[cfg(test)]
mod tests {
    use darc_rollout::model::{
        NormalizedTurn, NormalizedTurnMessage, NormalizedTurnStatus, NormalizedTurnStep,
    };

    use super::*;

    #[test]
    fn redact_text_handles_common_secret_shapes() {
        let private_key = "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----";
        let input = format!(
            "key sk-proj-abcdefghijklmnop bearer Bearer abcdefghijklmnop \
             aws AKIA1234567890ABCDEF gh ghp_abcdefghijklmnopqrstuvwxyz123456 \
             jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signatureabc \
             env API_TOKEN=supersecret github_token=lowercase PRIVATE_KEY=privatevalue \
             PASSWORD=\"quoted secret value\" --password \"flag secret value\" \
             flag --token topsecret \
             url https://user:pass@example.com/path?Signature=abc123 {private_key}"
        );

        let redacted = redact_text(&input);

        assert!(!redacted.contains("sk-proj-abcdefghijklmnop"));
        assert!(!redacted.contains("abcdefghijklmnop"));
        assert!(!redacted.contains("AKIA1234567890ABCDEF"));
        assert!(!redacted.contains("ghp_abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!redacted.contains("supersecret"));
        assert!(!redacted.contains("lowercase"));
        assert!(!redacted.contains("privatevalue"));
        assert!(!redacted.contains("quoted secret value"));
        assert!(!redacted.contains("flag secret value"));
        assert!(!redacted.contains("topsecret"));
        assert!(!redacted.contains("user:pass"));
        assert!(!redacted.contains("Signature=abc123"));
        assert!(!redacted.contains("BEGIN PRIVATE KEY"));
        assert!(redacted.contains(REDACTED_SECRET));
        assert!(redacted.contains(REDACTED_CREDENTIALS));
    }

    #[test]
    fn redact_text_replaces_local_home_paths() {
        let redacted =
            redact_text("read /Users/alice/project/.env and C:\\Users\\alice\\project\\secret.txt");

        assert!(!redacted.contains("alice"));
        assert_eq!(
            redacted,
            "read /Users/[REDACTED_HOME]/project/.env and C:\\Users\\[REDACTED_HOME]\\project\\secret.txt"
        );
    }

    #[test]
    fn redact_text_ignores_synthetic_relative_home_paths() {
        let input = r#"$runtime/home/alice/state /tmp/home/bob/cache"#;

        assert_eq!(redact_text(input), input);
    }

    #[test]
    fn redact_text_handles_inline_posix_home_paths() {
        let input = "`/Users/alice/project` </home/bob/.ssh> file:///Users/carol/src {\"/home/dave/.env\"}\n/Users/erin/.env";

        let redacted = redact_text(input);

        for name in ["alice", "bob", "carol", "dave", "erin"] {
            assert!(!redacted.contains(name));
        }
        assert!(redacted.contains("`/Users/[REDACTED_HOME]/project`"));
        assert!(redacted.contains("</home/[REDACTED_HOME]/.ssh>"));
        assert!(redacted.contains("file:///Users/[REDACTED_HOME]/src"));
        assert!(redacted.contains(r#"{"/home/[REDACTED_HOME]/.env"}"#));
        assert!(redacted.contains("\n/Users/[REDACTED_HOME]/.env"));
    }

    #[test]
    fn redact_text_limits_basic_auth_to_valid_header_contexts() {
        let input = concat!(
            "Authorization: Basic dTpw ",
            "Authorization: Basic dXNlcjpwYXNz ",
            "Authorization: Basic YXBpX2tleTo= ",
            "Authorization: Basic OnBhc3N3b3Jk ",
            "Authorization: Basic dXPDqXI6cMOkc3M= ",
            "-H 'Authorization: Basic YWxpY2U6c2VjcmV0' ",
            "headers[\"Authorization\"] = \"Basic YXBpX2tleTo=\" ",
            "Basic implementation remains visible ",
            "Authorization: Basic abcdefghijklmnop"
        );

        let redacted = redact_text(input);

        assert!(!redacted.contains("dTpw"));
        assert!(!redacted.contains("dXNlcjpwYXNz"));
        assert!(!redacted.contains("YXBpX2tleTo="));
        assert!(!redacted.contains("OnBhc3N3b3Jk"));
        assert!(!redacted.contains("dXPDqXI6cMOkc3M="));
        assert!(!redacted.contains("YWxpY2U6c2VjcmV0"));
        assert!(redacted.contains("Authorization: Basic [REDACTED_SECRET]"));
        assert!(redacted.contains("-H 'Authorization: Basic [REDACTED_SECRET]"));
        assert!(redacted.contains("headers[\"Authorization\"] = \"Basic [REDACTED_SECRET]\""));
        assert!(redacted.contains("Basic implementation remains visible"));
        assert!(redacted.contains("Authorization: Basic abcdefghijklmnop"));
    }

    #[test]
    fn redact_text_avoids_known_secret_assignment_false_positives() {
        let input = concat!(
            "tokenize='unicode61' ",
            "apiKeySource: \"ANTHROPIC_API_KEY\" ",
            "codex_app_server_protocol::protocol::common::FuzzyFileSearchParamsSuperset ",
            "query LIKE '%--token=%' ",
            "rg -n -S 'token=' ",
            "--token=**** --token=%%%% --token=\"****\" --token '%%%%'"
        );

        assert_eq!(redact_text(input), input);
    }

    #[test]
    fn redact_text_still_redacts_explicit_secret_assignments_and_flags() {
        let redacted = redact_text(
            "TOKEN=secretvalue API_TOKEN=supersecret SLACK_BOT_TOKEN=xoxb-secret \
             STRIPE_SECRET_KEY=stripe-secret AUTHORIZATION=opaque-secret \
             Authorization:\"Token abcdef\" password=\"quoted secret\" \
             --password=p%40ssword --token abc%2Fdef --token flagsecret \
             --github-token=github-secret --openai-api-key=\"openai-secret\" \
             --session-token=session%2Fsecret --passwd='passwd-secret'",
        );

        assert!(!redacted.contains("secretvalue"));
        assert!(!redacted.contains("supersecret"));
        assert!(!redacted.contains("xoxb-secret"));
        assert!(!redacted.contains("stripe-secret"));
        assert!(!redacted.contains("opaque-secret"));
        assert!(!redacted.contains("Token abcdef"));
        assert!(!redacted.contains("quoted secret"));
        assert!(!redacted.contains("p%40ssword"));
        assert!(!redacted.contains("abc%2Fdef"));
        assert!(!redacted.contains("%40ssword"));
        assert!(!redacted.contains("%2Fdef"));
        assert!(!redacted.contains("flagsecret"));
        assert!(!redacted.contains("github-secret"));
        assert!(!redacted.contains("openai-secret"));
        assert!(!redacted.contains("session%2Fsecret"));
        assert!(!redacted.contains("passwd-secret"));
        assert!(redacted.contains("--password=[REDACTED_SECRET]"));
        assert!(redacted.contains("--token [REDACTED_SECRET]"));
        assert!(redacted.contains("--openai-api-key=\"[REDACTED_SECRET]\""));
        assert!(redacted.contains("--passwd='[REDACTED_SECRET]'"));
    }

    #[test]
    fn redact_json_text_preserves_shape_and_redacts_secret_keys() {
        let json = r#"{
            "api_key": "plain-secret",
            "OPENAI_API_KEY": "prefixed-secret",
            "clientSecret": "camel-secret",
            "aws_secret_access_key": "compound-secret",
            "apiKeySource": "ANTHROPIC_API_KEY",
            "nested": {
                "Authorization": "Bearer abcdefghijklmnop",
                "refreshToken": "refresh-secret",
                "tokenSource": "env",
                "image_url": "data:image/png;base64,abcdef"
            },
            "path": "/Users/alice/project/src/lib.rs",
            "sk-proj-secretkeyname123456": true,
            "/Users/alice/project/.env": "read"
        }"#;

        let redacted = redact_json_text(json);
        let value: Value = serde_json::from_str(&redacted).expect("redacted JSON should parse");

        assert_eq!(value["api_key"], REDACTED_SECRET);
        assert_eq!(value["OPENAI_API_KEY"], REDACTED_SECRET);
        assert_eq!(value["clientSecret"], REDACTED_SECRET);
        assert_eq!(value["aws_secret_access_key"], REDACTED_SECRET);
        assert_eq!(value["apiKeySource"], "ANTHROPIC_API_KEY");
        assert_eq!(value["nested"]["Authorization"], REDACTED_SECRET);
        assert_eq!(value["nested"]["refreshToken"], REDACTED_SECRET);
        assert_eq!(value["nested"]["tokenSource"], "env");
        assert_eq!(value["nested"]["image_url"], REDACTED_BLOB);
        assert_eq!(value["path"], "/Users/[REDACTED_HOME]/project/src/lib.rs");
        let serialized = serde_json::to_string(&value).expect("redacted JSON should serialize");
        assert!(!serialized.contains("sk-proj-secretkeyname123456"));
        assert!(!serialized.contains("alice"));
    }

    #[test]
    fn redact_json_text_redacts_large_base64_string_values() {
        let blob = "A".repeat(512);
        let json = format!(r#"{{"payload":"{blob}","name":"kept"}}"#);

        let redacted = redact_json_text(&json);
        let value: Value = serde_json::from_str(&redacted).expect("redacted JSON should parse");

        assert_eq!(value["payload"], REDACTED_BLOB);
        assert_eq!(value["name"], "kept");
    }

    #[test]
    fn credential_key_terms_redact_across_text_flags_and_json() {
        let cases = [
            ("apiKey", "apiKey", "--api-key"),
            ("accessKey", "accessKey", "--access-key"),
            ("accessToken", "accessToken", "--access-token"),
            ("clientSecret", "clientSecret", "--client-secret"),
            ("privateKey", "privateKey", "--private-key"),
            ("refreshToken", "refreshToken", "--refresh-token"),
        ];

        for (json_key, assignment_key, flag) in cases {
            let json = format!(r#"{{"{json_key}":"json-secret"}}"#);
            let value: Value =
                serde_json::from_str(&redact_json_text(&json)).expect("redacted JSON should parse");
            assert_eq!(value[json_key], REDACTED_SECRET);

            let assignment = redact_text(&format!("{assignment_key}=assignment-secret"));
            assert!(!assignment.contains("assignment-secret"));

            let flag_value = redact_text(&format!("{flag} flag-secret"));
            assert!(!flag_value.contains("flag-secret"));
        }
    }

    #[test]
    fn redact_text_redacts_large_base64_runs() {
        let blob = "A".repeat(512);
        let redacted = redact_text(&format!("stdout {blob} done"));

        assert!(!redacted.contains(&blob));
        assert!(redacted.contains(REDACTED_BLOB));
    }

    #[test]
    fn redact_normalized_turn_covers_indexed_text_surfaces() {
        let mut turn = NormalizedTurn {
            turn_id: Some("turn-1".to_owned()),
            user_message: "Use token=secretvalue".to_owned(),
            final_answer: Some(NormalizedTurnMessage {
                timestamp: "2026-04-01T00:00:04Z".to_owned(),
                text: "The key is sk-proj-abcdefghijklmnop".to_owned(),
            }),
            started_at: "2026-04-01T00:00:00Z".to_owned(),
            completed_at: Some("2026-04-01T00:00:04Z".to_owned()),
            status: NormalizedTurnStatus::Completed,
            primary_model: None,
            token_usage: None,
            steps: vec![
                NormalizedTurnStep::Commentary {
                    timestamp: "2026-04-01T00:00:01Z".to_owned(),
                    text: "Reading /Users/alice/project/.env".to_owned(),
                },
                NormalizedTurnStep::ToolCall {
                    timestamp: "2026-04-01T00:00:02Z".to_owned(),
                    call_id: "call-1".to_owned(),
                    name: "exec_command".to_owned(),
                    arguments: r#"{"cmd":"curl -H 'Authorization: Bearer abcdefghijklmnop' https://example.com"}"#.to_owned(),
                },
                NormalizedTurnStep::ToolCallOutput {
                    timestamp: "2026-04-01T00:00:03Z".to_owned(),
                    call_id: "call-1".to_owned(),
                    output: r#"{"stdout":"password=hunter2"}"#.to_owned(),
                },
                NormalizedTurnStep::Attachment {
                    timestamp: "2026-04-01T00:00:04Z".to_owned(),
                    attachment_type: "image".to_owned(),
                    payload_json: r#"{"image_url":"data:image/png;base64,abcdef"}"#.to_owned(),
                },
            ],
        };

        redact_normalized_turn(&mut turn);
        let serialized = serde_json::to_string(&turn.steps).expect("steps should serialize");
        let all_text = format!(
            "{}\n{}\n{}",
            turn.user_message,
            turn.final_answer.as_ref().expect("final answer").text,
            serialized
        );

        assert!(!all_text.contains("secretvalue"));
        assert!(!all_text.contains("sk-proj-abcdefghijklmnop"));
        assert!(!all_text.contains("alice"));
        assert!(!all_text.contains("abcdefghijklmnop"));
        assert!(!all_text.contains("hunter2"));
        assert!(!all_text.contains("data:image/png"));
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact_text("TOKEN=secretvalue /Users/alice/project");
        let twice = redact_text(&once);

        assert_eq!(once, twice);
    }
}
