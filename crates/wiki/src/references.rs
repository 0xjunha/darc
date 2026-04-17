use darc_paths::SourceKind;

/// Stores one parsed provider/session reference like `codex:session-1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReference<'a> {
    pub provider: SourceKind,
    pub session_id: &'a str,
}

/// Stores one parsed evidence reference like `codex:session-1#2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceReference<'a> {
    pub session: SessionReference<'a>,
    pub turn_ordinal: u64,
}

/// Parses one canonical session reference into its typed provider and session id.
pub fn parse_session_reference(value: &str) -> Option<SessionReference<'_>> {
    let (provider, session_id) = value.split_once(':')?;
    let provider = match provider {
        "claude" => SourceKind::Claude,
        "codex" => SourceKind::Codex,
        _ => return None,
    };
    (!session_id.trim().is_empty())
        .then_some(SessionReference {
            provider,
            session_id,
        })
        .filter(|reference| {
            value
                == format!(
                    "{}:{}",
                    reference.provider.directory_name(),
                    reference.session_id
                )
        })
}

/// Parses one canonical evidence reference into its session ref plus turn ordinal.
pub fn parse_evidence_reference(value: &str) -> Option<EvidenceReference<'_>> {
    let (session_ref, turn_ordinal) = value.rsplit_once('#')?;
    let session = parse_session_reference(session_ref)?;
    let turn_ordinal = turn_ordinal.parse::<u64>().ok()?;
    Some(EvidenceReference {
        session,
        turn_ordinal,
    })
    .filter(|reference| {
        value
            == format!(
                "{}:{}#{}",
                reference.session.provider.directory_name(),
                reference.session.session_id,
                reference.turn_ordinal
            )
    })
}
