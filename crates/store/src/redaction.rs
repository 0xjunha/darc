use std::{borrow::Cow, sync::LazyLock};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use darc_rollout::model::{NormalizedTurn, NormalizedTurnStep};
use regex::Regex;
use serde_json::Value;

const REDACTED_BLOB: &str = "[REDACTED_BLOB]";
const REDACTED_CREDENTIALS: &str = "[REDACTED_CREDENTIALS]";
const REDACTED_HOME: &str = "[REDACTED_HOME]";
const REDACTED_SECRET: &str = "[REDACTED_SECRET]";
#[cfg(test)]
const SECRET_KEY_REGEX_FRAGMENT: &str = r"(?i-u:(?:[A-Za-z0-9]+[_-])*(?:openai[_-]?api[_-]?key|anthropic[_-]?api[_-]?key|aws[_-]?secret[_-]?access[_-]?key|aws[_-]?access[_-]?key(?:[_-]?id)?|aws[_-]?session[_-]?token|github[_-]?token|personal[_-]?access[_-]?token|api[_-]?token|auth[_-]?token|bearer[_-]?token|session[_-]?token|api[_-]?key|access[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|private[_-]?key|client[_-]?secret|secret[_-]?key|proxy[_-]?authorization|authorization|credentials?|password|passwd|secret|token|cookie)|[A-Za-z0-9]*(?:apiKey|privateKey|clientSecret|accessKey|accessToken|refreshToken|idToken|authToken|bearerToken|sessionToken|personalAccessToken|secretKey|proxyAuthorization|authorization|credentials?|password|passwd|secret|token|cookie))";
const SECRET_FLAG_REGEX_FRAGMENT: &str = r"(?i-u:(?:[A-Za-z0-9]+-)*(?:openai-api-key|anthropic-api-key|aws-secret-access-key|aws-access-key(?:-id)?|aws-session-token|github-token|personal-access-token|api-token|auth-token|bearer-token|session-token|api-key|access-key|access-token|refresh-token|id-token|private-key|client-secret|secret-key|proxy-authorization|authorization|credentials?|password|passwd|secret|token|cookie))";
const SIGNED_QUERY_PARAMETER_NAMES: &[&str] = &[
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "signature",
    "sig",
    "access_token",
    "access-token",
    "accesstoken",
    "refresh_token",
    "refresh-token",
    "refreshtoken",
    "id_token",
    "id-token",
    "idtoken",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "key",
    "password",
    "client_secret",
    "client-secret",
    "clientsecret",
];
const DELIMITED_SECRET_KEY_PATTERNS: &[&[&str]] = &[
    &["openai", "api", "key"],
    &["anthropic", "api", "key"],
    &["aws", "secret", "access", "key"],
    &["aws", "access", "key"],
    &["aws", "access", "key", "id"],
    &["aws", "session", "token"],
    &["github", "token"],
    &["personal", "access", "token"],
    &["api", "token"],
    &["auth", "token"],
    &["bearer", "token"],
    &["session", "token"],
    &["api", "key"],
    &["access", "key"],
    &["access", "token"],
    &["refresh", "token"],
    &["id", "token"],
    &["private", "key"],
    &["client", "secret"],
    &["secret", "key"],
    &["proxy", "authorization"],
    &["authorization"],
    &["credential"],
    &["credentials"],
    &["password"],
    &["passwd"],
    &["secret"],
    &["token"],
    &["cookie"],
];
const COMPACT_SECRET_KEY_SUFFIXES: &[&str] = &[
    "apikey",
    "privatekey",
    "clientsecret",
    "accesskey",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "authtoken",
    "bearertoken",
    "sessiontoken",
    "personalaccesstoken",
    "secretkey",
    "proxyauthorization",
    "authorization",
    "credential",
    "credentials",
    "password",
    "passwd",
    "secret",
    "token",
    "cookie",
];
const TEXT_REDACTION_PREFILTER_PATTERNS: &[&str] = &[
    "private key",
    "data:",
    "base64,",
    "sk-",
    "akia",
    "asia",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "npm_",
    "xox",
    "eyj",
    "bearer",
    "basic",
    "authorization",
    "proxy-authorization",
    "header",
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "cookie",
    "--",
    "://",
    "/users/",
    "/home/",
    ":\\users\\",
];

