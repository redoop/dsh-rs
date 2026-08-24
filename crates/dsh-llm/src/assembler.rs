//! [`BlockAssembler`]: fold a raw [`StreamChunk`] stream back into complete
//! [`ContentBlock`]s, usage, finish reason, and an assistant [`Message`].
//!
//! The agent loop logs raw chunks for replay fidelity while feeding the same
//! chunks through the assembler, then stores the assembled content.

use std::collections::HashMap;

use crate::types::{
    CallId, ContentBlock, FinishReason, Message, MessageId, MessageSource, Role, StreamChunk,
    TokenUsage,
};

/// In-progress assembly state of one block index.
#[derive(Debug, Clone)]
enum PartialBlock {
    Text { text: String },
    Reasoning { text: String },
    ToolCall { id: CallId, name: Option<String>, arguments: String },
}

impl PartialBlock {
    fn assemble(self) -> ContentBlock {
        match self {
            PartialBlock::Text { text } => ContentBlock::Text { text },
            PartialBlock::Reasoning { text } => ContentBlock::Reasoning { text },
            PartialBlock::ToolCall { id, name, arguments } => ContentBlock::ToolCall {
                id,
                name: name.unwrap_or_else(|| "tool".to_string()),
                arguments,
            },
        }
    }

    fn has_visible_content(&self) -> bool {
        match self {
            PartialBlock::Text { text } | PartialBlock::Reasoning { text } => {
                !text.trim().is_empty()
            }
            PartialBlock::ToolCall { .. } => false,
        }
    }
}

/// Incrementally assembles raw chunks into complete blocks and a final
/// assistant message. Tolerant of delta-only protocols (no block-start/end).
#[derive(Debug, Default)]
pub struct BlockAssembler {
    partials: HashMap<usize, PartialBlock>,
    /// Indexes in first-seen stream order.
    order: Vec<usize>,
    /// Indexes closed by `block-end`, in stream order.
    closed: Vec<usize>,
    /// Authoritative blocks delivered by `block-end`, keyed by index.
    finished_blocks: HashMap<usize, ContentBlock>,
    usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    finished: bool,
}

impl BlockAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk, in stream order. Chunks after the terminal `finish`
    /// are ignored, as are deltas for an index already closed by `block-end`.
    pub fn push(&mut self, chunk: &StreamChunk) {
        if self.finished {
            return;
        }
        match chunk {
            StreamChunk::BlockStart { index, block_type } => {
                self.partials
                    .entry(*index)
                    .or_insert_with(|| match block_type.as_str() {
                        "reasoning" => PartialBlock::Reasoning { text: String::new() },
                        "tool-call" => PartialBlock::ToolCall {
                            id: String::new(),
                            name: None,
                            arguments: String::new(),
                        },
                        _ => PartialBlock::Text { text: String::new() },
                    });
                self.observe(*index);
            }
            StreamChunk::TextDelta { index, text } => {
                if self.is_closed(*index) {
                    return;
                }
                if !self.partials.contains_key(index) {
                    self.observe(*index);
                    self.partials.insert(*index, PartialBlock::Text { text: String::new() });
                }
                if let Some(PartialBlock::Text { text: out }) = self.partials.get_mut(index) {
                    out.push_str(text);
                }
            }
            StreamChunk::ReasoningDelta { index, text } => {
                if self.is_closed(*index) {
                    return;
                }
                if !self.partials.contains_key(index) {
                    self.observe(*index);
                    self.partials.insert(*index, PartialBlock::Reasoning { text: String::new() });
                }
                if let Some(PartialBlock::Reasoning { text: out }) = self.partials.get_mut(index) {
                    out.push_str(text);
                }
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                if self.is_closed(*index) {
                    return;
                }
                if !self.partials.contains_key(index) {
                    self.observe(*index);
                    self.partials.insert(
                        *index,
                        PartialBlock::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                        },
                    );
                }
                if let PartialBlock::ToolCall {
                    id: out_id,
                    name: out_name,
                    arguments,
                } = self.partials.get_mut(index).expect("inserted above")
                {
                    if out_id.is_empty() {
                        *out_id = id.clone();
                    }
                    if out_name.is_none() {
                        *out_name = name.clone();
                    }
                    arguments.push_str(arguments_delta);
                }
            }
            StreamChunk::BlockEnd { index, block } => {
                if self.is_closed(*index) {
                    return;
                }
                // Adapter assembled the block authoritatively; drop our partial.
                self.partials.remove(index);
                self.closed.push(*index);
                self.observe(*index);
                self.finished_blocks.insert(*index, block.clone());
            }
            StreamChunk::Usage { usage } => {
                self.usage = Some(*usage);
            }
            StreamChunk::Finish { reason } => {
                self.finish = Some(reason.clone());
                self.finished = true;
            }
        }
    }

    fn observe(&mut self, index: usize) {
        if !self.order.contains(&index) {
            self.order.push(index);
        }
    }

    fn is_closed(&self, index: usize) -> bool {
        self.closed.contains(&index)
    }

    /// All blocks seen so far, in stream order. A `max-tokens` finish drops
    /// every tool call: a truncated call is unsafe to execute.
    pub fn blocks(&self) -> Vec<ContentBlock> {
        let drop_tools = matches!(self.finish, Some(FinishReason::MaxTokens));
        let mut blocks = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for index in &self.order {
            if seen.contains(index) {
                continue;
            }
            if let Some(block) = self.finished_blocks.get(index) {
                if drop_tools && matches!(block, ContentBlock::ToolCall { .. }) {
                    continue;
                }
                blocks.push(block.clone());
                seen.insert(index);
                continue;
            }
            if let Some(partial) = self.partials.get(index) {
                let block = partial.clone().assemble();
                if drop_tools && matches!(block, ContentBlock::ToolCall { .. }) {
                    continue;
                }
                blocks.push(block);
                seen.insert(index);
            }
        }
        blocks
    }

    /// The prefix an interrupted stream can safely finalize: closed and open
    /// text/reasoning blocks with non-whitespace content. Tool calls omitted.
    pub fn interrupted_blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        for index in &self.order {
            let partial = match self.partials.get(index) {
                Some(partial) => partial,
                None => match self.finished_blocks.get(index) {
                    Some(ContentBlock::Text { text }) if !text.trim().is_empty() => {
                        blocks.push(ContentBlock::Text { text: text.clone() });
                        continue;
                    }
                    Some(ContentBlock::Reasoning { text }) if !text.trim().is_empty() => {
                        blocks.push(ContentBlock::Reasoning { text: text.clone() });
                        continue;
                    }
                    _ => continue,
                },
            };
            if partial.has_visible_content() {
                blocks.push(partial.clone().assemble());
            }
        }
        blocks
    }

    /// Usage from the `usage` chunk; `None` until one arrives.
    pub fn usage(&self) -> Option<TokenUsage> {
        self.usage
    }

    /// Finish reason; `Stop` when the stream ended without a finish chunk.
    pub fn finish(&self) -> FinishReason {
        self.finish
            .clone()
            .unwrap_or(FinishReason::Stop)
    }

    /// The assembled assistant message over `blocks()`.
    pub fn message(&self, id: impl Into<MessageId>, source: MessageSource) -> Message {
        Message {
            id: id.into(),
            role: Role::Assistant,
            content: self.blocks(),
            source,
        }
    }
}
