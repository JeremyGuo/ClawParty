mod file_tools;
mod host_tools;
mod media_tools;
mod process_tools;
mod schema;
mod skill_tools;
mod web_tools;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
};

use crossbeam_channel::{select, Receiver};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::model_config::{ModelCapability, ModelConfig, ProviderType};

use super::tool_runtime::{LocalToolError, ToolExecutionContext};
pub use super::ToolRemoteMode;
use super::{
    ConversationBridge, ConversationBridgeRequest, SessionInitial, SessionType, ToolResultContent,
};

pub use file_tools::file_tool_definitions;
pub(crate) use file_tools::{file_tool_entries, ApplyPatchTool, ShellMakeVisibleTool};
pub(crate) use host_tools::host_tool_entries;
pub use host_tools::{host_tool_definitions, HostToolScope};
pub use media_tools::media_tool_definitions;
pub(crate) use media_tools::media_tool_entries;
pub use process_tools::process_tool_definitions;
pub(crate) use process_tools::{
    process_tool_entries, ShellExecTool, ShellStopTool, ShellWriteStdinTool,
};
pub use skill_tools::skill_tool_definitions;
pub(crate) use skill_tools::skill_tool_entries;
pub(crate) use web_tools::web_tool_entries;
pub use web_tools::{web_tool_definitions, WebSearchOptions};

#[allow(dead_code)]
pub(crate) struct ToolCallContext<'a> {
    pub execution: ToolExecutionContext<'a>,
}

pub struct ToolEnablementEnv<'a> {
    pub model_config: &'a ModelConfig,
    pub initial: Option<&'a SessionInitial>,
    pub options: &'a BuiltinToolCatalogOptions,
}

#[allow(dead_code)]
pub(crate) trait BaseTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn is_enabled(&self, env: &ToolEnablementEnv<'_>) -> bool {
        self.definition().is_enabled_for_model(env.model_config)
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError>;
}

#[allow(dead_code)]
pub(crate) trait ExtTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn base_tool_id(&self) -> &'static str;

    fn is_enabled(&self, env: &ToolEnablementEnv<'_>) -> bool {
        self.definition().is_enabled_for_model(env.model_config)
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError>;
}

pub(crate) trait ProviderNativeTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn is_enabled(&self, env: &ToolEnablementEnv<'_>) -> bool {
        self.definition().is_enabled_for_model(env.model_config)
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) enum ToolEntry {
    Base(Arc<dyn BaseTool>),
    Ext(Arc<dyn ExtTool>),
    ProviderNative(Arc<dyn ProviderNativeTool>),
}

impl ToolEntry {
    pub(crate) fn definition(&self) -> ToolDefinition {
        match self {
            Self::Base(tool) => tool.definition(),
            Self::Ext(tool) => tool.definition(),
            Self::ProviderNative(tool) => tool.definition(),
        }
    }

    fn is_enabled(&self, env: &ToolEnablementEnv<'_>) -> bool {
        match self {
            Self::Base(tool) => tool.is_enabled(env),
            Self::Ext(tool) => tool.is_enabled(env),
            Self::ProviderNative(tool) => tool.is_enabled(env),
        }
    }
}

pub trait ToolSet: Send + Sync {
    fn register(
        &self,
        catalog: &mut ToolCatalog,
        env: &ToolEnablementEnv<'_>,
    ) -> Result<(), ToolCatalogError>;
}

static NEXT_BRIDGE_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_bridge_request_id(tool_name: &str) -> String {
    let id = NEXT_BRIDGE_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("{tool_name}_{id}")
}

pub(crate) fn execute_bridge_tool(
    tool_name: &str,
    action: &str,
    parameters: &Value,
    ctx: &ToolCallContext<'_>,
    args: Value,
) -> Result<ToolResultContent, LocalToolError> {
    let Some(bridge) = ctx.execution.conversation_bridge.cloned() else {
        return Err(LocalToolError::Bridge(
            "conversation bridge is not configured".to_string(),
        ));
    };
    let Value::Object(payload) = args else {
        return Err(LocalToolError::InvalidArguments(
            "tool arguments must be a JSON object".to_string(),
        ));
    };
    validate_bridge_tool_payload(tool_name, parameters, &payload)?;
    let request_id = next_bridge_request_id(tool_name);
    let request = ConversationBridgeRequest {
        request_id,
        tool_call_id: tool_name.to_string(),
        tool_name: tool_name.to_string(),
        action: action.to_string(),
        payload: Value::Object(payload),
    };
    call_bridge_interruptibly(request, bridge, ctx.execution.cancel_token.cancel_rx())
}

fn validate_bridge_tool_payload(
    tool_name: &str,
    parameters: &Value,
    payload: &Map<String, Value>,
) -> Result<(), LocalToolError> {
    let properties = parameters.get("properties").and_then(Value::as_object);
    if parameters
        .get("additionalProperties")
        .and_then(Value::as_bool)
        == Some(false)
    {
        if let Some(properties) = properties {
            for key in payload.keys() {
                if !properties.contains_key(key) {
                    return invalid_bridge_args(tool_name, format!("unknown argument {key}"));
                }
            }
        }
    }

    let required = parameters
        .get("required")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for field in required.iter().filter_map(Value::as_str) {
        let Some(value) = payload.get(field) else {
            return invalid_bridge_args(tool_name, format!("missing required argument {field}"));
        };
        if let Some(schema) = properties.and_then(|properties| properties.get(field)) {
            validate_bridge_value(tool_name, field, value, schema)?;
            if schema.get("type").and_then(Value::as_str) == Some("string")
                && value.as_str().is_some_and(|text| text.trim().is_empty())
            {
                return invalid_bridge_args(
                    tool_name,
                    format!("argument {field} must not be empty"),
                );
            }
        }
    }

    if let Some(properties) = properties {
        for (key, value) in payload {
            if required
                .iter()
                .any(|field| field.as_str() == Some(key.as_str()))
            {
                continue;
            }
            if let Some(schema) = properties.get(key) {
                validate_bridge_value(tool_name, key, value, schema)?;
                if bridge_string_argument_must_be_non_empty(key)
                    && value.as_str().is_some_and(|text| text.trim().is_empty())
                {
                    return invalid_bridge_args(
                        tool_name,
                        format!("argument {key} must not be empty"),
                    );
                }
            }
        }
    }

    validate_bridge_business_rules(tool_name, payload)
}

