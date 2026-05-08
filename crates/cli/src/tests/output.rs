use super::*;

#[test]
fn query_color_policy_respects_terminal_environment() {
    assert!(should_color_output(ColorArg::Auto, true, false, None));
    assert!(should_color_output(
        ColorArg::Auto,
        true,
        false,
        Some("xterm-256color"),
    ));
    assert!(!should_color_output(ColorArg::Auto, false, false, None));
    assert!(!should_color_output(ColorArg::Auto, true, true, None));
    assert!(!should_color_output(
        ColorArg::Auto,
        true,
        false,
        Some("dumb"),
    ));
    assert!(should_color_output(
        ColorArg::Always,
        false,
        true,
        Some("dumb"),
    ));
    assert!(!should_color_output(
        ColorArg::Never,
        true,
        false,
        Some("xterm-256color"),
    ));
}

#[test]
fn auto_color_policy_respects_terminal_environment() {
    assert!(should_auto_color_output(true, false, None));
    assert!(should_auto_color_output(
        true,
        false,
        Some("xterm-256color"),
    ));
    assert!(!should_auto_color_output(false, false, None));
    assert!(!should_auto_color_output(true, true, None));
    assert!(!should_auto_color_output(true, false, Some("dumb")));
}

#[test]
fn query_json_coloring_strips_to_original_json() {
    let json = "{\n  \"schema\": \"darc.query.workspace.v1\",\n  \"data\": {\n    \"count\": 1,\n    \"enabled\": true,\n    \"missing\": null,\n    \"escaped\": \"quote \\\" ok\"\n  }\n}";
    let colored = super::color_json(json);

    assert!(colored.contains("\x1b["));
    assert_eq!(strip_ansi_text(&colored), json);
}

/// Strips ANSI control sequences from rendered text for unit assertions.
fn strip_ansi_text(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}