static PRIVATE_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
    )
});
static DATA_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r"[dD][aA][tT][aA]:[A-Za-z0-9.+/-]+/[^;\s]+;[bB][aA][sS][eE]64,[A-Za-z0-9+/=_-]+")
});
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
#[cfg(test)]
static SECRET_ASSIGNMENT_DOUBLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"\b({SECRET_KEY_REGEX_FRAGMENT})\b(\s*[:=]\s*)"([^\s"\r\n][^"\r\n]{{3,}})""#
    ))
});
#[cfg(test)]
static SECRET_ASSIGNMENT_SINGLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"\b({SECRET_KEY_REGEX_FRAGMENT})\b(\s*[:=]\s*)'([^\s'\r\n][^'\r\n]{{3,}})'"#
    ))
});
#[cfg(test)]
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"\b({SECRET_KEY_REGEX_FRAGMENT})\b(\s*[:=]\s*)([^\s"',;`}}{{]{{4,}})"#
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
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}=)([^\s"',;`}}{{]+)"#
    ))
});
static SECRET_FLAG_VALUE_DOUBLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}[ \t]+)"([^"\r\n]{{4,}})""#
    ))
});
static SECRET_FLAG_VALUE_SINGLE_QUOTED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}[ \t]+)'([^'\r\n]{{4,}})'"#
    ))
});
static SECRET_FLAG_VALUE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(&format!(
        r#"(--{SECRET_FLAG_REGEX_FRAGMENT}[ \t]+)([^\s"',;`}}{{]+)"#
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
static TEXT_REDACTION_PREFILTER: LazyLock<AhoCorasick> = LazyLock::new(|| {
    AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .build(TEXT_REDACTION_PREFILTER_PATTERNS)
        .expect("valid text redaction prefilter patterns")
});

/// Lowercases one ASCII byte without changing other bytes.
fn ascii_lower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}

/// Returns whether haystack starts with needle case-insensitively at one byte index.
fn starts_with_ascii_case_insensitive_at(haystack: &[u8], index: usize, needle: &[u8]) -> bool {
    haystack
        .get(index..index.saturating_add(needle.len()))
        .is_some_and(|candidate| {
            candidate
                .iter()
                .zip(needle.iter())
                .all(|(left, right)| ascii_lower(*left) == ascii_lower(*right))
        })
}

/// Returns whether one string starts with an ASCII needle ignoring case.
fn starts_with_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    starts_with_ascii_case_insensitive_at(haystack.as_bytes(), 0, needle.as_bytes())
}

/// Returns whether one string ends with an ASCII needle ignoring case.
fn ends_with_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    haystack
        .len()
        .checked_sub(needle.len())
        .is_some_and(|start| starts_with_ascii_case_insensitive_at(haystack, start, needle))
}

/// Records which redaction detectors can possibly match one text payload.
#[derive(Debug, Default, Clone, Copy)]
struct TextRedactionPlan {
    private_key: bool,
    data_url: bool,
    openai_or_anthropic_key: bool,
    aws_access_key_id: bool,
    github_token: bool,
    npm_token: bool,
    slack_token: bool,
    jwt: bool,
    bearer_token: bool,
    basic_auth: bool,
    secret_assignment: bool,
    secret_flag: bool,
    url_credentials: bool,
    signed_query_param: bool,
    base64_blob: bool,
    posix_home: bool,
    windows_home: bool,
}

impl TextRedactionPlan {
    /// Builds a conservative detector plan with cheap literal scans.
    fn inspect(input: &str) -> Self {
        let bytes = input.as_bytes();
        let mut plan = Self {
            base64_blob: bytes.len() >= 512,
            ..Self::default()
        };
        let mut has_basic = false;
        let mut has_auth_context = false;
        let mut has_assignment_separator = false;
        let mut has_double_dash = false;
        let mut has_query_separator = false;
        let mut has_secret_trigger = false;
        let mut has_base64_marker = false;

        for byte in bytes {
            match byte {
                b':' | b'=' => has_assignment_separator = true,
                b'?' | b'&' => has_query_separator = true,
                _ => {}
            }
        }

        for mat in TEXT_REDACTION_PREFILTER.find_iter(input) {
            match mat.pattern().as_usize() {
                0 => plan.private_key = true,
                1 => plan.data_url = true,
                2 => has_base64_marker = true,
                3 => plan.openai_or_anthropic_key = true,
                4 | 5 => plan.aws_access_key_id = true,
                6..=11 => plan.github_token = true,
                12 => plan.npm_token = true,
                13 => plan.slack_token = true,
                14 => plan.jwt = true,
                15 => plan.bearer_token = true,
                16 => has_basic = true,
                17 | 18 => {
                    has_auth_context = true;
                    has_secret_trigger = true;
                }
                19 => has_auth_context = true,
                20..=26 => has_secret_trigger = true,
                27 => has_double_dash = true,
                28 => plan.url_credentials = true,
                29 | 30 => plan.posix_home = true,
                31 => plan.windows_home = true,
                _ => {}
            }
        }

        plan.data_url = plan.data_url && has_base64_marker;
        plan.jwt = plan.jwt && bytes.contains(&b'.');
        plan.basic_auth = has_basic && has_auth_context;
        plan.secret_assignment = has_assignment_separator && has_secret_trigger;
        plan.secret_flag = has_double_dash && has_secret_trigger;
        plan.signed_query_param = has_assignment_separator
            && has_query_separator
            && may_contain_signed_query_parameter(input);
        plan
    }