fn validate_bridge_value(
    tool_name: &str,
    key: &str,
    value: &Value,
    schema: &Value,
) -> Result<(), LocalToolError> {
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let valid = match expected {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };
        if !valid {
            return invalid_bridge_args(tool_name, format!("argument {key} must be a {expected}"));
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|item| item == value) {
            return invalid_bridge_args(tool_name, format!("argument {key} has invalid value"));
        }
    }

    if let Some(items_schema) = schema.get("items") {
        if let Some(items) = value.as_array() {
            for (index, item) in items.iter().enumerate() {
                validate_bridge_value(tool_name, &format!("{key}[{index}]"), item, items_schema)?;
            }
        }
    }

    Ok(())
}

fn validate_bridge_business_rules(
    tool_name: &str,
    payload: &Map<String, Value>,
) -> Result<(), LocalToolError> {
    if tool_name == "cron_task_update" {
        let timing_fields = [
            "cron_second",
            "cron_minute",
            "cron_hour",
            "cron_day_of_month",
            "cron_month",
            "cron_day_of_week",
        ];
        let present = timing_fields
            .iter()
            .filter(|field| payload.contains_key(**field))
            .count();
        if present != 0 && present != timing_fields.len() {
            return invalid_bridge_args(
                tool_name,
                "cron timing fields must be provided together".to_string(),
            );
        }
    }
    Ok(())
}

fn bridge_string_argument_must_be_non_empty(key: &str) -> bool {
    matches!(
        key,
        "agent_id"
            | "description"
            | "id"
            | "memory_id"
            | "query"
            | "request_id"
            | "scope"
            | "skill_name"
            | "task"
            | "text"
            | "tool"
    )
}

fn invalid_bridge_args<T>(
    tool_name: &str,
    message: impl Into<String>,
) -> Result<T, LocalToolError> {
    Err(LocalToolError::InvalidArguments(format!(
        "{tool_name}: {}",
        message.into()
    )))
}

fn call_bridge_interruptibly(
    request: ConversationBridgeRequest,
    bridge: Arc<dyn ConversationBridge + Send + Sync>,
    cancel_rx: Receiver<()>,
) -> Result<ToolResultContent, LocalToolError> {
    let request_for_thread = request.clone();
    let bridge_for_request = bridge.clone();
    let (result_tx, result_rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let result = bridge_for_request
            .call(request_for_thread)
            .map(|response| response.result.result)
            .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });

    select! {
        recv(result_rx) -> result => result
            .map_err(|_| LocalToolError::Bridge("conversation bridge stopped".to_string()))?
            .map_err(LocalToolError::Bridge),
        recv(cancel_rx) -> _ => {
            if let Ok(result) = result_rx.try_recv() {
                return result.map_err(LocalToolError::Bridge);
            }
            if request.action == "subagent_join" {
                return cancel_subagent_join_bridge_request(request, bridge, result_rx);
            }
            Ok(ToolResultContent::from_json(json!({
                "error": "tool interrupted before conversation bridge response completed"
            })))
        }
    }
}

fn cancel_subagent_join_bridge_request(
    request: ConversationBridgeRequest,
    bridge: Arc<dyn ConversationBridge + Send + Sync>,
    result_rx: Receiver<Result<ToolResultContent, String>>,
) -> Result<ToolResultContent, LocalToolError> {
    let cancel_request = subagent_join_cancel_request(&request);
    let (cancel_tx, cancel_rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let result = bridge
            .call(cancel_request)
            .map(|_| ())
            .map_err(|error| error.to_string());
        let _ = cancel_tx.send(result);
    });

    select! {
        recv(result_rx) -> result => result
            .map_err(|_| LocalToolError::Bridge("conversation bridge stopped".to_string()))?
            .map_err(LocalToolError::Bridge),
        recv(cancel_rx) -> cancel_result => {
            cancel_result
                .map_err(|_| LocalToolError::Bridge("conversation bridge cancel stopped".to_string()))?
                .map_err(LocalToolError::Bridge)?;
            if let Ok(result) = result_rx.try_recv() {
                return result.map_err(LocalToolError::Bridge);
            }
            Ok(ToolResultContent::from_json(json!({
                "status": "interrupted",
                "reason": "subagent_join_cancelled",
            })))
        }
    }
}

