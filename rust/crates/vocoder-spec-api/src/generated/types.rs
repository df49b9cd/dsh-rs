// GENERATED from spec/schemas (via spec/typert/remote.json) — do not edit.
#![allow(clippy::all)]
use serde::{Deserialize, Serialize};

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/copy:from`.
pub type AgentPresetsCopyFrom = String;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/copy:id`.
pub type AgentPresetsCopyId = String;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/copy:name` (structural: unknown).
pub type AgentPresetsCopyName = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/copy:result` (structural: unknown).
pub type AgentPresetsCopyResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/deletePreset:id`.
pub type AgentPresetsDeletePresetId = String;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/deletePreset:result` (structural: unknown).
pub type AgentPresetsDeletePresetResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-agent-presets/types#AgentPresetRoster`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresetRoster {
    pub authorable: bool,
    pub mode_selection_enabled: bool,
    pub presets: Vec<serde_json::Value>,
}

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/read:agentPreset`.
pub type AgentPresetsReadAgentPreset = String;

/// Wire type for `@deepseek-ai/dsh-agent-presets/types#AgentPresetDocument`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresetDocument {
    pub agent_preset: String,
    pub content: String,
    pub description: Option<String>,
    pub name: Option<String>,
    pub trust: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-session/types#SessionId` (structural: unknown).
pub type SessionId = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/select:agentPreset`.
pub type AgentPresetsSelectAgentPreset = String;

/// Wire alias for `@deepseek-ai/dsh-agent-presets#agentPresets/select:result`.
pub type AgentPresetsSelectResult = String;

/// Wire alias for `@deepseek-ai/dsh-api-session-controller#fileReferences/list:query`.
pub type FileReferencesListQuery = String;