    /// Returns whether any redaction detector should run.
    fn has_work(self) -> bool {
        self.private_key
            || self.data_url
            || self.openai_or_anthropic_key
            || self.aws_access_key_id
            || self.github_token
            || self.npm_token
            || self.slack_token
            || self.jwt
            || self.bearer_token
            || self.basic_auth
            || self.secret_assignment
            || self.secret_flag
            || self.url_credentials
            || self.signed_query_param
            || self.base64_blob
            || self.posix_home
            || self.windows_home
    }
}

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
    let plan = TextRedactionPlan::inspect(input);
    if !plan.has_work() {
        return input.to_owned();
    }

    let mut redacted = None;
    if plan.private_key {
        apply_regex_replacement(&mut redacted, input, &PRIVATE_KEY_REGEX, REDACTED_SECRET);
    }
    if plan.data_url {
        apply_regex_replacement(&mut redacted, input, &DATA_URL_REGEX, REDACTED_BLOB);
    }
    if plan.openai_or_anthropic_key {
        apply_regex_replacement(&mut redacted, input, &OPENAI_KEY_REGEX, REDACTED_SECRET);
        apply_regex_replacement(&mut redacted, input, &ANTHROPIC_KEY_REGEX, REDACTED_SECRET);
    }
    if plan.aws_access_key_id {
        apply_regex_replacement(
            &mut redacted,
            input,
            &AWS_ACCESS_KEY_ID_REGEX,
            REDACTED_SECRET,
        );
    }
    if plan.github_token {
        apply_regex_replacement(&mut redacted, input, &GITHUB_TOKEN_REGEX, REDACTED_SECRET);
    }
    if plan.npm_token {
        apply_regex_replacement(&mut redacted, input, &NPM_TOKEN_REGEX, REDACTED_SECRET);
    }
    if plan.slack_token {
        apply_regex_replacement(&mut redacted, input, &SLACK_TOKEN_REGEX, REDACTED_SECRET);
    }
    if plan.jwt {
        apply_regex_replacement(&mut redacted, input, &JWT_REGEX, REDACTED_SECRET);
    }
    if plan.bearer_token {
        apply_regex_replacement(
            &mut redacted,
            input,
            &BEARER_TOKEN_REGEX,
            "Bearer [REDACTED_SECRET]",
        );
    }
    if plan.basic_auth {
        apply_computed_redaction(&mut redacted, input, redact_basic_auth);
    }
    if plan.secret_assignment {
        apply_computed_redaction(&mut redacted, input, redact_secret_assignments);
    }
    if plan.secret_flag {
        apply_computed_redaction(&mut redacted, input, |current| {
            redact_quoted_secret_flag(current, &SECRET_FLAG_EQUALS_DOUBLE_QUOTED_REGEX, '"')
        });
        apply_computed_redaction(&mut redacted, input, |current| {
            redact_quoted_secret_flag(current, &SECRET_FLAG_EQUALS_SINGLE_QUOTED_REGEX, '\'')
        });
        apply_computed_redaction(&mut redacted, input, |current| {
            redact_unquoted_secret_flag(current, &SECRET_FLAG_EQUALS_REGEX)
        });
        apply_computed_redaction(&mut redacted, input, |current| {
            redact_quoted_secret_flag(current, &SECRET_FLAG_VALUE_DOUBLE_QUOTED_REGEX, '"')
        });
        apply_computed_redaction(&mut redacted, input, |current| {
            redact_quoted_secret_flag(current, &SECRET_FLAG_VALUE_SINGLE_QUOTED_REGEX, '\'')
        });
        apply_computed_redaction(&mut redacted, input, |current| {
            redact_unquoted_secret_flag(current, &SECRET_FLAG_VALUE_REGEX)
        });
    }
    if plan.url_credentials {
        let credentials_replacement = format!("$1{REDACTED_CREDENTIALS}@");
        apply_regex_replacement(
            &mut redacted,
            input,
            &URL_CREDENTIALS_REGEX,
            credentials_replacement.as_str(),
        );
    }
    if plan.signed_query_param {
        apply_regex_replacement(
            &mut redacted,
            input,
            &SIGNED_QUERY_PARAM_REGEX,
            "$1[REDACTED_SECRET]",
        );
    }
    if plan.base64_blob {
        apply_computed_redaction(&mut redacted, input, redact_base64_blobs);
    }
    if plan.posix_home {
        apply_computed_redaction(&mut redacted, input, redact_posix_home_paths);
    }
    if plan.windows_home {
        let windows_home_replacement = format!("$1{REDACTED_HOME}");
        apply_regex_replacement(
            &mut redacted,
            input,
            &WINDOWS_HOME_PATH_REGEX,
            windows_home_replacement.as_str(),
        );
    }
    redacted.unwrap_or_else(|| input.to_owned())
}