fn subagent_join_cancel_request(request: &ConversationBridgeRequest) -> ConversationBridgeRequest {
    let agent_id = request.payload.get("agent_id").cloned();
    let mut payload = json!({
        "request_id": request.request_id,
        "reason": "tool_interrupted",
    });
    if let Some(agent_id) = agent_id {
        payload["agent_id"] = agent_id;
    }
    ConversationBridgeRequest {
        request_id: format!("{}_cancel", request.request_id),
        tool_call_id: request.tool_call_id.clone(),
        tool_name: request.tool_name.clone(),
        action: "subagent_join_cancel".to_string(),
        payload,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    #[default]
    Parallel,
    Serial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBackedToolKind {
    ImageAnalysis,
    PdfAnalysis,
    AudioAnalysis,
    ImageGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderNativeToolKind {
    ImageGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolBackend {
    Local,
    ProviderBacked { kind: ProviderBackedToolKind },
    ProviderNative { kind: ProviderNativeToolKind },
    ConversationBridge { action: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub concurrency: ToolConcurrency,
    pub backend: ToolBackend,
    #[serde(default, skip_serializing, skip_deserializing)]
    pub disabled_provider_types: Vec<ProviderType>,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        backend: ToolBackend,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            concurrency: ToolConcurrency::Parallel,
            backend,
            disabled_provider_types: Vec::new(),
        }
    }

    pub fn with_concurrency(mut self, concurrency: ToolConcurrency) -> Self {
        self.concurrency = concurrency;
        self
    }

    pub fn disabled_for_provider(mut self, provider_type: ProviderType) -> Self {
        if !self
            .disabled_provider_types
            .iter()
            .any(|disabled| disabled == &provider_type)
        {
            self.disabled_provider_types.push(provider_type);
        }
        self
    }

    pub fn is_enabled_for_model(&self, model_config: &ModelConfig) -> bool {
        !self
            .disabled_provider_types
            .iter()
            .any(|provider_type| provider_type == &model_config.provider_type)
    }

    pub fn openai_tool_schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description_with_concurrency(),
                "parameters": self.parameters,
            }
        })
    }

    pub fn responses_tool_schema(&self) -> Value {
        match &self.backend {
            ToolBackend::ProviderNative {
                kind: ProviderNativeToolKind::ImageGeneration,
            } => json!({
                "type": "image_generation",
                "output_format": "png",
            }),
            _ => json!({
                "type": "function",
                "name": self.name,
                "description": self.description_with_concurrency(),
                "parameters": self.parameters,
            }),
        }
    }

    pub fn claude_tool_schema(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description_with_concurrency(),
            "input_schema": self.parameters,
        })
    }

    fn description_with_concurrency(&self) -> String {
        let concurrency_guidance = match self.concurrency {
            ToolConcurrency::Parallel => {
                "Execution concurrency: parallel. The batch executor may run this tool at the same time as other parallel tools in the same tool-call batch."
            }
            ToolConcurrency::Serial => {
                "Execution concurrency: serial. The batch executor runs this tool exclusively relative to other tools in the same tool-call batch."
            }
        };

        format!("{concurrency_guidance} {}", self.description)
    }
}

#[derive(Clone, Default)]
pub struct ToolCatalog {
    tools: BTreeMap<String, ToolDefinition>,
    entries: BTreeMap<String, ToolEntry>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_model_config(model_config: &ModelConfig) -> Result<Self, ToolCatalogError> {
        let options = BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: web_search_options_for_single_model(model_config),
            enable_native_image_view: model_config.supports(ModelCapability::ImageIn),
            enable_native_pdf_view: model_config.supports(ModelCapability::PdfIn),
            enable_native_audio_view: model_config.supports(ModelCapability::AudioIn),
            enable_native_image_generation: model_config.provider_type
                == ProviderType::CodexSubscription
                && model_config.supports(ModelCapability::ImageOut),
            enable_provider_image_generation: model_config.provider_type
                != ProviderType::CodexSubscription
                && model_config.supports(ModelCapability::ImageOut),
            ..BuiltinToolCatalogOptions::default()
        };

