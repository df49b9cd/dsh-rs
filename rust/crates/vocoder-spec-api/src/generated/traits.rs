// GENERATED from spec/typert/remote.json — do not edit.
#![allow(clippy::all)]
use super::types::*;
use crate::RemoteError;

/// `agentPresets` namespace — 5 endpoint(s).
#[async_trait::async_trait]
pub trait AgentPresetsService: Send + Sync {
    async fn copy(
        &self,
        from: AgentPresetsCopyFrom,
        id: AgentPresetsCopyId,
        name: AgentPresetsCopyName,
    ) -> Result<AgentPresetsCopyResult, RemoteError>;
    async fn delete_preset(
        &self,
        id: AgentPresetsDeletePresetId,
    ) -> Result<AgentPresetsDeletePresetResult, RemoteError>;
    async fn list(&self) -> Result<AgentPresetRoster, RemoteError>;
    async fn read(
        &self,
        agent_preset: AgentPresetsReadAgentPreset,
    ) -> Result<AgentPresetDocument, RemoteError>;
    async fn select(
        &self,
        agent: SessionId,
        agent_preset: AgentPresetsSelectAgentPreset,
    ) -> Result<AgentPresetsSelectResult, RemoteError>;
}

/// `agentTeams` namespace — 3 endpoint(s).
#[async_trait::async_trait]
pub trait AgentTeamsService: Send + Sync {
    async fn create_task(
        &self,
        agent: SessionId,
        request: CreateTeamTaskRequest,
    ) -> Result<TeamTaskMutationResult, RemoteError>;
    async fn update_task(
        &self,
        agent: SessionId,
        request: UpdateTeamTaskRequest,
    ) -> Result<TeamTaskMutationResult, RemoteError>;
    async fn view(&self, agent: SessionId) -> Result<TeamView, RemoteError>;
}

/// `commands` namespace — 2 endpoint(s).
#[async_trait::async_trait]
pub trait CommandsService: Send + Sync {
    async fn execute(
        &self,
        agent: SessionId,
        line: CommandsExecuteLine,
        submitted_attachments: CommandsExecuteSubmittedAttachments,
    ) -> Result<CommandsExecuteResult, RemoteError>;
    async fn list(&self, agent: SessionId) -> Result<CommandsListResult, RemoteError>;
}

/// `credentials` namespace — 3 endpoint(s).
#[async_trait::async_trait]
pub trait CredentialsService: Send + Sync {
    async fn describe(
        &self,
        refs: CredentialsDescribeRefs,
    ) -> Result<CredentialsDescribeResult, RemoteError>;
    async fn set(
        &self,
        ref_: CredentialsSetRef,
        value: CredentialsSetValue,
    ) -> Result<CredentialsSetResult, RemoteError>;
    async fn unset(&self, ref_: CredentialsUnsetRef)
    -> Result<CredentialsUnsetResult, RemoteError>;
}

/// `directoryPicker` namespace — 3 endpoint(s).
#[async_trait::async_trait]
pub trait DirectoryPickerService: Send + Sync {
    async fn create_directory(
        &self,
        path: DirectoryPickerCreateDirectoryPath,
        name: DirectoryPickerCreateDirectoryName,
    ) -> Result<DirectoryPickerCreateDirectoryResult, RemoteError>;
    async fn list(&self, path: DirectoryPickerListPath) -> Result<DirectoryListing, RemoteError>;
    async fn pick(&self) -> Result<DirectoryPickerPickResult, RemoteError>;
}