/// Returns the current redaction buffer or the original input when unchanged.
fn current_text<'a>(input: &'a str, redacted: &'a Option<String>) -> &'a str {
    redacted.as_deref().unwrap_or(input)
}

/// Stores one optional redaction result.
fn apply_redaction(redacted: &mut Option<String>, value: Option<String>) {
    if let Some(value) = value {
        *redacted = Some(value);
    }
}

/// Applies a callback redaction against the current text buffer.
fn apply_computed_redaction(
    redacted: &mut Option<String>,
    input: &str,
    redaction: impl FnOnce(&str) -> Option<String>,
) {
    let value = {
        let current = current_text(input, redacted);
        redaction(current)
    };
    apply_redaction(redacted, value);
}

/// Applies one simple regex replacement only when it changes the current text.
fn apply_regex_replacement(
    redacted: &mut Option<String>,
    input: &str,
    regex: &Regex,
    replacement: &str,
) {
    let replacement = {
        let current = current_text(input, redacted);
        match regex.replace_all(current, replacement) {
            Cow::Borrowed(_) => None,
            Cow::Owned(value) => Some(value),
        }
    };
    apply_redaction(redacted, replacement);
}

/// Redacts Basic auth credentials only when they appear in header contexts.
fn redact_basic_auth(input: &str) -> Option<String> {
    match BASIC_AUTH_CONTEXT_REGEX.replace_all(input, |captures: &regex::Captures<'_>| {
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
    }) {
        Cow::Borrowed(_) => None,
        Cow::Owned(value) => Some(value),
    }
}

/// Redacts generic secret assignments unless a long CLI flag should handle them.
fn redact_secret_assignments(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = String::new();
    let mut last = 0;
    let mut index = 0;
    while index < bytes.len() {
        if !matches!(bytes[index], b':' | b'=') {
            index += 1;
            continue;
        }
        match scan_secret_assignment(input, index) {
            Some(SecretAssignmentScan::Redact(redaction_match)) => {
                if redaction_match.key_start < last {
                    index += 1;
                    continue;
                }
                output.push_str(&input[last..redaction_match.key_start]);
                output.push_str(redaction_match.key);
                output.push_str(redaction_match.separator);
                if let Some(quote) = redaction_match.quote {
                    output.push(quote);
                    output.push_str(REDACTED_SECRET);
                    output.push(quote);
                } else {
                    output.push_str(REDACTED_SECRET);
                }
                last = redaction_match.full_end;
                index = redaction_match.full_end;
            }
            Some(SecretAssignmentScan::Skip { full_end }) => {
                index = full_end;
            }
            None => {
                index += 1;
            }
        }
    }

    if last == 0 {
        return None;
    }
    output.push_str(&input[last..]);
    Some(output)
}

/// Describes how a parsed secret assignment candidate should affect scanning.
#[derive(Debug, Clone, Copy)]
enum SecretAssignmentScan<'a> {
    Redact(SecretAssignmentMatch<'a>),
    Skip { full_end: usize },
}

/// Stores one parsed secret assignment redaction.
#[derive(Debug, Clone, Copy)]
struct SecretAssignmentMatch<'a> {
    key_start: usize,
    full_end: usize,
    key: &'a str,
    separator: &'a str,
    quote: Option<char>,
}

/// Parses one secret assignment around a known `:` or `=` byte position.
fn scan_secret_assignment(input: &str, separator_index: usize) -> Option<SecretAssignmentScan<'_>> {
    let bytes = input.as_bytes();
    let mut key_end = separator_index;
    while key_end > 0 && bytes[key_end - 1].is_ascii_whitespace() {
        key_end -= 1;
    }

    let mut key_start = key_end;
    while key_start > 0 && is_assignment_key_byte(bytes[key_start - 1]) {
        key_start -= 1;
    }
    if key_start == key_end || !has_word_boundary_before(input, key_start) {
        return None;
    }
    if is_embedded_long_flag_assignment(input, key_start) {
        return None;
    }

    let key = &input[key_start..key_end];
    if !matches_secret_assignment_key(key) {
        return None;
    }

    let mut value_start = separator_index + 1;
    while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
        value_start += 1;
    }
    if value_start >= bytes.len() {
        return None;
    }

    let (value, full_end, quote) = parse_assignment_value(input, value_start)?;
    let separator = &input[key_end..value_start];
    if is_comparison_operator_assignment(input, separator_index, value)
        || is_authorization_scheme_assignment(key, separator, value)
        || is_authorization_search_pattern_assignment(key, separator, value)
        || is_non_secret_literal_assignment_value(value)
    {
        return Some(SecretAssignmentScan::Skip { full_end });
    }

    Some(SecretAssignmentScan::Redact(SecretAssignmentMatch {
        key_start,
        full_end,
        key,
        separator,
        quote,
    }))
}