        builtin_tool_catalog(options).map(|catalog| catalog.filtered_for_model_config(model_config))
    }

    pub fn from_model_config_and_session_type(
        model_config: &ModelConfig,
        session_type: SessionType,
    ) -> Result<Self, ToolCatalogError> {
        let options = BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: web_search_options_for_single_model(model_config),
            enable_native_image_view: model_config.supports(ModelCapability::ImageIn),
            enable_native_pdf_view: model_config.supports(ModelCapability::PdfIn),
            enable_native_audio_view: model_config.supports(ModelCapability::AudioIn),
            enable_native_image_generation: model_config.provider_type
                == ProviderType::CodexSubscription
                && model_config.supports(ModelCapability::ImageOut),
            enable_provider_image_generation: model_config.provider_type
                != ProviderType::CodexSubscription
                && model_config.supports(ModelCapability::ImageOut),
            host_tool_scope: Some(HostToolScope::from(session_type)),
            ..BuiltinToolCatalogOptions::default()
        };

        builtin_tool_catalog(options).map(|catalog| catalog.filtered_for_model_config(model_config))
    }

    pub fn from_model_config_and_initial(
        model_config: &ModelConfig,
        initial: &SessionInitial,
    ) -> Result<Self, ToolCatalogError> {
        Self::from_model_config_and_initial_with_tool_set(model_config, initial, None)
    }

    pub fn from_model_config_and_initial_with_tool_set(
        model_config: &ModelConfig,
        initial: &SessionInitial,
        tool_set: Option<&dyn ToolSet>,
    ) -> Result<Self, ToolCatalogError> {
        let image_tool = selected_media_tool(model_config, initial.image_tool_model.as_ref());
        let pdf_tool = selected_media_tool(model_config, initial.pdf_tool_model.as_ref());
        let audio_tool = selected_media_tool(model_config, initial.audio_tool_model.as_ref());
        let generation_tool =
            selected_media_tool(model_config, initial.image_generation_tool_model.as_ref());
        let options = BuiltinToolCatalogOptions {
            remote_mode: initial.tool_remote_mode.clone(),
            web_search: web_search_options_for_initial(initial),
            enable_native_image_view: image_tool.self_model
                && image_tool.model_supports(ModelCapability::ImageIn),
            enable_native_pdf_view: pdf_tool.self_model
                && pdf_tool.model_supports(ModelCapability::PdfIn),
            enable_native_audio_view: audio_tool.self_model
                && audio_tool.model_supports(ModelCapability::AudioIn),
            enable_provider_image_analysis: !image_tool.self_model
                && image_tool.model_supports(ModelCapability::ImageIn),
            enable_provider_pdf_analysis: !pdf_tool.self_model
                && pdf_tool.model_supports(ModelCapability::PdfIn),
            enable_provider_audio_analysis: !audio_tool.self_model
                && audio_tool.model_supports(ModelCapability::AudioIn),
            enable_native_image_generation: generation_tool.self_model
                && generation_tool.model_config.provider_type == ProviderType::CodexSubscription
                && generation_tool.model_supports(ModelCapability::ImageOut),
            enable_provider_image_generation: (!generation_tool.self_model
                || generation_tool.model_config.provider_type != ProviderType::CodexSubscription)
                && generation_tool.model_supports(ModelCapability::ImageOut),
            enable_skill_persistence_tools: true,
            enable_memory_tools: initial.memory_enabled,
            host_tool_scope: Some(HostToolScope::from(initial.session_type)),
            ..BuiltinToolCatalogOptions::default()
        };

        let mut catalog = ToolCatalog::new();
        let env = ToolEnablementEnv {
            model_config,
            initial: Some(initial),
            options: &options,
        };
        match tool_set {
            Some(tool_set) => tool_set.register(&mut catalog, &env)?,
            None => BuiltinToolSet.register(&mut catalog, &env)?,
        }
        Ok(catalog.filtered_for_model_config(model_config))
    }

    pub(crate) fn add_tool_entry(&mut self, entry: ToolEntry) -> Result<(), ToolCatalogError> {
        let definition = entry.definition();
        if self.tools.contains_key(&definition.name) {
            return Err(ToolCatalogError::DuplicateTool(definition.name));
        }

        self.entries.insert(definition.name.clone(), entry);
        self.tools.insert(definition.name.clone(), definition);
        Ok(())
    }

    pub(crate) fn add_enabled_tool_entry(
        &mut self,
        entry: ToolEntry,
        env: &ToolEnablementEnv<'_>,
    ) -> Result<(), ToolCatalogError> {
        if entry.is_enabled(env) && self.entry_base_is_enabled(&entry, env) {
            self.add_tool_entry(entry)?;
        }
        Ok(())
    }

    fn entry_base_is_enabled(&self, entry: &ToolEntry, env: &ToolEnablementEnv<'_>) -> bool {
        let ToolEntry::Ext(tool) = entry else {
            return true;
        };
        let base_tool_id = tool.base_tool_id();
        match self.entries.get(base_tool_id) {
            Some(base_entry) => base_entry.is_enabled(env),
            None => builtin_tool_definition(base_tool_id, env.options)
                .is_some_and(|definition| definition.is_enabled_for_model(env.model_config)),
        }
    }

    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name)
    }

    pub(crate) fn should_execute_registered(&self, name: &str) -> bool {
        match self.entries.get(name) {
            Some(ToolEntry::Ext(_)) => true,
            Some(ToolEntry::Base(tool)) => {
                let definition = tool.definition();
                match definition.backend {
                    ToolBackend::Local => true,
                    ToolBackend::ConversationBridge { .. } => true,
                    ToolBackend::ProviderBacked { .. } => true,
                    ToolBackend::ProviderNative { .. } => false,
                }
            }
            Some(ToolEntry::ProviderNative(_)) | None => false,
        }
    }

    pub(crate) fn call_tool(
        &self,
        name: &str,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        match self.entries.get(name) {
            Some(ToolEntry::Base(tool)) => tool.call(ctx, args),
            Some(ToolEntry::Ext(tool)) => tool.call(ctx, args),
            Some(ToolEntry::ProviderNative(_)) => Err(LocalToolError::UnsupportedTool(format!(
                "{name} is provider-native and cannot be executed locally"
            ))),
            None => Err(LocalToolError::UnsupportedTool(name.to_string())),
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &ToolDefinition)> {
        self.tools.iter()
    }

    pub fn filtered_for_model_config(mut self, model_config: &ModelConfig) -> Self {
        self.tools
            .retain(|_, tool| tool.is_enabled_for_model(model_config));
        let enabled_names = self.tools.keys().cloned().collect::<BTreeSet<_>>();
        self.entries
            .retain(|name, _| enabled_names.contains(name.as_str()));
        self
    }

    pub fn openai_tool_schemas(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(ToolDefinition::openai_tool_schema)
            .collect()
    }
}

struct SelectedMediaTool<'a> {
    model_config: &'a ModelConfig,
    self_model: bool,
}

impl SelectedMediaTool<'_> {
    fn model_supports(&self, capability: ModelCapability) -> bool {
        self.model_config.supports(capability)
    }
}

fn selected_media_tool<'a>(
    primary_model: &'a ModelConfig,
    configured_model: Option<&'a ModelConfig>,
) -> SelectedMediaTool<'a> {
    match configured_model {
        Some(model_config) if model_config != primary_model => SelectedMediaTool {
            model_config,
            self_model: false,
        },
        _ => SelectedMediaTool {
            model_config: primary_model,
            self_model: true,
        },
    }
}

fn web_search_options_for_single_model(model_config: &ModelConfig) -> WebSearchOptions {
    let web = model_config.supports(ModelCapability::WebSearch);
    WebSearchOptions {
        enabled: web,
        web,
        ..WebSearchOptions::default()
    }
}

fn web_search_options_for_initial(initial: &SessionInitial) -> WebSearchOptions {
    let web = optional_model_supports_web_search(initial.search_tool_model.as_ref());
    let image = optional_model_supports_web_search(initial.search_image_tool_model.as_ref());
    let video = optional_model_supports_web_search(initial.search_video_tool_model.as_ref());
    let news = optional_model_supports_web_search(initial.search_news_tool_model.as_ref());
    WebSearchOptions {
        enabled: web || image || video || news,
        web,
        image,
        video,
        news,
    }
}

fn optional_model_supports_web_search(model_config: Option<&ModelConfig>) -> bool {
    model_config.is_some_and(|model_config| model_config.supports(ModelCapability::WebSearch))
}

