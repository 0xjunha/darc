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
    /// Lists every stable evidence field in indexed evidence order.
    pub const ALL: [Self; 12] = [
        Self::UserMessage,
        Self::FinalAnswer,
        Self::Commentary,
        Self::ReasoningSummary,
        Self::ToolName,
        Self::ToolArguments,
        Self::ToolOutput,
        Self::DelegationSummary,
        Self::DelegationMetadata,
        Self::HookSummary,
        Self::AttachmentMetadata,
        Self::ProviderResponseItemMetadata,
    ];

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

    /// Parses one stable evidence label from snake_case or CLI kebab-case text.
    pub fn parse_label(value: &str) -> Option<Self> {
        match value.replace('-', "_").as_str() {
            "user_message" => Some(Self::UserMessage),
            "final_answer" => Some(Self::FinalAnswer),
            "commentary" => Some(Self::Commentary),
            "reasoning_summary" => Some(Self::ReasoningSummary),
            "tool_name" => Some(Self::ToolName),
            "tool_arguments" => Some(Self::ToolArguments),
            "tool_output" => Some(Self::ToolOutput),
            "delegation_summary" => Some(Self::DelegationSummary),
            "delegation_metadata" => Some(Self::DelegationMetadata),
            "hook_summary" => Some(Self::HookSummary),
            "attachment_metadata" => Some(Self::AttachmentMetadata),
            "provider_response_item_metadata" => Some(Self::ProviderResponseItemMetadata),
            _ => None,
        }
    }
}
