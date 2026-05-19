use super::*;

/// Validates one user-facing share branch shorthand.
pub(crate) fn validate_share_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty() {
        bail!("share branch name cannot be empty");
    }
    if branch.starts_with('/') || branch.ends_with('/') || branch.contains("//") {
        bail!("share branch name must not start, end, or repeat `/`");
    }
    if branch.contains("..") || branch.contains("@{") {
        bail!("share branch name is not a safe Git branch component");
    }
    for component in branch.split('/') {
        if component.is_empty()
            || component.starts_with('.')
            || component.ends_with('.')
            || component.ends_with(".lock")
        {
            bail!("share branch name is not a safe Git branch component");
        }
    }
    if !branch
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        bail!("share branch name may only contain ASCII letters, digits, `/`, `-`, `_`, or `.`");
    }
    Ok(())
}

/// Derives one stable authenticated share user id.
pub(crate) fn derive_user_id(signing_public_key: &str) -> String {
    format!(
        "usr-{}",
        &sha256_hex(format!("signing-key:{}", signing_public_key.trim()).as_bytes())[..16]
    )
}

/// Returns one lowercase hex SHA-256 digest.
pub(crate) fn sha256_hex(input: &[u8]) -> String {
    hex_encode(&Sha256::digest(input))
}

/// Returns one lowercase hex string.
pub(crate) fn hex_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// Decodes one fixed-size lowercase or uppercase hex string.
pub(crate) fn hex_decode_fixed<const N: usize>(input: &str) -> Result<[u8; N]> {
    let trimmed = input.trim();
    if trimmed.len() != N * 2 {
        bail!("expected {} hex characters", N * 2);
    }
    let mut out = [0_u8; N];
    for (index, chunk) in trimmed.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_value(chunk[0]).context("invalid hex digit")?;
        let low = hex_value(chunk[1]).context("invalid hex digit")?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

/// Returns one nibble value for a hex byte.
pub(crate) fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