/// Wire alias for `@deepseek-ai/dsh-api-session-controller#fileReferences/list:result` (structural: array).
pub type FileReferencesListResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionAttachmentRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAttachmentRequest {
    pub attachment_id: serde_json::Value,
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionAttachmentValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAttachmentValue {
    pub attachment: serde_json::Value,
    pub data: String,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionCancelRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelRequest {
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionCancelValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelValue {
    pub accepted: bool,
}

/// Wire alias for `@deepseek-ai/dsh-api-session-controller#session/canOpenWorkspacePath:result` (structural: boolean).
pub type SessionCanOpenWorkspacePathResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-session-controller/types#SessionControlFrame` (structural: unknown).
pub type SessionControlFrame = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionCreateRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateRequest {
    pub agent_preset: Option<String>,
    pub cwd: Option<String>,
    pub session_id: Option<serde_json::Value>,
    pub workspace_id: Option<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionCreateValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateValue {
    pub agent_preset: Option<String>,
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionFollowRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFollowRequest {
    pub address: serde_json::Value,
    pub assistant_stream: Option<bool>,
    pub max_messages: Option<f64>,
}

/// Wire alias for `@deepseek-ai/dsh-api-session-controller/types#SessionFollowFrame` (structural: unknown).
pub type SessionFollowFrame = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionForkRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkRequest {
    pub at_seq: Option<f64>,
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionForkValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkValue {
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionListRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListRequest {
    pub cursor: Option<String>,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionListValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListValue {
    pub items: Vec<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#ModelCatalog`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub default: serde_json::Value,
    pub failures: Vec<serde_json::Value>,
    pub groups: Vec<serde_json::Value>,
    pub routable_providers: Vec<String>,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionOpenWorkspacePathRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpenWorkspacePathRequest {
    pub action: Option<String>,
    pub path: String,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionOpenWorkspacePathValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpenWorkspacePathValue {
    pub opened: bool,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionPageRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPageRequest {
    pub address: serde_json::Value,
    pub before_seq: Option<f64>,
    pub max_messages: Option<f64>,
    pub through_seq: f64,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionPage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub has_more: bool,
    pub records: Vec<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionPromptRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptRequest {
    pub client_time_zone: Option<String>,
    pub content: Vec<serde_json::Value>,
    pub mode: serde_json::Value,
    pub request_id: serde_json::Value,
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionPromptValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptValue {
    pub accepted: bool,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionRenameRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRenameRequest {
    pub session_id: serde_json::Value,
    pub title: String,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionRenameValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRenameValue {
    pub seq: f64,
    pub title: String,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionSearchRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSearchRequest {
    pub query: String,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionSearchValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSearchValue {
    pub has_more: bool,
    pub items: Vec<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionSelectModelRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSelectModelRequest {
    pub model: String,
    pub provider: String,
    pub reasoning_effort: Option<String>,
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionSelectModelValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSelectModelValue {
    pub selected: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionUpdateQueueRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateQueueRequest {
    pub action: serde_json::Value,
    pub item_id: serde_json::Value,
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SessionUpdateQueueValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateQueueValue {
    pub accepted: bool,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SkillListRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillListRequest {
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-session-controller/types#SkillListValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillListValue {
    pub skills: Vec<serde_json::Value>,
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#credentials/describe:refs` (structural: array).
pub type CredentialsDescribeRefs = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-settings-controller#credentials/describe:result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsDescribeResult {
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#credentials/set:ref`.
pub type CredentialsSetRef = String;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#credentials/set:value`.
pub type CredentialsSetValue = String;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#credentials/set:result` (structural: unknown).
pub type CredentialsSetResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#credentials/unset:ref`.
pub type CredentialsUnsetRef = String;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#credentials/unset:result` (structural: unknown).
pub type CredentialsUnsetResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/canOpenAgentPresetDirectory:result` (structural: boolean).
pub type SettingsCanOpenAgentPresetDirectoryResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-settings/types#SettingsDescribeValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDescribeValue {
    pub has_document: bool,
    pub namespaces: Vec<serde_json::Value>,
    pub writable: bool,
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/mutate:ns`.
pub type SettingsMutateNs = String;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/mutate:ops` (structural: array).
pub type SettingsMutateOps = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/mutate:expectedRevision` (structural: unknown).
pub type SettingsMutateExpectedRevision = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-settings/types#SettingsNamespaceView`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsNamespaceView {
    pub applies: serde_json::Value,
    pub base: Option<serde_json::Value>,
    pub ns: String,
    pub revision: f64,
    pub schema: serde_json::Value,
    pub secrets: Vec<serde_json::Value>,
    pub user: Option<serde_json::Value>,
    pub value: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/openAgentPresetDirectory:agentPreset`.
pub type SettingsOpenAgentPresetDirectoryAgentPreset = String;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller/types#AgentPresetDirectoryOpenValue` (structural: unknown).
pub type AgentPresetDirectoryOpenValue = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-settings-controller/types#SettingsDocumentOpenValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDocumentOpenValue {
    pub opened: bool,
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/replace:ns`.
pub type SettingsReplaceNs = String;

/// Wire type for `@deepseek-ai/dsh-api-settings-controller#settings/replace:section`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsReplaceSection {
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/replace:expectedRevision` (structural: unknown).
pub type SettingsReplaceExpectedRevision = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/update:ns`.
pub type SettingsUpdateNs = String;

/// Wire type for `@deepseek-ai/dsh-api-settings-controller#settings/update:patch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdatePatch {
}

/// Wire alias for `@deepseek-ai/dsh-api-settings-controller#settings/update:expectedRevision` (structural: unknown).
pub type SettingsUpdateExpectedRevision = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-controller#directoryPicker/createDirectory:path`.
pub type DirectoryPickerCreateDirectoryPath = String;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-controller#directoryPicker/createDirectory:name`.
pub type DirectoryPickerCreateDirectoryName = String;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-controller#directoryPicker/createDirectory:result`.
pub type DirectoryPickerCreateDirectoryResult = String;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-controller#directoryPicker/list:path` (structural: unknown).
pub type DirectoryPickerListPath = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-host-directory-picker/types#DirectoryListing`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListing {
    pub crumbs: Vec<serde_json::Value>,
    pub entries: Vec<serde_json::Value>,
    pub home: String,
    pub path: String,
    pub truncated: bool,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-controller#directoryPicker/pick:result` (structural: unknown).
pub type DirectoryPickerPickResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceArchiveSessionRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceArchiveSessionRequest {
    pub session_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceArchiveValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceArchiveValue {
    pub archived_session_ids: Vec<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceCreateRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCreateRequest {
    pub path: String,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceCreateValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCreateValue {
    pub created: bool,
    pub workspace: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceDeleteRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDeleteRequest {
    pub workspace_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceDeleteValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDeleteValue {
    pub deleted: bool,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceFollowFrame` (structural: unknown).
pub type WorkspaceFollowFrame = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceInsertBeforeRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInsertBeforeRequest {
    pub before_workspace_id: Option<serde_json::Value>,
    pub workspace_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceOrderValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOrderValue {
    pub workspace_ids: Vec<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceInsertSessionBeforeRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInsertSessionBeforeRequest {
    pub before_session_id: Option<serde_json::Value>,
    pub session_id: serde_json::Value,
    pub workspace_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceValue {
    pub workspace: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-controller/types#WorkspaceRenameRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRenameRequest {
    pub title: String,
    pub workspace_id: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceFileWatchFrame` (structural: unknown).
pub type WorkspaceFileWatchFrame = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/list:path`.
pub type WorkspaceFilesListPath = String;

/// Wire type for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceDirectoryListing`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirectoryListing {
    pub entries: Vec<serde_json::Value>,
    pub path: String,
    pub truncated: bool,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/read:path`.
pub type WorkspaceFilesReadPath = String;

/// Wire type for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceFileRange`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileRange {
    pub limit: Option<f64>,
    pub offset: Option<f64>,
}

/// Wire type for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceFileText`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileText {
    pub absolute_path: String,
    pub bytes: Option<f64>,
    pub eof: bool,
    pub lines: f64,
    pub offset: f64,
    pub text: String,
    pub version: String,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/readAll:path`.
pub type WorkspaceFilesReadAllPath = String;

/// Wire type for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceFileBytes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileBytes {
    pub absolute_path: String,
    pub bytes: Option<f64>,
    pub data: String,
    pub eof: bool,
    pub offset: f64,
    pub version: String,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/readBytes:path`.
pub type WorkspaceFilesReadBytesPath = String;

/// Wire type for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceByteRange`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceByteRange {
    pub length: Option<f64>,
    pub offset: Option<f64>,
}

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/readRelated:path`.
pub type WorkspaceFilesReadRelatedPath = String;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/readRelated:relativePath`.
pub type WorkspaceFilesReadRelatedRelativePath = String;

/// Wire alias for `@deepseek-ai/dsh-api-workspace-files#workspaceFiles/stat:path`.
pub type WorkspaceFilesStatPath = String;

/// Wire type for `@deepseek-ai/dsh-api-workspace-files/types#WorkspaceFileStat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileStat {
    pub absolute_path: String,
    pub bytes: Option<f64>,
    pub version: String,
}

/// Wire type for `@deepseek-ai/dsh-client-file-upload/types#EncodedFileUploadRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodedFileUploadRequest {
    pub data: String,
    pub name: Option<String>,
}

/// Wire type for `@deepseek-ai/dsh-client-file-upload/types#FileUploadValue`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUploadValue {
    pub file: serde_json::Value,
    pub receipt_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-command-feedback/types#SessionFeedbackRecordRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFeedbackRecordRequest {
    pub category: Option<serde_json::Value>,
    pub session_id: serde_json::Value,
    pub text: Option<String>,
}

/// Wire alias for `@deepseek-ai/dsh-command-feedback/types#SessionFeedbackRecordResult` (structural: unknown).
pub type SessionFeedbackRecordResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-commands#commands/execute:line`.
pub type CommandsExecuteLine = String;

/// Wire alias for `@deepseek-ai/dsh-commands#commands/execute:submittedAttachments` (structural: array).
pub type CommandsExecuteSubmittedAttachments = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-commands#commands/execute:result` (structural: unknown).
pub type CommandsExecuteResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-commands#commands/list:result` (structural: array).
pub type CommandsListResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#CordisDynamicPluginId` (structural: unknown).
pub type CordisDynamicPluginId = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#CordisDynamicPluginRunId` (structural: unknown).
pub type CordisDynamicPluginRunId = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisClientSource`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicCordisClientSource {
    pub code: String,
    pub name: String,
    pub package_id: serde_json::Value,
    pub plugin_id: serde_json::Value,
    pub plugin_run_id: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/inventory:result` (structural: array).
pub type DynamicCordisRunnerInventoryResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/invoke:method`.
pub type DynamicCordisRunnerInvokeMethod = String;

/// Wire alias for `@deepseek-ai/dsh-util-values#JsonValue` (structural: unknown).
pub type JsonValue = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisInvokeResult` (structural: unknown).
pub type DynamicCordisInvokeResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-cordis-host-runner/types#CordisErrorDetails`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CordisErrorDetails {
    pub message: String,
    pub stack: Option<String>,
}

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/reportClientGuardFailure:result` (structural: null).
pub type DynamicCordisRunnerReportClientGuardFailureResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisRenderFailure`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicCordisRenderFailure {
    pub abdicated: bool,
    pub message: String,
    pub slot: String,
    pub stack: Option<String>,
}

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/reportRenderFailure:result` (structural: null).
pub type DynamicCordisRunnerReportRenderFailureResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#CordisInspectRequestId` (structural: unknown).
pub type CordisInspectRequestId = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#CordisInspectQueryResolution` (structural: unknown).
pub type CordisInspectQueryResolution = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-cordis-host-runner/types#CordisInspectResolveAck`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CordisInspectResolveAck {
    pub accepted: bool,
}

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#ApprovalRequestId` (structural: unknown).
pub type ApprovalRequestId = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisRunResolution` (structural: unknown).
pub type DynamicCordisRunResolution = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisResolveAck`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicCordisResolveAck {
    pub accepted: bool,
}

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#CordisDynamicPackageId` (structural: unknown).
pub type CordisDynamicPackageId = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#CordisDynamicRunMode` (structural: unknown).
pub type CordisDynamicRunMode = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/runHostHalf:requestId` (structural: unknown).
pub type DynamicCordisRunnerRunHostHalfRequestId = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/runHostHalf:approveFutureVersions` (structural: boolean).
pub type DynamicCordisRunnerRunHostHalfApproveFutureVersions = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisHostHalfResult` (structural: unknown).
pub type DynamicCordisHostHalfResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisRunResponse` (structural: unknown).
pub type DynamicCordisRunResponse = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisStopResponse` (structural: unknown).
pub type DynamicCordisStopResponse = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/syncInspectManifest:providers` (structural: array).
pub type DynamicCordisRunnerSyncInspectManifestProviders = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner#dynamicCordisRunner/syncInspectManifest:result` (structural: null).
pub type DynamicCordisRunnerSyncInspectManifestResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-cordis-host-runner/types#DynamicCordisUndefineReceipt` (structural: unknown).
pub type DynamicCordisUndefineReceipt = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-experimental-agent-team/client#CreateTeamTaskRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTeamTaskRequest {
    pub blocked_by: Option<Vec<serde_json::Value>>,
    pub description: String,
    pub subject: String,
    pub write_scopes: Option<Vec<String>>,
}

/// Wire alias for `@deepseek-ai/dsh-experimental-agent-team/client#TeamTaskMutationResult` (structural: unknown).
pub type TeamTaskMutationResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-experimental-agent-team/client#UpdateTeamTaskRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTeamTaskRequest {
    pub action: serde_json::Value,
    pub blocked_by: Option<Vec<serde_json::Value>>,
    pub description: Option<String>,
    pub expected_revision: f64,
    pub owner: Option<String>,
    pub subject: Option<String>,
    pub task_id: serde_json::Value,
    pub write_scopes: Option<Vec<String>>,
}

/// Wire type for `@deepseek-ai/dsh-experimental-agent-team/client#TeamView`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamView {
    pub members: Vec<serde_json::Value>,
    pub tasks: Vec<serde_json::Value>,
}

/// Wire type for `@deepseek-ai/dsh-goal/client#GoalRef`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRef {
    pub id: serde_json::Value,
    pub revision: f64,
}

/// Wire type for `@deepseek-ai/dsh-goal/client#GoalView`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalView {
    pub activation: serde_json::Value,
    pub blocked_reason: Option<serde_json::Value>,
    pub created_at: f64,
    pub id: serde_json::Value,
    pub max_goal_rounds: f64,
    pub objective: String,
    pub phase: serde_json::Value,
    pub revision: f64,
    pub rounds_started: f64,
    pub updated_at: f64,
}

/// Wire type for `@deepseek-ai/dsh-goal/client#CreateGoalRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGoalRequest {
    pub max_goal_rounds: Option<f64>,
    pub objective: String,
}

/// Wire type for `@deepseek-ai/dsh-goal/client#CreateGoalResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGoalResult {
    pub ref_: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-goal/client#EditGoalRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditGoalRequest {
    pub max_goal_rounds: Option<f64>,
    pub objective: Option<String>,
}

/// Wire alias for `@deepseek-ai/dsh-goal#goals/get:result` (structural: unknown).
pub type GoalsGetResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-host-plugin-inventory/types#PluginInventorySnapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInventorySnapshot {
    pub agent_presets: Option<Vec<serde_json::Value>>,
    pub entries: Vec<serde_json::Value>,
}

/// Wire alias for `@deepseek-ai/dsh-llm#llm/discoverModels:settingsNs`.
pub type LlmDiscoverModelsSettingsNs = String;

/// Wire type for `@deepseek-ai/dsh-llm/types#LlmModelDiscoveryRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmModelDiscoveryRequest {
    pub api: Option<String>,
    pub api_key: Option<String>,
    pub base_u_r_l: Option<String>,
    pub provider: Option<String>,
}

/// Wire alias for `@deepseek-ai/dsh-llm#llm/discoverModels:result` (structural: array).
pub type LlmDiscoverModelsResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-llm#llm/listConfigurableProviders:result` (structural: array).
pub type LlmListConfigurableProvidersResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-llm#llm/listProviders:result` (structural: array).
pub type LlmListProvidersResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-message-feedback/types#MessageFeedbackDeleteRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageFeedbackDeleteRequest {
    pub if_version: serde_json::Value,
    pub message_id: serde_json::Value,
    pub session_id: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-message-feedback/types#MessageFeedbackDeleteResult` (structural: unknown).
pub type MessageFeedbackDeleteResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-message-feedback/types#MessageFeedbackListRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageFeedbackListRequest {
    pub session_id: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-message-feedback/types#MessageFeedbackListResult` (structural: unknown).
pub type MessageFeedbackListResult = serde_json::Value;

/// Wire type for `@deepseek-ai/dsh-message-feedback/types#MessageFeedbackPutRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageFeedbackPutRequest {
    pub category: Option<serde_json::Value>,
    pub if_version: serde_json::Value,
    pub message_id: serde_json::Value,
    pub note: Option<String>,
    pub rating: serde_json::Value,
    pub session_id: serde_json::Value,
}

/// Wire alias for `@deepseek-ai/dsh-message-feedback/types#MessageFeedbackPutResult` (structural: unknown).
pub type MessageFeedbackPutResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-session-reference#sessionReferenceResolver/candidates:query`.
pub type SessionReferenceResolverCandidatesQuery = String;

/// Wire alias for `@deepseek-ai/dsh-session-reference#sessionReferenceResolver/candidates:result` (structural: array).
pub type SessionReferenceResolverCandidatesResult = serde_json::Value;

/// Wire alias for `@deepseek-ai/dsh-subagent#subagents/interruptByParent:mode`.
pub type SubagentsInterruptByParentMode = String;

/// Wire type for `@deepseek-ai/dsh-subagent/client#SubagentInterruptReceipt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentInterruptReceipt {
    pub accepted: bool,
}

/// Wire type for `@deepseek-ai/dsh-subagent/client#SubagentCatalog`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCatalog {
    pub entries: Vec<serde_json::Value>,
    pub parent_available: bool,
}

/// Wire type for `@deepseek-ai/dsh-subagent/client#SubagentPromptRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentPromptRequest {
    pub child_session_id: serde_json::Value,
    pub client_time_zone: Option<String>,
    pub content: Vec<serde_json::Value>,
    pub delivery: serde_json::Value,
    pub mode: String,
    pub parent_session_id: serde_json::Value,
    pub request_id: serde_json::Value,
}

/// Wire type for `@deepseek-ai/dsh-subagent/client#SubagentPromptReceipt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentPromptReceipt {
    pub message_id: serde_json::Value,
}

