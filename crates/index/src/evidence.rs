/// Identifies one stable evidence field label stored in `turn_evidence.field`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceField {
    /// User prompt text.
    UserMessage,
    /// Assistant final-answer text.
    FinalAnswer,
    /// Assistant commentary text.
    Commentary,
    /// Stored plaintext reasoning-summary text.
    ReasoningSummary,
    /// Tool call name.
    ToolName,
    /// Tool call arguments.
    ToolArguments,
    /// Tool call output.
    ToolOutput,
    /// Delegation summary text.
    DelegationSummary,
    /// Compact delegation metadata.
    DelegationMetadata,
    /// Compact hook-summary metadata.
    HookSummary,
    /// Compact attachment metadata.
    AttachmentMetadata,
    /// Compact provider-response-item metadata.
    ProviderResponseItemMetadata,
}

impl EvidenceField {
    /// Returns the stable SQLite and query-protocol label for this evidence field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::FinalAnswer => "final_answer",
            Self::Commentary => "commentary",
            Self::ReasoningSummary => "reasoning_summary",
            Self::ToolName => "tool_name",
            Self::ToolArguments => "tool_arguments",
            Self::ToolOutput => "tool_output",
            Self::DelegationSummary => "delegation_summary",
            Self::DelegationMetadata => "delegation_metadata",
            Self::HookSummary => "hook_summary",
            Self::AttachmentMetadata => "attachment_metadata",
            Self::ProviderResponseItemMetadata => "provider_response_item_metadata",
        }
    }
}