/// Parses one quoted or unquoted assignment value.
fn parse_assignment_value(input: &str, value_start: usize) -> Option<(&str, usize, Option<char>)> {
    let bytes = input.as_bytes();
    match bytes[value_start] {
        b'"' | b'\'' => parse_quoted_assignment_value(input, value_start),
        byte if is_unquoted_assignment_value_byte(byte) => {
            let mut value_end = value_start;
            for (relative_index, ch) in input[value_start..].char_indices() {
                if !is_unquoted_assignment_value_char(ch) {
                    break;
                }
                value_end = value_start + relative_index + ch.len_utf8();
            }
            let value = &input[value_start..value_end];
            (value.len() >= 4).then_some((value, value_end, None))
        }
        _ => None,
    }
}

/// Parses one quoted assignment value and returns its unquoted content.
fn parse_quoted_assignment_value(
    input: &str,
    value_start: usize,
) -> Option<(&str, usize, Option<char>)> {
    let bytes = input.as_bytes();
    let quote = bytes[value_start];
    let content_start = value_start + 1;
    let mut content_end = content_start;
    while content_end < bytes.len() && bytes[content_end] != quote {
        if matches!(bytes[content_end], b'\r' | b'\n') {
            return None;
        }
        content_end += 1;
    }
    if content_end >= bytes.len() || bytes[content_end] != quote {
        return None;
    }

    let value = &input[content_start..content_end];
    let first = value.chars().next()?;
    if value.len() < 4 || first.is_whitespace() || first == char::from(quote) {
        return None;
    }
    Some((value, content_end + 1, Some(char::from(quote))))
}

/// Returns whether one byte can appear in an assignment key.
fn is_assignment_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

/// Returns whether one byte can be part of an unquoted assignment value.
fn is_unquoted_assignment_value_byte(byte: u8) -> bool {
    !matches!(byte, b'"' | b'\'' | b',' | b';' | b'`' | b'}' | b'{') && !byte.is_ascii_whitespace()
}

/// Returns whether one char can be part of an unquoted assignment value.
fn is_unquoted_assignment_value_char(ch: char) -> bool {
    !ch.is_whitespace() && !matches!(ch, '"' | '\'' | ',' | ';' | '`' | '}' | '{')
}

/// Returns whether a byte index starts at a regex word boundary.
fn has_word_boundary_before(input: &str, index: usize) -> bool {
    let Some(previous) = input[..index].chars().next_back() else {
        return true;
    };
    let Some(current) = input[index..].chars().next() else {
        return true;
    };
    is_regex_word_char(previous) != is_regex_word_char(current)
}

/// Returns whether one char is a regex word char for ASCII keys.
fn is_regex_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// Returns whether one key matches the generic secret-assignment regex key grammar.
fn matches_secret_assignment_key(key: &str) -> bool {
    key.bytes().all(is_assignment_key_byte)
        && (matches_delimited_secret_assignment_key(key)
            || matches_compact_secret_assignment_key(key))
}

/// Returns whether one key matches the delimited branch of the assignment regex key grammar.
fn matches_delimited_secret_assignment_key(key: &str) -> bool {
    if key.starts_with(['_', '-']) || key.ends_with(['_', '-']) {
        return false;
    }

    let mut terminal_start = 0;
    loop {
        if valid_assignment_key_prefix(&key[..terminal_start])
            && matches_delimited_secret_terminal(&key[terminal_start..])
        {
            return true;
        }
        let Some(relative_separator) = key[terminal_start..].find(['_', '-']) else {
            break;
        };
        terminal_start += relative_separator + 1;
    }
    false
}

