use super::*;

/// Reads and parses one age identity file.
pub(crate) fn read_share_identity_key(path: &Path) -> Result<Identity> {
    ensure_regular_private_key_file(path)?;
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Identity::from_str(content.trim()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Reads and parses one Ed25519 share signing key file.
pub(crate) fn read_share_signing_key(path: &Path) -> Result<SigningKey> {
    ensure_regular_private_key_file(path)?;
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let seed = hex_decode_fixed::<32>(content.trim())
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Writes one age identity file with private-key permissions on Unix.
pub(crate) fn write_share_identity_key(path: &Path, content: &str) -> Result<()> {
    write_share_private_key(path, content)
}

/// Writes one share private-key file with private permissions on Unix.
pub(crate) fn write_share_private_key(path: &Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
    }
}

/// Restricts one private key file to the current user on Unix.
pub(crate) fn harden_private_key_permissions(path: &Path) -> Result<()> {
    ensure_regular_private_key_file(path)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Rejects missing, symlinked, or non-regular private key files.
pub(crate) fn ensure_regular_private_key_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("share private key path is a symlink: {}", path.display());
    }
    if !file_type.is_file() {
        bail!(
            "share private key path is not a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

/// Creates and validates a private-key directory without following symlinked ancestors.
pub(crate) fn ensure_safe_private_key_directory(root: &Path, directory: &Path) -> Result<PathBuf> {
    let relative = directory.strip_prefix(root).with_context(|| {
        format!(
            "share private key directory {} is outside root {}",
            directory.display(),
            root.display()
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("share private key directory contains unsafe path components");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    bail!(
                        "share private key directory is a symlink: {}",
                        current.display()
                    );
                }
                if !file_type.is_dir() {
                    bail!(
                        "share private key directory path is not a directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("failed to create {}", current.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(directory.to_path_buf())
}

/// Restricts one age identity file to the current user on Unix.
pub(crate) fn harden_share_key_permissions(path: &Path) -> Result<()> {
    harden_private_key_permissions(path)
}

/// Builds sorted encryption recipient strings from local identity and configured teammates.
pub(crate) fn encryption_recipient_strings(
    identity: &ShareIdentity,
    settings: &ShareSettings,
) -> Vec<String> {
    let mut recipient_strings = BTreeSet::new();
    recipient_strings.insert(identity.public_key.clone());
    for recipient in &settings.recipients {
        recipient_strings.insert(recipient.recipient.clone());
    }
    recipient_strings.into_iter().collect()
}

/// Parses age recipients from sorted recipient strings.
pub(crate) fn parse_encryption_recipients(recipient_strings: &[String]) -> Result<Vec<Recipient>> {
    recipient_strings
        .iter()
        .map(|recipient| Recipient::from_str(recipient).map_err(|error| anyhow::anyhow!("{error}")))
        .collect()
}

/// Returns a short stable fingerprint for the recipient set used by an object.
pub(crate) fn encryption_recipient_fingerprint(recipient_strings: &[String]) -> String {
    sha256_hex(recipient_strings.join("\n").as_bytes())[..16].to_owned()
}

/// Compresses one share chunk before encryption.
pub(crate) fn gzip_compress(plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(plaintext)
        .context("failed to write gzip share chunk")?;
    encoder
        .finish()
        .context("failed to finish gzip share chunk")
}

/// Decompresses one decrypted share chunk.
pub(crate) fn gzip_decompress(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(compressed);
    let mut plaintext = Vec::new();
    decoder
        .by_ref()
        .take(MAX_SHARE_CHUNK_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut plaintext)
        .context("failed to decompress share chunk")?;
    if u64::try_from(plaintext.len()).unwrap_or(u64::MAX) > MAX_SHARE_CHUNK_DECOMPRESSED_BYTES {
        bail!(
            "decompressed share chunk exceeds maximum supported size of {MAX_SHARE_CHUNK_DECOMPRESSED_BYTES} bytes"
        );
    }
    Ok(plaintext)
}

/// Encrypts one plaintext payload to every configured recipient.
pub(crate) fn encrypt_payload(plaintext: &[u8], recipients: &[Recipient]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        bail!("at least one share recipient is required");
    }
    let encryptor = age::Encryptor::with_recipients(
        recipients
            .iter()
            .map(|recipient| recipient as &dyn age::Recipient),
    )
    .context("failed to create age encryptor")?;
    let mut encrypted = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .context("failed to start age encryption")?;
    writer
        .write_all(plaintext)
        .context("failed to write age plaintext")?;
    writer.finish().context("failed to finish age encryption")?;
    Ok(encrypted)
}

/// Decrypts one encrypted payload with the local identity.
pub(crate) fn decrypt_payload(ciphertext: &[u8], identity: &Identity) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ciphertext).context("failed to read age payload")?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .context("failed to create age decryptor")?;
    let mut plaintext = Vec::new();
    reader
        .read_to_end(&mut plaintext)
        .context("failed to read decrypted share payload")?;
    Ok(plaintext)
}

/// Signs one turn payload with the local share signing key.
pub(crate) fn sign_turn_payload(
    payload: &mut EncryptedTurnPayload,
    signing_key: &SigningKey,
) -> Result<()> {
    payload.signature = None;
    let unsigned =
        serde_json::to_vec(payload).context("failed to serialize unsigned turn payload")?;
    payload.signature = Some(sign_bytes(signing_key, TURN_SIGNATURE_DOMAIN, &unsigned));
    Ok(())
}

/// Signs one sync payload with the local share signing key.
pub(crate) fn sign_sync_payload(
    payload: &mut EncryptedSyncPayload,
    signing_key: &SigningKey,
) -> Result<()> {
    payload.signature = None;
    let unsigned =
        serde_json::to_vec(payload).context("failed to serialize unsigned sync payload")?;
    payload.signature = Some(sign_bytes(signing_key, SYNC_SIGNATURE_DOMAIN, &unsigned));
    Ok(())
}

/// Verifies one decrypted turn payload signature.
pub(crate) fn verify_turn_payload_signature(payload: &EncryptedTurnPayload) -> Result<()> {
    let mut unsigned_payload = payload.clone();
    let signature = unsigned_payload
        .signature
        .take()
        .context("share turn payload is missing an exporter signature")?;
    let unsigned = serde_json::to_vec(&unsigned_payload)
        .context("failed to serialize unsigned turn payload")?;
    verify_payload_signature(
        &payload.exporter,
        TURN_SIGNATURE_DOMAIN,
        &unsigned,
        &signature,
    )
}

/// Verifies one decrypted sync payload signature.
pub(crate) fn verify_sync_payload_signature(payload: &EncryptedSyncPayload) -> Result<()> {
    let mut unsigned_payload = payload.clone();
    let signature = unsigned_payload
        .signature
        .take()
        .context("share sync payload is missing an exporter signature")?;
    let unsigned = serde_json::to_vec(&unsigned_payload)
        .context("failed to serialize unsigned sync payload")?;
    verify_payload_signature(
        &payload.exporter,
        SYNC_SIGNATURE_DOMAIN,
        &unsigned,
        &signature,
    )
}

/// Signs one domain-separated payload byte sequence.
pub(crate) fn sign_bytes(signing_key: &SigningKey, domain: &[u8], unsigned: &[u8]) -> String {
    let signature: Signature = signing_key.sign(&signature_message(domain, unsigned));
    hex_encode(&signature.to_bytes())
}

/// Verifies one domain-separated payload signature against the exporter identity.
pub(crate) fn verify_payload_signature(
    exporter: &ShareIdentity,
    domain: &[u8],
    unsigned: &[u8],
    signature: &str,
) -> Result<()> {
    if derive_user_id(&exporter.signing_public_key) != exporter.user_id {
        bail!("share payload exporter user_id does not match signing key");
    }
    let public_key = hex_decode_fixed::<32>(&exporter.signing_public_key)
        .context("share payload exporter signing key is not valid hex")?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .context("share payload exporter signing key is invalid")?;
    let signature_bytes =
        hex_decode_fixed::<64>(signature).context("share payload signature is not valid hex")?;
    let signature = Signature::from_bytes(&signature_bytes);
    verifying_key
        .verify(&signature_message(domain, unsigned), &signature)
        .context("share payload exporter signature is invalid")
}

/// Builds the exact byte string signed by share payloads.
pub(crate) fn signature_message(domain: &[u8], unsigned: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(domain.len() + 1 + unsigned.len());
    message.extend_from_slice(domain);
    message.push(b'\n');
    message.extend_from_slice(unsigned);
    message
}

/// Returns the hex-encoded Ed25519 verifying key.
pub(crate) fn signing_public_key_hex(signing_key: &SigningKey) -> String {
    hex_encode(&signing_key.verifying_key().to_bytes())
}
