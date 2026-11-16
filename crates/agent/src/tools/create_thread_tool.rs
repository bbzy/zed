use agent_client_protocol::v2::schema as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, SiblingThreadRequest, ThreadEnvironment, ToolCallEventStream, ToolInput};

/// Create a new agent thread that runs in parallel with this one.
///
/// Only use this after the user explicitly asks for or approves another thread.
/// The new thread appears in the agent sidebar just like a thread the user created
/// themselves, and runs independently — you will NOT receive its output and you
/// cannot interact with it afterwards. Use `spawn_agent` instead if you need the
/// results back.
///
/// A successful call returns only the title, agent ID, and model used; there is
/// currently no way to look up or control a sibling thread by session ID.
///
/// ### When to use
/// - The user explicitly asks you to create or start another thread.
/// - You may suggest a new thread for a separable task, but call this tool only
///   after the user explicitly agrees.
///
/// ### Prompt design
/// The new thread has no access to this conversation's history. Include in `prompt`
/// everything the new agent needs: goals, relevant file paths, constraints, and
/// context. Assume the new thread starts from a blank slate in the same project.
///
/// ### Agent and model selection
/// - If you don't know what agents or models are available, call `list_agents_and_models`.
/// - For bulk / lightweight work (e.g., spawning many parallel threads), prefer a
///   cheaper / faster model over the default.
/// - Leave `agent` and `model` unset to use the user's current defaults.
///
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct CreateThreadToolInput {
    /// Short descriptive title for the new thread, shown in the sidebar
    /// (e.g., "Investigate flaky login test").
    pub title: String,

    /// The initial prompt to send to the new thread. Include all the context the
    /// new agent needs — files, goals, constraints — because it has no access to
    /// the current conversation's history.
    pub prompt: String,

    /// Optional agent ID to use. Omit to use the user's currently selected agent.
    /// Call `list_agents_and_models` if you need to see what's available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,

    /// Optional model override as `provider/model-id` (e.g.,
    /// `anthropic/claude-haiku-4-latest`). Only meaningful for Zed's native
    /// agent. Omit to use the user's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CreateThreadToolOutput {
    Success {
        title: String,
        agent_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Error {
        error: String,
    },
}

impl From<CreateThreadToolOutput> for LanguageModelToolResultContent {
    fn from(output: CreateThreadToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize create_thread output: {e}"))
            .into()
    }
}

pub struct CreateThreadTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl CreateThreadTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for CreateThreadTool {
    type Input = CreateThreadToolInput;
    type Output = CreateThreadToolOutput;

    const NAME: &'static str = "create_thread";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => format!("Create thread: {}", i.title).into(),
            Err(value) => value
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| format!("Create thread: {s}").into())
                .unwrap_or_else(|| "Create thread".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| CreateThreadToolOutput::Error {
                    error: format!("Failed to receive tool input: {e}"),
                })?;

            let title: SharedString = input.title.clone().into();
            let request = SiblingThreadRequest {
                title: title.clone(),
                prompt: input.prompt,
                agent_id: input.agent,
                model: input.model,
            };

            let task = self.environment.create_sibling_thread(request, cx);
            match task.await {
                Ok(info) => Ok(CreateThreadToolOutput::Success {
                    title: info.title.to_string(),
                    agent_id: info.agent_id,
                    model: info.model,
                }),
                Err(error) => Err(CreateThreadToolOutput::Error {
                    error: error.to_string(),
                }),
            }
        })
    }
}