impl From<SessionType> for HostToolScope {
    fn from(value: SessionType) -> Self {
        match value {
            SessionType::Foreground => HostToolScope::MainForeground,
            SessionType::Background => HostToolScope::MainBackground,
            SessionType::Subagent => HostToolScope::SubAgent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinToolCatalogOptions {
    pub remote_mode: ToolRemoteMode,
    pub web_search: WebSearchOptions,
    pub enable_native_image_view: bool,
    pub enable_native_pdf_view: bool,
    pub enable_native_audio_view: bool,
    pub enable_provider_image_analysis: bool,
    pub enable_provider_pdf_analysis: bool,
    pub enable_provider_audio_analysis: bool,
    pub enable_native_image_generation: bool,
    pub enable_provider_image_generation: bool,
    pub skill_names: Vec<String>,
    pub enable_skill_persistence_tools: bool,
    pub enable_memory_tools: bool,
    pub host_tool_scope: Option<HostToolScope>,
}

impl Default for BuiltinToolCatalogOptions {
    fn default() -> Self {
        Self {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: WebSearchOptions::default(),
            enable_native_image_view: false,
            enable_native_pdf_view: false,
            enable_native_audio_view: false,
            enable_provider_image_analysis: false,
            enable_provider_pdf_analysis: false,
            enable_provider_audio_analysis: false,
            enable_native_image_generation: false,
            enable_provider_image_generation: false,
            skill_names: Vec::new(),
            enable_skill_persistence_tools: false,
            enable_memory_tools: false,
            host_tool_scope: None,
        }
    }
}

pub struct BuiltinToolSet;

impl ToolSet for BuiltinToolSet {
    fn register(
        &self,
        catalog: &mut ToolCatalog,
        env: &ToolEnablementEnv<'_>,
    ) -> Result<(), ToolCatalogError> {
        for entry in file_tool_entries(&env.options.remote_mode) {
            catalog.add_enabled_tool_entry(entry, env)?;
        }
        for entry in process_tool_entries(&env.options.remote_mode) {
            catalog.add_enabled_tool_entry(entry, env)?;
        }
        for entry in web_tool_entries(env.options.web_search) {
            catalog.add_enabled_tool_entry(entry, env)?;
        }
        for entry in media_tool_entries(env.options) {
            catalog.add_enabled_tool_entry(entry, env)?;
        }
        for entry in skill_tool_entries(
            &env.options.skill_names,
            env.options.enable_skill_persistence_tools,
        ) {
            catalog.add_enabled_tool_entry(entry, env)?;
        }
        if let Some(scope) = env.options.host_tool_scope {
            for entry in host_tool_entries(scope, env.options.enable_memory_tools) {
                catalog.add_enabled_tool_entry(entry, env)?;
            }
        }

        Ok(())
    }
}

pub fn builtin_tool_catalog(
    options: BuiltinToolCatalogOptions,
) -> Result<ToolCatalog, ToolCatalogError> {
    let mut catalog = ToolCatalog::new();

    for entry in file_tool_entries(&options.remote_mode) {
        catalog.add_tool_entry(entry)?;
    }
    for entry in process_tool_entries(&options.remote_mode) {
        catalog.add_tool_entry(entry)?;
    }
    for entry in web_tool_entries(options.web_search) {
        catalog.add_tool_entry(entry)?;
    }
    for entry in media_tool_entries(&options) {
        catalog.add_tool_entry(entry)?;
    }
    for entry in skill_tool_entries(&options.skill_names, options.enable_skill_persistence_tools) {
        catalog.add_tool_entry(entry)?;
    }
    if let Some(scope) = options.host_tool_scope {
        for entry in host_tool_entries(scope, options.enable_memory_tools) {
            catalog.add_tool_entry(entry)?;
        }
    }

    Ok(catalog)
}

pub(crate) fn builtin_tool_entry(
    options: &BuiltinToolCatalogOptions,
    tool_name: &str,
) -> Option<ToolEntry> {
    builtin_tool_entries(options)
        .into_iter()
        .find(|entry| entry.definition().name == tool_name)
}

pub(crate) fn builtin_tool_definition(
    tool_name: &str,
    options: &BuiltinToolCatalogOptions,
) -> Option<ToolDefinition> {
    builtin_tool_entry(options, tool_name).map(|entry| entry.definition())
}

fn builtin_tool_entries(options: &BuiltinToolCatalogOptions) -> Vec<ToolEntry> {
    let mut entries = Vec::new();
    entries.extend(file_tool_entries(&options.remote_mode));
    entries.extend(process_tool_entries(&options.remote_mode));
    entries.extend(web_tool_entries(options.web_search));
    entries.extend(media_tool_entries(options));
    entries.extend(skill_tool_entries(
        &options.skill_names,
        options.enable_skill_persistence_tools,
    ));
    if let Some(scope) = options.host_tool_scope {
        entries.extend(host_tool_entries(scope, options.enable_memory_tools));
    }
    entries
}

#[derive(Debug, Error)]
pub enum ToolCatalogError {
    #[error("duplicate tool definition: {0}")]
    DuplicateTool(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{ProviderType, RetryMode, TokenEstimatorType};

    #[test]
    fn builds_builtin_catalog_with_copied_core_tool_schemas() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: WebSearchOptions {
                enabled: true,
                web: true,
                image: true,
                video: true,
                news: true,
            },
            enable_native_image_view: true,
            enable_native_pdf_view: true,
            enable_native_audio_view: true,
            enable_provider_image_analysis: true,
            enable_provider_pdf_analysis: true,
            enable_provider_audio_analysis: true,
            enable_provider_image_generation: true,
            skill_names: vec!["imagegen".to_string()],
            enable_skill_persistence_tools: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        assert!(!catalog.contains("file_read"));
        assert!(!catalog.contains("file_write"));
        assert!(!catalog.contains("shell"));
        assert!(catalog.contains("shell_exec"));
        assert!(catalog.contains("shell_write_stdin"));
        assert!(catalog.contains("shell_stop"));
        assert!(!catalog.contains("file_download_start"));
        assert!(!catalog.contains("file_download_progress"));
        assert!(!catalog.contains("file_download_wait"));
        assert!(!catalog.contains("file_download_cancel"));
        assert!(!catalog.contains("dsl_start"));
        assert!(!catalog.contains("dsl_wait"));
        assert!(!catalog.contains("dsl_kill"));
        assert!(catalog.contains("web_search"));
        assert!(catalog.contains("image_analysis"));
        assert!(catalog.contains("image_stop"));
        assert!(catalog.contains("image_view"));
        assert!(catalog.contains("pdf_analysis"));
        assert!(catalog.contains("pdf_stop"));
        assert!(catalog.contains("audio_analysis"));
        assert!(catalog.contains("audio_stop"));
        assert!(catalog.contains("image_generation"));
        assert!(catalog.contains("image_generation_stop"));
        assert!(catalog.contains("skill_load"));
        assert!(catalog.contains("skill_create"));
        assert!(catalog.contains("skill_update"));
        assert!(catalog.contains("skill_delete"));

        assert!(!catalog.contains("edit"));
        assert!(!catalog.contains("grep"));
        assert!(!catalog.contains("ls"));

        let shell = catalog.get("shell_exec").unwrap();
        assert!(shell.parameters["properties"]
            .get("yield_time_ms")
            .is_some());
        assert!(shell.parameters["properties"].get("remote").is_some());
        assert!(shell.parameters["properties"].get("session_id").is_none());
        assert!(shell.parameters["properties"].get("shell_id").is_none());
        assert!(shell.parameters["properties"].get("tty").is_some());

        let image_generation = catalog.get("image_generation").unwrap();
        assert_eq!(
            image_generation.backend,
            ToolBackend::ProviderBacked {
                kind: ProviderBackedToolKind::ImageGeneration
            }
        );
    }

    #[test]
    fn provider_tool_set_can_replace_default_catalog() {
        struct SingleToolSet;

        impl ToolSet for SingleToolSet {
            fn register(
                &self,
                catalog: &mut ToolCatalog,
                env: &ToolEnablementEnv<'_>,
            ) -> Result<(), ToolCatalogError> {
                catalog.add_enabled_tool_entry(ToolEntry::Base(Arc::new(ProviderExtShell)), env)
            }
        }

        struct ProviderExtShell;

        impl BaseTool for ProviderExtShell {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new(
                    "provider_ext_shell",
                    "Provider-specific shell facade.",
                    json!({
                        "type": "object",
                        "properties": {
                            "cmd": {"type": "string"}
                        },
                        "required": ["cmd"],
                        "additionalProperties": false
                    }),
                    ToolBackend::Local,
                )
            }

            fn call(
                &self,
                _ctx: &ToolCallContext<'_>,
                _args: Value,
            ) -> Result<ToolResultContent, LocalToolError> {
                Err(LocalToolError::UnsupportedTool(
                    "provider_ext_shell is test-only".to_string(),
                ))
            }
        }

        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let initial = SessionInitial::new("session_1", SessionType::Foreground);
        let catalog = ToolCatalog::from_model_config_and_initial_with_tool_set(
            &config,
            &initial,
            Some(&SingleToolSet),
        )
        .expect("catalog should build");

        assert!(catalog.contains("provider_ext_shell"));
        assert!(!catalog.contains("shell_exec"));
    }

    #[test]
    fn ext_tool_inherits_builtin_base_tool_enablement() {
        struct VisibilityExtTool;

        impl ExtTool for VisibilityExtTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new(
                    "provider_visibility",
                    "Provider-specific visibility facade.",
                    json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    }),
                    ToolBackend::Local,
                )
            }