/// `dynamicCordisRunner` namespace — 12 endpoint(s).
#[async_trait::async_trait]
pub trait DynamicCordisRunnerService: Send + Sync {
    async fn get_client_code(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
        plugin_run_id: CordisDynamicPluginRunId,
    ) -> Result<DynamicCordisClientSource, RemoteError>;
    async fn inventory(&self) -> Result<DynamicCordisRunnerInventoryResult, RemoteError>;
    async fn invoke(
        &self,
        plugin_id: CordisDynamicPluginId,
        plugin_run_id: CordisDynamicPluginRunId,
        method: DynamicCordisRunnerInvokeMethod,
        args: JsonValue,
    ) -> Result<DynamicCordisInvokeResult, RemoteError>;
    async fn report_client_guard_failure(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
        plugin_run_id: CordisDynamicPluginRunId,
        failure: CordisErrorDetails,
    ) -> Result<DynamicCordisRunnerReportClientGuardFailureResult, RemoteError>;
    async fn report_render_failure(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
        plugin_run_id: CordisDynamicPluginRunId,
        failure: DynamicCordisRenderFailure,
    ) -> Result<DynamicCordisRunnerReportRenderFailureResult, RemoteError>;
    async fn resolve_inspect_query(
        &self,
        agent: SessionId,
        request_id: CordisInspectRequestId,
        resolution: CordisInspectQueryResolution,
    ) -> Result<CordisInspectResolveAck, RemoteError>;
    async fn resolve_request_run(
        &self,
        request_id: ApprovalRequestId,
        resolution: DynamicCordisRunResolution,
    ) -> Result<DynamicCordisResolveAck, RemoteError>;
    async fn run_host_half(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
        package_id: CordisDynamicPackageId,
        mode: CordisDynamicRunMode,
        request_id: DynamicCordisRunnerRunHostHalfRequestId,
        approve_future_versions: DynamicCordisRunnerRunHostHalfApproveFutureVersions,
    ) -> Result<DynamicCordisHostHalfResult, RemoteError>;
    async fn settle_user_run(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
        resolution: DynamicCordisRunResolution,
    ) -> Result<DynamicCordisRunResponse, RemoteError>;
    async fn stop_from_panel(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
    ) -> Result<DynamicCordisStopResponse, RemoteError>;
    async fn sync_inspect_manifest(
        &self,
        providers: DynamicCordisRunnerSyncInspectManifestProviders,
    ) -> Result<DynamicCordisRunnerSyncInspectManifestResult, RemoteError>;
    async fn undefine_from_panel(
        &self,
        agent: SessionId,
        plugin_id: CordisDynamicPluginId,
    ) -> Result<DynamicCordisUndefineReceipt, RemoteError>;
}

/// `fileReferences` namespace — 1 endpoint(s).
#[async_trait::async_trait]
pub trait FileReferencesService: Send + Sync {
    async fn list(
        &self,
        agent: SessionId,
        query: FileReferencesListQuery,
    ) -> Result<FileReferencesListResult, RemoteError>;
}

/// `fileUploads` namespace — 1 endpoint(s).
#[async_trait::async_trait]
pub trait FileUploadsService: Send + Sync {
    async fn upload(
        &self,
        agent: SessionId,
        request: EncodedFileUploadRequest,
    ) -> Result<FileUploadValue, RemoteError>;
}

/// `goals` namespace — 7 endpoint(s).
#[async_trait::async_trait]
pub trait GoalsService: Send + Sync {
    async fn clear(&self, agent: SessionId, ref_: GoalRef) -> Result<GoalRef, RemoteError>;
    async fn complete(&self, agent: SessionId, ref_: GoalRef) -> Result<GoalView, RemoteError>;
    async fn create(
        &self,
        agent: SessionId,
        request: CreateGoalRequest,
    ) -> Result<CreateGoalResult, RemoteError>;
    async fn edit(
        &self,
        agent: SessionId,
        ref_: GoalRef,
        request: EditGoalRequest,
    ) -> Result<GoalView, RemoteError>;
    async fn get(&self, agent: SessionId) -> Result<GoalsGetResult, RemoteError>;
    async fn pause(&self, agent: SessionId, ref_: GoalRef) -> Result<GoalView, RemoteError>;
    async fn resume(&self, agent: SessionId, ref_: GoalRef) -> Result<GoalView, RemoteError>;
}

/// `llm` namespace — 3 endpoint(s).
#[async_trait::async_trait]
pub trait LlmService: Send + Sync {
    async fn discover_models(
        &self,
        settings_ns: LlmDiscoverModelsSettingsNs,
        request: LlmModelDiscoveryRequest,
    ) -> Result<LlmDiscoverModelsResult, RemoteError>;
    async fn list_configurable_providers(
        &self,
    ) -> Result<LlmListConfigurableProvidersResult, RemoteError>;
    async fn list_providers(&self) -> Result<LlmListProvidersResult, RemoteError>;
}

