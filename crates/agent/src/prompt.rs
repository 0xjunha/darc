/// Builds one inline-schema stdin payload for structured-output runtimes.
pub fn build_inline_schema_prompt(prompt: &str, schema_json: &str) -> String {
    format!("{prompt}\n\nReturn JSON that matches this schema exactly:\n{schema_json}\n")
}