/// Returns whether one prefix is zero or more nonempty alnum segments with separators.
fn valid_assignment_key_prefix(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if !prefix.ends_with(['_', '-']) {
        return false;
    }
    prefix
        .trim_end_matches(['_', '-'])
        .split(['_', '-'])
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

/// Returns whether one suffix matches a terminal secret key phrase with optional separators.
fn matches_delimited_secret_terminal(terminal: &str) -> bool {
    DELIMITED_SECRET_KEY_PATTERNS
        .iter()
        .any(|pattern| matches_optional_separator_sequence(terminal, pattern))
}

/// Returns whether input equals a case-insensitive sequence with optional separators.
fn matches_optional_separator_sequence(mut input: &str, sequence: &[&str]) -> bool {
    for (index, part) in sequence.iter().enumerate() {
        if !starts_with_ascii_case_insensitive(input, part) {
            return false;
        }
        input = &input[part.len()..];
        if index + 1 == sequence.len() {
            return input.is_empty();
        }
        if input.starts_with(['_', '-']) {
            input = &input[1..];
        }
    }
    input.is_empty()
}

/// Returns whether one key matches the compact branch of the assignment regex key grammar.
fn matches_compact_secret_assignment_key(key: &str) -> bool {
    key.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && COMPACT_SECRET_KEY_SUFFIXES
            .iter()
            .any(|suffix| ends_with_ascii_case_insensitive(key, suffix))
}

/// Redacts generic assignments with the legacy regex path for scanner equivalence tests.
#[cfg(test)]
fn redact_secret_assignments_with_regex_oracle(input: &str) -> String {
    let redacted = redact_secret_assignment_with_regex(
        input,
        &SECRET_ASSIGNMENT_DOUBLE_QUOTED_REGEX,
        "\"[REDACTED_SECRET]\"",
    );
    let redacted = redact_secret_assignment_with_regex(
        &redacted,
        &SECRET_ASSIGNMENT_SINGLE_QUOTED_REGEX,
        "'[REDACTED_SECRET]'",
    );
    redact_secret_assignment_with_regex(&redacted, &SECRET_ASSIGNMENT_REGEX, REDACTED_SECRET)
}

/// Redacts one legacy regex assignment shape for scanner equivalence tests.
#[cfg(test)]
fn redact_secret_assignment_with_regex(
    input: &str,
    regex: &Regex,
    replacement_value: &str,
) -> String {
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
            if is_regex_comparison_operator_assignment(input, separator, value) {
                return captures[0].to_owned();
            }
            if is_authorization_scheme_assignment(
                secret_key.as_str(),
                separator.as_str(),
                value.as_str(),
            ) {
                return captures[0].to_owned();
            }
            if is_authorization_search_pattern_assignment(
                secret_key.as_str(),
                separator.as_str(),
                value.as_str(),
            ) {
                return captures[0].to_owned();
            }
            if is_non_secret_literal_assignment_value(value.as_str()) {
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

/// Returns whether a legacy regex match is comparison or arrow syntax.
#[cfg(test)]
fn is_regex_comparison_operator_assignment(
    input: &str,
    separator: regex::Match<'_>,
    value: regex::Match<'_>,
) -> bool {
    let Some((relative_operator_index, operator)) = separator
        .as_str()
        .bytes()
        .enumerate()
        .find(|(_, byte)| matches!(byte, b':' | b'='))
    else {
        return false;
    };
    if operator != b'=' {
        return false;
    }

    is_comparison_operator_assignment(
        input,
        separator.start() + relative_operator_index,
        value.as_str(),
    )
}

/// Returns whether an assignment match is the key suffix of a `--flag=value`.
fn is_embedded_long_flag_assignment(input: &str, match_start: usize) -> bool {
    let bytes = input.as_bytes();
    match_start >= 2
        && bytes.get(match_start - 1).is_some_and(|byte| *byte == b'-')
        && bytes.get(match_start - 2).is_some_and(|byte| *byte == b'-')
}

/// Returns whether an assignment match is part of comparison or arrow syntax.
fn is_comparison_operator_assignment(input: &str, operator_index: usize, value: &str) -> bool {
    if input.as_bytes().get(operator_index) != Some(&b'=') {
        return false;
    }
    let bytes = input.as_bytes();
    value.starts_with('=')
        || matches!(bytes.get(operator_index + 1), Some(b'=') | Some(b'>'))
        || operator_index
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| matches!(byte, b'<' | b'>' | b'!'))
}

/// Returns whether one secret-like key names an Authorization header.
fn is_authorization_key(key: &str) -> bool {
    let parts = secret_key_parts(key);
    matches!(parts.as_slice(), [part] if part == "authorization")
        || matches!(parts.as_slice(), [first, second] if first == "proxy" && second == "authorization")
}

/// Returns whether an Authorization header match only captured the auth scheme.
fn is_authorization_scheme_assignment(key: &str, separator: &str, value: &str) -> bool {
    separator.contains(':')
        && is_authorization_key(key)
        && matches!(
            value.to_ascii_lowercase().as_str(),
            "basic" | "bearer" | "digest" | "negotiate" | "ntlm" | "token"
        )
}

/// Returns whether an Authorization match is a search pattern rather than a header.
fn is_authorization_search_pattern_assignment(key: &str, separator: &str, value: &str) -> bool {
    separator.contains(':')
        && is_authorization_key(key)
        && matches!(value.trim_start(), value if value.starts_with('|') || value.starts_with(r"\|"))
}

/// Returns whether a generic assignment value is a non-secret literal.
fn is_non_secret_literal_assignment_value(value: &str) -> bool {
    matches!(
        value
            .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`'))
            .to_ascii_lowercase()
            .as_str(),
        "true" | "false" | "null" | "none" | "nil"
    )
}

/// Returns whether one JSON value is a non-secret placeholder literal.
fn is_non_secret_json_literal(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) => true,
        Value::String(text) => is_non_secret_literal_assignment_value(text),
        Value::Array(_) | Value::Number(_) | Value::Object(_) => false,
    }
}

/// Redacts quoted CLI secret flag values while leaving wildcard-only examples intact.
fn redact_quoted_secret_flag(input: &str, regex: &Regex, quote: char) -> Option<String> {
    match regex.replace_all(input, |captures: &regex::Captures<'_>| {
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
    }) {
        Cow::Borrowed(_) => None,
        Cow::Owned(value) => Some(value),
    }
}

/// Redacts unquoted CLI secret flag values while leaving wildcard-only examples intact.
fn redact_unquoted_secret_flag(input: &str, regex: &Regex) -> Option<String> {
    match regex.replace_all(input, |captures: &regex::Captures<'_>| {
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
    }) {
        Cow::Borrowed(_) => None,
        Cow::Owned(value) => Some(value),
    }
}

/// Returns whether a secret-looking flag value is only a shell, SQL, or glob wildcard.
fn is_wildcard_only_secret_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| matches!(byte, b'%' | b'_' | b'*' | b'?'))
}

/// Returns whether text could contain a signed or credential query parameter.
fn may_contain_signed_query_parameter(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !matches!(bytes[index], b'?' | b'&') {
            index += 1;
            continue;
        }

        let key_start = index + 1;
        let mut key_end = key_start;
        while key_end < bytes.len()
            && !matches!(
                bytes[key_end],
                b'=' | b'&' | b'#' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            key_end += 1;
        }
        if bytes.get(key_end) == Some(&b'=') {
            let key = &input[key_start..key_end];
            if is_signed_query_parameter_name(key) {
                return true;
            }
        }
        index = key_end.saturating_add(1);
    }
    false
}

/// Returns whether one query parameter name is matched by the signed-query regex.
fn is_signed_query_parameter_name(name: &str) -> bool {
    SIGNED_QUERY_PARAMETER_NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Redacts long base64-like byte runs without invoking a regex over every large payload.
fn redact_base64_blobs(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = String::new();
    let mut last = 0;
    let mut index = 0;
    while index < bytes.len() {
        if !is_base64_blob_byte(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_base64_blob_byte(bytes[index]) {
            index += 1;
        }
        if index - start < 512 {
            continue;
        }
        output.push_str(&input[last..start]);
        output.push_str(REDACTED_BLOB);
        last = index;
    }

    if last == 0 {
        return None;
    }
    output.push_str(&input[last..]);
    Some(output)
}

/// Returns whether one byte belongs to the base64-like blob regex class.
fn is_base64_blob_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// Redacts POSIX home path usernames without treating relative synthetic paths as homes.
fn redact_posix_home_paths(input: &str) -> Option<String> {
    match POSIX_HOME_PATH_REGEX.replace_all(input, |captures: &regex::Captures<'_>| {
        let prefix = captures.get(1).map_or("", |value| value.as_str());
        if is_synthetic_posix_home_prefix(prefix) {
            return captures[0].to_owned();
        }
        let Some(root) = captures.get(2) else {
            return captures[0].to_owned();
        };
        format!("{prefix}/{}/{REDACTED_HOME}", root.as_str())
    }) {
        Cow::Borrowed(_) => None,
        Cow::Owned(value) => Some(value),
    }
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
        if is_non_secret_json_literal(value) {
            return;
        }
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
            "--token=**** --token=%%%% --token=\"****\" --token '%%%%' ",
            "rg -n -S 'Bearer |Authorization:|PRIVATE KEY' ",
            r#"rg -n -S 'Bearer \|Authorization:\|PRIVATE KEY' "#,
            r#"rg -n -S 'proxyAuthorization:\|PRIVATE KEY' "#,
            "--with-api-key\n          Read the API key from stdin ",
            "persist-credentials: false token: null password=none secret=true ",
            "`persist-credentials: false`... ",
            "signature=abcdefghi ",
            r#"if token == \"fmt\" { continue; } if token=> \"fmt\" { continue; }"#
        );

        assert_eq!(redact_text(input), input);
    }

    #[test]
    fn secret_assignment_scanner_matches_regex_oracle() {
        let cases = [
            "TOKEN=secretvalue API_TOKEN=supersecret",
            "mytoken=legacy-token-secret serviceapikey=legacy-key-secret",
            "STRIPE_SECRET_KEY=stripe-secret AUTHORIZATION=opaque-secret",
            "Authorization:\"Token abcdef\" password=\"quoted secret\"",
            "TOKEN:=make-secret clientSecret = 'quoted secret'",
            "tokenize='unicode61' apiKeySource: \"ANTHROPIC_API_KEY\"",
            "query LIKE '%--token=%' rg -n -S 'token='",
            "--token=flag-secret --password=p%40ssword",
            "persist-credentials: false token: null password=none secret=true",
            r#"rg -n "Authorization:|password=false|secret=false|token: false|token=PRIVATE KEY""#,
            r#"if token == \"fmt\" { continue; } if token=> \"fmt\" { continue; }"#,
            "proxyAuthorization:\\|PRIVATE KEY Authorization: Basic",
            "credential='non ascii 값' cookie=abcd",
        ];

        for case in cases {
            assert_eq!(
                redact_secret_assignments(case).unwrap_or_else(|| case.to_owned()),
                redact_secret_assignments_with_regex_oracle(case),
                "{case}"
            );
        }
    }

    #[test]
    fn redact_text_still_redacts_explicit_secret_assignments_and_flags() {
        let redacted = redact_text(
            "TOKEN=secretvalue API_TOKEN=supersecret SLACK_BOT_TOKEN=xoxb-secret \
             STRIPE_SECRET_KEY=stripe-secret AUTHORIZATION=opaque-secret \
             Authorization:\"Token abcdef\" password=\"quoted secret\" \
             TOKEN:=make-secret mytoken=legacy-token-secret serviceapikey=legacy-key-secret \
             --password=p%40ssword --token abc%2Fdef --token flagsecret \
             --github-token=github-secret --with-api-key stdin-secret \
             --openai-api-key=\"openai-secret\" \
             --session-token=session%2Fsecret --passwd='passwd-secret'",
        );

        assert!(!redacted.contains("secretvalue"));
        assert!(!redacted.contains("supersecret"));
        assert!(!redacted.contains("xoxb-secret"));
        assert!(!redacted.contains("stripe-secret"));
        assert!(!redacted.contains("opaque-secret"));
        assert!(!redacted.contains("Token abcdef"));
        assert!(!redacted.contains("quoted secret"));
        assert!(!redacted.contains("make-secret"));
        assert!(!redacted.contains("legacy-token-secret"));
        assert!(!redacted.contains("legacy-key-secret"));
        assert!(!redacted.contains("p%40ssword"));
        assert!(!redacted.contains("abc%2Fdef"));
        assert!(!redacted.contains("%40ssword"));
        assert!(!redacted.contains("%2Fdef"));
        assert!(!redacted.contains("flagsecret"));
        assert!(!redacted.contains("github-secret"));
        assert!(!redacted.contains("stdin-secret"));
        assert!(!redacted.contains("openai-secret"));
        assert!(!redacted.contains("session%2Fsecret"));
        assert!(!redacted.contains("passwd-secret"));
        assert!(redacted.contains("--password=[REDACTED_SECRET]"));
        assert!(redacted.contains("--token [REDACTED_SECRET]"));
        assert!(redacted.contains("--openai-api-key=\"[REDACTED_SECRET]\""));
        assert!(redacted.contains("--passwd='[REDACTED_SECRET]'"));
    }

    #[test]
    fn redact_text_redacts_signed_query_parameters_after_prefilter() {
        for name in SIGNED_QUERY_PARAMETER_NAMES {
            let secret = "synthetic-secret-value";
            let input = format!("https://example.test/callback?{name}={secret}");

            let redacted = redact_text(&input);

            assert!(!redacted.contains(secret), "{name}");
            assert!(
                redacted.contains(&format!("?{name}=[REDACTED_SECRET]")),
                "{name}: {redacted}"
            );
        }
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
                "optionalToken": null,
                "persist-credentials": false,
                "password": "none",
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
        assert!(value["nested"]["optionalToken"].is_null());
        assert_eq!(value["nested"]["persist-credentials"], false);
        assert_eq!(value["nested"]["password"], "none");
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
