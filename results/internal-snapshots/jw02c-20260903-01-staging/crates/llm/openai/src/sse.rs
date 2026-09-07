use super::wire;
use jingwei_llm::*;

/// Byte framing prevents UTF-8 replacement when an HTTP chunk splits a scalar.
pub(super) struct Decoder {
    line: Vec<u8>,
    data: Vec<String>,
    accumulator: GenerationAccumulator,
    reason: Option<FinishReason>,
    usage: TokenUsage,
    done: bool,
    bytes: usize,
    limit: usize,
}

impl Decoder {
    pub fn new(limits: GenerationLimits) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            accumulator: GenerationAccumulator::new(limits),
            reason: None,
            usage: TokenUsage::default(),
            done: false,
            bytes: 0,
            limit: limits.max_output_bytes,
        }
    }
    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<GenerationStreamEvent>, LlmError> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or(ModelProtocolError::OutputLimitExceeded)?;
        if self.bytes > self.limit {
            return Err(ModelProtocolError::OutputLimitExceeded.into());
        }
        let mut events = Vec::new();
        for byte in bytes {
            if self.done {
                break;
            }
            if *byte != b'\n' {
                self.line.push(*byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = std::str::from_utf8(&line)
                .map_err(|_| wire::invalid("invalid UTF-8 in SSE"))?
                .trim_end_matches('\r');
            if line.is_empty() {
                self.dispatch(&mut events)?;
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            } // SSE comments, event/id/retry fields are framing metadata only.
        }
        Ok(events)
    }

    fn dispatch(&mut self, events: &mut Vec<GenerationStreamEvent>) -> Result<(), LlmError> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data).join("\n");
        if data.trim().is_empty() {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            let reason = self
                .reason
                .clone()
                .ok_or(ModelProtocolError::MissingStreamTerminal)?;
            events.push(GenerationStreamEvent::Finished(
                self.accumulator.response(reason, self.usage.clone())?,
            ));
            self.done = true;
            return Ok(());
        }
        let chunk: wire::Chunk =
            serde_json::from_str(&data).map_err(|_| wire::invalid("invalid SSE JSON"))?;
        if let Some(usage) = chunk.usage {
            self.usage = usage.into();
        }
        if chunk.choices.len() > 1 {
            return Err(wire::invalid("multiple stream choices are unsupported"));
        }
        for choice in chunk.choices {
            if choice.index != 0 || self.reason.is_some() {
                return Err(wire::invalid("invalid or post-terminal stream choice"));
            }
            if choice
                .delta
                .role
                .as_deref()
                .is_some_and(|role| role != "assistant")
                || choice.delta.function_call.is_some()
            {
                return Err(wire::invalid("unsupported streamed message"));
            }
            if let Some(text) = choice.delta.content {
                self.emit(GenerationDelta::Text { text }, events)?;
            }
            for call in choice.delta.tool_calls.unwrap_or_default() {
                if call.kind.as_deref().is_some_and(|kind| kind != "function") {
                    return Err(wire::invalid("unsupported streamed tool kind"));
                }
                let function = call.function.unwrap_or_default();
                self.emit(
                    GenerationDelta::ToolCall {
                        index: call.index,
                        id: call.id,
                        name: function.name,
                        arguments: function.arguments.unwrap_or_default(),
                    },
                    events,
                )?;
            }
            if let Some(reason) = choice.finish_reason {
                self.reason = Some(wire::reason(Some(reason)));
            }
        }
        Ok(())
    }
    fn emit(
        &mut self,
        delta: GenerationDelta,
        events: &mut Vec<GenerationStreamEvent>,
    ) -> Result<(), LlmError> {
        self.accumulator.push(&delta)?;
        events.push(GenerationStreamEvent::Delta(delta));
        Ok(())
    }
}