            fn base_tool_id(&self) -> &'static str {
                "shell_make_visible"
            }

            fn call(
                &self,
                _ctx: &ToolCallContext<'_>,
                _args: Value,
            ) -> Result<ToolResultContent, LocalToolError> {
                Ok(ToolResultContent::from_text("ok"))
            }
        }

        struct VisibilityToolSet;

        impl ToolSet for VisibilityToolSet {
            fn register(
                &self,
                catalog: &mut ToolCatalog,
                env: &ToolEnablementEnv<'_>,
            ) -> Result<(), ToolCatalogError> {
                catalog.add_enabled_tool_entry(ToolEntry::Ext(Arc::new(VisibilityExtTool)), env)
            }
        }

        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let selectable_initial = SessionInitial::new("session_1", SessionType::Foreground);
        let selectable_catalog = ToolCatalog::from_model_config_and_initial_with_tool_set(
            &config,
            &selectable_initial,
            Some(&VisibilityToolSet),
        )
        .expect("catalog should build");
        assert!(!selectable_catalog.contains("provider_visibility"));

        let mut fixed_initial = SessionInitial::new("session_2", SessionType::Foreground);
        fixed_initial.tool_remote_mode = ToolRemoteMode::FixedSsh {
            host: "gpu-box".to_string(),
            cwd: None,
        };
        let fixed_catalog = ToolCatalog::from_model_config_and_initial_with_tool_set(
            &config,
            &fixed_initial,
            Some(&VisibilityToolSet),
        )
        .expect("catalog should build");
        assert!(fixed_catalog.contains("provider_visibility"));
    }

    #[test]
    fn model_config_enables_capability_dependent_tools() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![
                ModelCapability::Chat,
                ModelCapability::ImageIn,
                ModelCapability::ImageOut,
                ModelCapability::PdfIn,
                ModelCapability::AudioIn,
                ModelCapability::WebSearch,
            ],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let catalog = ToolCatalog::from_model_config(&config).expect("catalog should build");

        assert!(catalog.contains("web_search"));
        assert!(catalog.contains("image_view"));
        assert!(catalog.contains("pdf_view"));
        assert!(catalog.contains("audio_view"));
        assert!(catalog.contains("image_generation"));
    }

    #[test]
    fn codex_subscription_image_out_exposes_native_image_generation_tool() {
        let config = ModelConfig {
            provider_type: ProviderType::CodexSubscription,
            model_name: "gpt-5.5".to_string(),
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            api_key_env: "CHATGPT_ACCESS_TOKEN".to_string(),
            capabilities: vec![
                ModelCapability::Chat,
                ModelCapability::ImageIn,
                ModelCapability::ImageOut,
            ],
            token_max_context: 262_144,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let catalog = ToolCatalog::from_model_config(&config).expect("catalog should build");
        let image_generation = catalog
            .get("image_generation")
            .expect("native image_generation should be exposed");

        assert_eq!(
            image_generation.backend,
            ToolBackend::ProviderNative {
                kind: ProviderNativeToolKind::ImageGeneration
            }
        );
        assert_eq!(
            image_generation.responses_tool_schema(),
            json!({
                "type": "image_generation",
                "output_format": "png",
            })
        );
        assert!(!catalog.contains("image_generation_stop"));
    }

    #[test]
    fn initial_external_media_models_expose_analysis_tools_instead_of_load_tools() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut image_helper = primary.clone();
        image_helper.model_name = "vision-helper".to_string();
        image_helper.capabilities = vec![ModelCapability::Chat, ModelCapability::ImageIn];
        let mut pdf_helper = primary.clone();
        pdf_helper.model_name = "pdf-helper".to_string();
        pdf_helper.capabilities = vec![ModelCapability::Chat, ModelCapability::PdfIn];
        let mut audio_helper = primary.clone();
        audio_helper.model_name = "audio-helper".to_string();
        audio_helper.capabilities = vec![ModelCapability::Chat, ModelCapability::AudioIn];

        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.image_tool_model = Some(image_helper);
        initial.pdf_tool_model = Some(pdf_helper);
        initial.audio_tool_model = Some(audio_helper);
        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        assert!(catalog.contains("image_analysis"));
        assert!(catalog.contains("image_stop"));
        assert!(!catalog.contains("image_view"));
        assert!(catalog.contains("pdf_analysis"));
        assert!(catalog.contains("pdf_stop"));
        assert!(!catalog.contains("pdf_view"));
        assert!(catalog.contains("audio_analysis"));
        assert!(catalog.contains("audio_stop"));
        assert!(!catalog.contains("audio_view"));
    }

    #[test]
    fn initial_search_model_exposes_web_search_for_text_primary_model() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut search_model = primary.clone();
        search_model.provider_type = ProviderType::BraveSearch;
        search_model.model_name = "brave-web-search".to_string();
        search_model.url = "https://api.search.brave.com/res/v1/web/search".to_string();
        search_model.api_key_env = "BRAVE_SEARCH_API_KEY".to_string();
        search_model.capabilities = vec![ModelCapability::WebSearch];
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.search_tool_model = Some(search_model);

        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        assert!(catalog.contains("web_search"));
    }

    #[test]
    fn initial_without_search_tool_model_hides_web_search() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::WebSearch],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let initial = SessionInitial::new("session_1", SessionType::Foreground);

        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        assert!(!catalog.contains("web_search"));
    }

    #[test]
    fn initial_image_search_model_enables_image_mode_on_web_search() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut image_search_model = primary.clone();
        image_search_model.provider_type = ProviderType::BraveSearchImage;
        image_search_model.model_name = "brave-image-search".to_string();
        image_search_model.url = "https://api.search.brave.com/res/v1/images/search".to_string();
        image_search_model.api_key_env = "BRAVE_SEARCH_API_KEY".to_string();
        image_search_model.capabilities = vec![ModelCapability::WebSearch];
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.search_image_tool_model = Some(image_search_model);

        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        let tool = catalog
            .get("web_search")
            .expect("web_search should be exposed");
        assert!(tool.description.contains("image"));
        assert!(tool.parameters["properties"].get("image").is_some());
    }

    #[test]
    fn session_type_controls_host_tool_scope() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let foreground =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Foreground)
                .expect("foreground catalog should build");
        let background =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Background)
                .expect("background catalog should build");
        let subagent =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Subagent)
                .expect("subagent catalog should build");

        assert!(foreground.contains("background_agent_start"));
        assert!(!foreground.contains("terminate"));
        assert!(background.contains("terminate"));
        assert!(!background.contains("background_agent_start"));
        let subagent_start = subagent.get("subagent_start").unwrap();
        assert!(subagent_start.parameters["properties"]
            .get("model")
            .is_none());
        assert!(subagent_start
            .description
            .contains("more than 3 sequential tool operations"));
        assert!(subagent_start.description.contains("irreversible damage"));
        assert!(!subagent.contains("cron_tasks_list"));
    }

    #[test]
    fn codex_subscription_catalog_keeps_host_tools() {
        let config = ModelConfig {
            provider_type: ProviderType::CodexSubscription,
            model_name: "gpt-5.5".to_string(),
            url: "wss://codex.example.invalid".to_string(),
            api_key_env: "CODEX_AUTH".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let foreground =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Foreground)
                .expect("foreground catalog should build");
        let initial = SessionInitial::new("session_1", SessionType::Foreground);
        let from_initial =
            ToolCatalog::from_model_config_and_initial(&config, &initial).expect("catalog");

        for catalog in [&foreground, &from_initial] {
            assert!(catalog.contains("update_plan"));
            assert!(catalog.contains("cron_tasks_list"));
        }
    }

    #[test]
    fn exports_openai_tool_schema() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default())
            .expect("catalog should build");
        let schemas = catalog.openai_tool_schemas();

        assert!(schemas
            .iter()
            .all(|tool| tool["function"]["name"] != "file_read"));
        assert!(schemas
            .iter()
            .all(|tool| tool["function"]["name"] != "file_write"));
        assert!(schemas.iter().all(|tool| !tool["function"]["name"]
            .as_str()
            .unwrap_or("")
            .starts_with("file_download_")));
        assert!(schemas.iter().all(|tool| !tool["function"]["name"]
            .as_str()
            .unwrap_or("")
            .starts_with("dsl_")));

        let apply_patch = schemas
            .iter()
            .find(|tool| tool["function"]["name"] == "apply_patch")
            .expect("apply_patch schema should be exported");
        assert_eq!(apply_patch["type"], "function");
        assert_eq!(
            apply_patch["function"]["parameters"]["additionalProperties"],
            false
        );
        assert!(apply_patch["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Execution concurrency: serial"));
        assert!(
            apply_patch["function"]["parameters"]["properties"]["format"]["enum"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("freeform"))
        );
    }

    #[test]
    fn host_tools_use_conversation_bridge_without_legacy_workspace_tools() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        assert!(catalog.contains("subagent_start"));
        assert!(catalog.contains("background_agent_start"));
        assert!(catalog.contains("background_agents_list"));
        assert!(catalog.contains("cron_tasks_list"));
        assert!(!catalog.contains("memory_search"));
        assert!(!catalog.contains("memory_write"));
        assert!(!catalog.contains("workpath_add"));
        assert!(!catalog.contains("workspaces_list"));
        assert!(!catalog.contains("workspace_content_list"));
        assert!(!catalog.contains("workspace_mount"));
        assert!(!catalog.contains("workspace_content_move"));

        let background = catalog.get("background_agent_start").unwrap();
        assert_eq!(background.parameters["required"], json!(["task"]));
        assert!(background.parameters["properties"].get("model").is_none());
    }

    #[test]
    fn memory_host_tools_are_config_gated() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            enable_memory_tools: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");
        let memory_write = catalog.get("memory_write").unwrap();
        assert_eq!(
            memory_write.backend,
            ToolBackend::ConversationBridge {
                action: "memory_write".to_string()
            }
        );
        assert_eq!(
            memory_write.parameters["required"],
            json!(["scope", "text"])
        );
        assert!(memory_write.parameters["properties"]
            .get("reason")
            .is_none());
    }

    #[test]
    fn bridge_tool_validation_rejects_missing_required_fields_before_bridge() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            enable_memory_tools: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");
        let memory_search = catalog.get("memory_search").unwrap();
        let payload = Map::new();

        let error =
            validate_bridge_tool_payload("memory_search", &memory_search.parameters, &payload)
                .expect_err("missing query should be rejected");

        assert!(
            matches!(error, LocalToolError::InvalidArguments(message) if message.contains("missing required argument query"))
        );
    }

    #[test]
    fn bridge_tool_validation_rejects_bad_types_and_enums_before_bridge() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            enable_memory_tools: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");
        let memory_write = catalog.get("memory_write").unwrap();
        let payload = json!({
            "scope": "private",
            "text": "remember this",
        });
        let payload = payload.as_object().unwrap();

        let error = validate_bridge_tool_payload("memory_write", &memory_write.parameters, payload)
            .expect_err("invalid memory scope should be rejected");

        assert!(
            matches!(error, LocalToolError::InvalidArguments(message) if message.contains("scope"))
        );
    }

    #[test]
    fn bridge_tool_validation_rejects_partial_cron_timing_patch() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");
        let cron_update = catalog.get("cron_task_update").unwrap();
        let payload = json!({
            "id": "cron_0001",
            "cron_minute": "0",
        });
        let payload = payload.as_object().unwrap();

        let error =
            validate_bridge_tool_payload("cron_task_update", &cron_update.parameters, payload)
                .expect_err("partial cron timing should be rejected");

        assert!(
            matches!(error, LocalToolError::InvalidArguments(message) if message.contains("timing fields"))
        );
    }

    #[test]
    fn selectable_remote_mode_exposes_remote_schema_from_ssh_config() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.tool_remote_mode = ToolRemoteMode::Selectable;

        let catalog =
            ToolCatalog::from_model_config_and_initial(&config, &initial).expect("catalog");
        let shell = catalog.get("shell_exec").unwrap();
        let remote = &shell.parameters["properties"]["remote"];

        assert_eq!(remote["type"], "string");
        assert!(remote["description"]
            .as_str()
            .unwrap()
            .contains("~/.ssh/config"));
        assert!(!catalog.contains("file_download_start"));
        assert!(!catalog.contains("workpath_add"));
        assert!(catalog.contains("skill_load"));
        assert!(catalog.contains("skill_create"));
        assert!(catalog.contains("skill_update"));
        assert!(catalog.contains("skill_delete"));
    }

    #[test]
    fn fixed_remote_mode_hides_remote_schema() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::FixedSsh {
                host: "gpu-box".to_string(),
                cwd: None,
            },
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        let shell = catalog.get("shell_exec").unwrap();
        assert!(shell.parameters["properties"].get("remote").is_none());
        assert!(!catalog.contains("file_download_start"));
        assert!(catalog.contains("shell_make_visible"));
        assert!(catalog.contains("attachment_make_visible"));
        assert!(catalog
            .get("shell_make_visible")
            .unwrap()
            .description
            .contains("shell_exec reads .stellaclaw/... paths"));
        assert!(catalog
            .get("attachment_make_visible")
            .unwrap()
            .description
            .contains("Before referencing a remote-only file with [name](path) or ![alt](path)"));
    }

    #[test]
    fn media_path_tools_expose_remote_schema_in_selectable_mode() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            enable_native_image_view: true,
            enable_native_pdf_view: true,
            enable_native_audio_view: true,
            enable_provider_image_analysis: true,
            enable_provider_pdf_analysis: true,
            enable_provider_audio_analysis: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        for name in [
            "image_view",
            "pdf_view",
            "audio_view",
            "image_analysis",
            "pdf_analysis",
            "audio_analysis",
        ] {
            let tool = catalog
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(tool.parameters["properties"].get("remote").is_some());
        }
    }
}
