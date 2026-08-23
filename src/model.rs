use async_openai::{
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
        ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionTool, ChatCompletionTools,
        CreateChatCompletionRequestArgs, FunctionCall, FunctionObject, ResponseFormat,
        ResponseFormatJsonSchema,
    },
    Client,
};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::{
    config::{Config, ConfigError},
    git::Diff,
    output::{Event, Reporter},
};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("{0}")]
    Api(#[from] OpenAIError),

    #[error("the model returned nothing")]
    Empty,

    // The error alone says nothing about what actually came back, which is the
    // one thing needed to tell a bad schema from a bad model.
    #[error("the model returned a shape that does not fit: {source}\nit returned: {text}")]
    Shape {
        #[source]
        source: serde_json::Error,
        text: String,
    },
}

pub fn system(text: impl Into<String>) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
        content: ChatCompletionRequestSystemMessageContent::Text(text.into()),
        name: None,
    })
}

pub fn user(text: impl Into<String>) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: ChatCompletionRequestUserMessageContent::Text(text.into()),
        name: None,
    })
}

pub struct Model {
    client: Client<OpenAIConfig>,
    name: String,
    max_tool_calls: u32,
    max_tool_bytes: usize,
}

/// One tool call, rebuilt from the chunks a stream delivers it in.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl Model {
    /// Both the key and the endpoint are read here rather than at config load,
    /// because either may live in a file that only this process should open.
    pub fn new(config: &Config) -> Result<Self, ConfigError> {
        let client = Client::with_config(
            OpenAIConfig::new()
                .with_api_key(config.api_key()?)
                .with_api_base(config.endpoint()?.trim_end_matches('/')),
        );

        Ok(Self {
            client,
            name: config.model.clone(),
            max_tool_calls: config.max_tool_calls,
            max_tool_bytes: config.max_tool_bytes,
        })
    }

    /// Streams a plain text answer, reporting each delta as it arrives so the
    /// subject line shows up long before the body is finished.
    pub async fn text(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        diff: Option<&Diff>,
        reporter: &mut Reporter,
    ) -> Result<String, ModelError> {
        self.run(messages, None, diff, reporter, true).await
    }

    /// Streams an answer constrained to a JSON schema, then parses it. Nothing
    /// is shown while it streams, because half an object is not readable.
    pub async fn structured<T: DeserializeOwned>(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        name: &str,
        schema: serde_json::Value,
        diff: Option<&Diff>,
        reporter: &mut Reporter,
    ) -> Result<T, ModelError> {
        let format = ResponseFormat::JsonSchema {
            json_schema: ResponseFormatJsonSchema {
                description: None,
                name: name.to_owned(),
                schema,
                strict: Some(true),
            },
        };

        let text = self
            .run(messages, Some(format), diff, reporter, false)
            .await?;

        serde_json::from_str(&text).map_err(|source| ModelError::Shape { source, text })
    }

    /// The tools are only offered when something was actually withheld, so a
    /// diff that fits entirely in the prompt can never spend a round trip on
    /// retrieval.
    fn tools(&self, diff: Option<&Diff>) -> Option<Vec<ChatCompletionTools>> {
        let diff = diff.filter(|diff| diff.has_withheld() && self.max_tool_calls > 0)?;
        let paths = diff.withheld_paths();

        Some(vec![
            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: "read_diff".to_owned(),
                    description: Some(format!(
                        "Read part of the diff of a file whose hunks were withheld from the \
                         prompt. Withheld files: {}.",
                        paths.join(", ")
                    )),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "One of the withheld files."},
                            "offset": {"type": "integer", "description": "First line of the diff to return."},
                            "limit": {"type": "integer", "description": "How many lines to return."}
                        },
                        "required": ["path", "offset", "limit"],
                        "additionalProperties": false
                    })),
                    strict: Some(true),
                },
            }),
            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: "search_diff".to_owned(),
                    description: Some(
                        "Search every withheld diff for a regular expression. Use this to find \
                         the one dependency or symbol that explains a large withheld change."
                            .to_owned(),
                    ),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "pattern": {"type": "string", "description": "A regular expression, case insensitive."}
                        },
                        "required": ["pattern"],
                        "additionalProperties": false
                    })),
                    strict: Some(true),
                },
            }),
        ])
    }

    async fn run(
        &self,
        mut messages: Vec<ChatCompletionRequestMessage>,
        format: Option<ResponseFormat>,
        diff: Option<&Diff>,
        reporter: &mut Reporter,
        show_deltas: bool,
    ) -> Result<String, ModelError> {
        let mut calls_left = self.max_tool_calls;

        loop {
            let mut builder = CreateChatCompletionRequestArgs::default();

            builder
                .model(&self.name)
                .messages(messages.clone())
                .temperature(0.2f32)
                .stream(true);

            if let Some(format) = format.clone() {
                builder.response_format(format);
            }

            if calls_left > 0 {
                if let Some(tools) = self.tools(diff) {
                    builder.tools(tools);
                }
            }

            let request = builder.build()?;

            // Deserialised into this crate's own Chunk rather than the
            // library's, because a reasoning model's thinking arrives in a
            // field the OpenAI schema does not have.
            let mut stream = self
                .client
                .chat()
                .create_stream_byot::<_, Chunk>(request)
                .await?;

            let mut content = String::new();
            let mut calls: Vec<PartialCall> = Vec::new();

            while let Some(chunk) = stream.next().await {
                let Some(choice) = chunk?.choices.into_iter().next() else {
                    continue;
                };

                if let Some(text) = choice.delta.reasoning() {
                    if !text.is_empty() {
                        reporter.event(Event::Reasoning { text });
                    }
                }

                if let Some(text) = choice.delta.content {
                    if !text.is_empty() {
                        content.push_str(&text);

                        // A structured answer is not readable half-written, so
                        // only the plain text path shows its deltas.
                        reporter.event(if show_deltas {
                            Event::Delta { text }
                        } else {
                            Event::Writing {
                                bytes: content.len(),
                            }
                        });
                    }
                }

                for part in choice.delta.tool_calls.into_iter().flatten() {
                    let index = part.index as usize;

                    if calls.len() <= index {
                        calls.resize_with(index + 1, PartialCall::default);
                    }

                    if let Some(id) = part.id {
                        calls[index].id = id;
                    }

                    if let Some(function) = part.function {
                        if let Some(name) = function.name {
                            calls[index].name = name;
                        }
                        if let Some(arguments) = function.arguments {
                            calls[index].arguments.push_str(&arguments);
                        }
                    }
                }
            }

            if calls.is_empty() {
                reporter.end_stream();

                return if content.trim().is_empty() {
                    Err(ModelError::Empty)
                } else {
                    Ok(content)
                };
            }

            // A model that keeps asking has to stop somewhere. Answering the
            // last round with the budget exhausted is better than looping.
            calls_left = calls_left.saturating_sub(1);

            messages.push(ChatCompletionRequestMessage::Assistant(
                ChatCompletionRequestAssistantMessage {
                    content: (!content.is_empty())
                        .then_some(ChatCompletionRequestAssistantMessageContent::Text(content)),
                    tool_calls: Some(
                        calls
                            .iter()
                            .map(|call| {
                                ChatCompletionMessageToolCalls::Function(
                                    ChatCompletionMessageToolCall {
                                        id: call.id.clone(),
                                        function: FunctionCall {
                                            name: call.name.clone(),
                                            arguments: call.arguments.clone(),
                                        },
                                    },
                                )
                            })
                            .collect(),
                    ),
                    ..Default::default()
                },
            ));

            for call in calls {
                let arguments: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or(json!({}));

                let result = match diff {
                    Some(diff) => diff.call_tool(&call.name, &arguments, self.max_tool_bytes),
                    None => "no diff is available to read".to_owned(),
                };

                reporter.event(Event::Tool {
                    name: call.name.clone(),
                    arguments,
                    returned_bytes: result.len(),
                });

                messages.push(ChatCompletionRequestMessage::Tool(
                    ChatCompletionRequestToolMessage {
                        content: ChatCompletionRequestToolMessageContent::Text(result),
                        tool_call_id: call.id,
                    },
                ));
            }
        }
    }
}

/// This crate's own view of a streamed chunk. The library's type is close, but
/// it has no field for the thinking a reasoning model emits before its answer,
/// and that thinking is the only sign of life during a long wait.
///
/// Every field is optional and unknown fields are ignored, so a gateway that
/// sends more than this still parses.
#[derive(Debug, serde::Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
}

#[derive(Debug, serde::Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,

    /// Not in the OpenAI schema. Providers disagree on the name, so both
    /// spellings in the wild are accepted.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,

    #[serde(default)]
    tool_calls: Option<Vec<ToolCallChunk>>,
}

impl ChunkDelta {
    fn reasoning(&self) -> Option<String> {
        self.reasoning_content
            .clone()
            .or_else(|| self.reasoning.clone())
    }
}

#[derive(Debug, serde::Deserialize)]
struct ToolCallChunk {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionChunk>,
}

#[derive(Debug, serde::Deserialize)]
struct FunctionChunk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

pub fn assistant(text: impl Into<String>) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
        content: Some(ChatCompletionRequestAssistantMessageContent::Text(
            text.into(),
        )),
        ..Default::default()
    })
}