/// `messageFeedback` namespace — 3 endpoint(s).
#[async_trait::async_trait]
pub trait MessageFeedbackService: Send + Sync {
    async fn delete(
        &self,
        request: MessageFeedbackDeleteRequest,
    ) -> Result<MessageFeedbackDeleteResult, RemoteError>;
    async fn list(
        &self,
        request: MessageFeedbackListRequest,
    ) -> Result<MessageFeedbackListResult, RemoteError>;
    async fn put(
        &self,
        request: MessageFeedbackPutRequest,
    ) -> Result<MessageFeedbackPutResult, RemoteError>;
}

/// `pluginInventory` namespace — 1 endpoint(s).
#[async_trait::async_trait]
pub trait PluginInventoryService: Send + Sync {
    async fn list(&self) -> Result<PluginInventorySnapshot, RemoteError>;
}

/// `session` namespace — 16 endpoint(s).
#[async_trait::async_trait]
pub trait SessionService: Send + Sync {
    async fn attachment(
        &self,
        request: SessionAttachmentRequest,
    ) -> Result<SessionAttachmentValue, RemoteError>;
    async fn cancel(
        &self,
        request: SessionCancelRequest,
    ) -> Result<SessionCancelValue, RemoteError>;
    async fn can_open_workspace_path(
        &self,
    ) -> Result<SessionCanOpenWorkspacePathResult, RemoteError>;
    async fn control(&self) -> Result<SessionControlFrame, RemoteError>;
    async fn create(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionCreateValue, RemoteError>;
    async fn follow(
        &self,
        request: SessionFollowRequest,
    ) -> Result<SessionFollowFrame, RemoteError>;
    async fn fork(&self, request: SessionForkRequest) -> Result<SessionForkValue, RemoteError>;
    async fn list(&self, _request: SessionListRequest) -> Result<SessionListValue, RemoteError>;
    async fn model_catalog(&self) -> Result<ModelCatalog, RemoteError>;
    async fn open_workspace_path(
        &self,
        request: SessionOpenWorkspacePathRequest,
    ) -> Result<SessionOpenWorkspacePathValue, RemoteError>;
    async fn page(&self, request: SessionPageRequest) -> Result<SessionPage, RemoteError>;
    async fn prompt(
        &self,
        request: SessionPromptRequest,
    ) -> Result<SessionPromptValue, RemoteError>;
    async fn rename(
        &self,
        request: SessionRenameRequest,
    ) -> Result<SessionRenameValue, RemoteError>;
    async fn search(
        &self,
        request: SessionSearchRequest,
    ) -> Result<SessionSearchValue, RemoteError>;
    async fn select_model(
        &self,
        request: SessionSelectModelRequest,
    ) -> Result<SessionSelectModelValue, RemoteError>;
    async fn update_queue(
        &self,
        request: SessionUpdateQueueRequest,
    ) -> Result<SessionUpdateQueueValue, RemoteError>;
}

/// `sessionFeedback` namespace — 1 endpoint(s).
#[async_trait::async_trait]
pub trait SessionFeedbackService: Send + Sync {
    async fn record(
        &self,
        request: SessionFeedbackRecordRequest,
    ) -> Result<SessionFeedbackRecordResult, RemoteError>;
}

/// `sessionReferenceResolver` namespace — 1 endpoint(s).
#[async_trait::async_trait]
pub trait SessionReferenceResolverService: Send + Sync {
    async fn candidates(
        &self,
        agent: SessionId,
        query: SessionReferenceResolverCandidatesQuery,
    ) -> Result<SessionReferenceResolverCandidatesResult, RemoteError>;
}

/// `settings` namespace — 7 endpoint(s).
#[async_trait::async_trait]
pub trait SettingsService: Send + Sync {
    async fn can_open_agent_preset_directory(
        &self,
    ) -> Result<SettingsCanOpenAgentPresetDirectoryResult, RemoteError>;
    async fn describe(&self) -> Result<SettingsDescribeValue, RemoteError>;
    async fn mutate(
        &self,
        ns: SettingsMutateNs,
        ops: SettingsMutateOps,
        expected_revision: SettingsMutateExpectedRevision,
    ) -> Result<SettingsNamespaceView, RemoteError>;
    async fn open_agent_preset_directory(
        &self,
        agent_preset: SettingsOpenAgentPresetDirectoryAgentPreset,
    ) -> Result<AgentPresetDirectoryOpenValue, RemoteError>;
    async fn open_settings_document(&self) -> Result<SettingsDocumentOpenValue, RemoteError>;
    async fn replace(
        &self,
        ns: SettingsReplaceNs,
        section: SettingsReplaceSection,
        expected_revision: SettingsReplaceExpectedRevision,
    ) -> Result<SettingsNamespaceView, RemoteError>;
    async fn update(
        &self,
        ns: SettingsUpdateNs,
        patch: SettingsUpdatePatch,
        expected_revision: SettingsUpdateExpectedRevision,
    ) -> Result<SettingsNamespaceView, RemoteError>;
}

/// `skills` namespace — 1 endpoint(s).
#[async_trait::async_trait]
pub trait SkillsService: Send + Sync {
    async fn list(&self, request: SkillListRequest) -> Result<SkillListValue, RemoteError>;
}

/// `subagents` namespace — 3 endpoint(s).
#[async_trait::async_trait]
pub trait SubagentsService: Send + Sync {
    async fn interrupt_by_parent(
        &self,
        child_session_id: SessionId,
        parent_session_id: SessionId,
        mode: SubagentsInterruptByParentMode,
    ) -> Result<SubagentInterruptReceipt, RemoteError>;
    async fn list(&self, parent_session_id: SessionId) -> Result<SubagentCatalog, RemoteError>;
    async fn prompt(
        &self,
        request: SubagentPromptRequest,
    ) -> Result<SubagentPromptReceipt, RemoteError>;
}

/// `workspace` namespace — 7 endpoint(s).
#[async_trait::async_trait]
pub trait WorkspaceService: Send + Sync {
    async fn archive_session(
        &self,
        request: WorkspaceArchiveSessionRequest,
    ) -> Result<WorkspaceArchiveValue, RemoteError>;
    async fn create(
        &self,
        request: WorkspaceCreateRequest,
    ) -> Result<WorkspaceCreateValue, RemoteError>;
    async fn delete(
        &self,
        request: WorkspaceDeleteRequest,
    ) -> Result<WorkspaceDeleteValue, RemoteError>;
    async fn follow(&self) -> Result<WorkspaceFollowFrame, RemoteError>;
    async fn insert_before(
        &self,
        request: WorkspaceInsertBeforeRequest,
    ) -> Result<WorkspaceOrderValue, RemoteError>;
    async fn insert_session_before(
        &self,
        request: WorkspaceInsertSessionBeforeRequest,
    ) -> Result<WorkspaceValue, RemoteError>;
    async fn rename(&self, request: WorkspaceRenameRequest) -> Result<WorkspaceValue, RemoteError>;
}

/// `workspaceFiles` namespace — 7 endpoint(s).
#[async_trait::async_trait]
pub trait WorkspaceFilesService: Send + Sync {
    async fn changes(
        &self,
        workspace_file_scope: SessionId,
    ) -> Result<WorkspaceFileWatchFrame, RemoteError>;
    async fn list(
        &self,
        workspace_file_scope: SessionId,
        path: WorkspaceFilesListPath,
    ) -> Result<WorkspaceDirectoryListing, RemoteError>;
    async fn read(
        &self,
        workspace_file_scope: SessionId,
        path: WorkspaceFilesReadPath,
        range: WorkspaceFileRange,
    ) -> Result<WorkspaceFileText, RemoteError>;
    async fn read_all(
        &self,
        workspace_file_scope: SessionId,
        path: WorkspaceFilesReadAllPath,
    ) -> Result<WorkspaceFileBytes, RemoteError>;
    async fn read_bytes(
        &self,
        workspace_file_scope: SessionId,
        path: WorkspaceFilesReadBytesPath,
        range: WorkspaceByteRange,
    ) -> Result<WorkspaceFileBytes, RemoteError>;
    async fn read_related(
        &self,
        workspace_file_scope: SessionId,
        path: WorkspaceFilesReadRelatedPath,
        relative_path: WorkspaceFilesReadRelatedRelativePath,
    ) -> Result<WorkspaceFileBytes, RemoteError>;
    async fn stat(
        &self,
        workspace_file_scope: SessionId,
        path: WorkspaceFilesStatPath,
    ) -> Result<WorkspaceFileStat, RemoteError>;
}
