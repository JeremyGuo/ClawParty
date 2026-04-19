use agent_frame::{ChatMessage, SessionEvent};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// A single entry in the append-only transcript log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub seq: usize,
    pub ts: String,
    #[serde(rename = "type")]
    pub entry_type: TranscriptEntryType,

    // user_message fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attachment_count: usize,

    // model_call fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_message: Option<ChatMessage>,

    // tool_result fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errored: Option<bool>,

    // compaction fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<usize>,
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptEntryType {
    UserMessage,
    ModelCall,
    ToolResult,
    Compaction,
}

/// Lightweight skeleton of a transcript entry for paginated list responses.
/// Does not include full tool output or full assistant message content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptEntrySkeleton {
    pub seq: usize,
    pub ts: String,
    #[serde(rename = "type")]
    pub entry_type: TranscriptEntryType,

    // user_message
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attachment_count: usize,

    // model_call
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Truncated to ~200 chars for list view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_text_preview: Option<String>,
    /// Names of tools called in this round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_names: Vec<String>,

    // tool_result
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errored: Option<bool>,

    // compaction
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<usize>,
}

const ASSISTANT_PREVIEW_LIMIT: usize = 200;

impl TranscriptEntry {
    /// Convert to a lightweight skeleton for paginated list responses.
    pub fn to_skeleton(&self) -> TranscriptEntrySkeleton {
        let (assistant_text_preview, tool_call_names) = if let Some(msg) = &self.assistant_message {
            let preview = extract_text_preview(msg, ASSISTANT_PREVIEW_LIMIT);
            let tool_names = msg
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().map(|tc| tc.function.name.clone()).collect())
                .unwrap_or_default();
            (preview, tool_names)
        } else {
            (None, Vec::new())
        };

        TranscriptEntrySkeleton {
            seq: self.seq,
            ts: self.ts.clone(),
            entry_type: self.entry_type.clone(),
            text: self.text.clone(),
            attachment_count: self.attachment_count,
            round: self.round,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            assistant_text_preview,
            tool_call_names,
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            output_len: self.output_len,
            errored: self.errored,
            tokens_before: self.tokens_before,
            tokens_after: self.tokens_after,
        }
    }
}

fn extract_text_preview(message: &ChatMessage, limit: usize) -> Option<String> {
    let content = message.content.as_ref()?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => {
            let mut combined = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !combined.is_empty() {
                        combined.push(' ');
                    }
                    combined.push_str(text);
                }
            }
            combined
        }
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    if text.len() <= limit {
        Some(text)
    } else {
        let truncated: String = text.chars().take(limit).collect();
        Some(format!("{}…", truncated))
    }
}

/// Manages the append-only transcript log for a session.
#[derive(Debug)]
pub struct SessionTranscript {
    path: PathBuf,
    next_seq: usize,
}

impl SessionTranscript {
    /// Open or create a transcript file. Reads existing entries to determine next_seq.
    pub fn open(session_root: &Path) -> Result<Self> {
        let path = session_root.join("transcript.jsonl");
        let next_seq = if path.exists() {
            count_lines(&path)?
        } else {
            0
        };
        Ok(Self { path, next_seq })
    }

    /// Append a transcript entry derived from a SessionEvent.
    /// Returns the entry if one was written, or None if the event is not transcript-relevant.
    pub fn record_event(&mut self, event: &SessionEvent) -> Result<Option<TranscriptEntry>> {
        let entry = match event {
            SessionEvent::UserMessageReceived {
                text,
                attachment_count,
            } => TranscriptEntry {
                seq: self.next_seq,
                ts: now_rfc3339(),
                entry_type: TranscriptEntryType::UserMessage,
                text: text.clone(),
                attachment_count: *attachment_count,
                round: None,
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                assistant_message: None,
                tool_call_id: None,
                tool_name: None,
                output: None,
                output_len: None,
                errored: None,
                tokens_before: None,
                tokens_after: None,
            },
            SessionEvent::ModelCallCompleted {
                round_index,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                assistant_message,
                ..
            } => TranscriptEntry {
                seq: self.next_seq,
                ts: now_rfc3339(),
                entry_type: TranscriptEntryType::ModelCall,
                text: None,
                attachment_count: 0,
                round: Some(*round_index),
                prompt_tokens: Some(*prompt_tokens),
                completion_tokens: Some(*completion_tokens),
                total_tokens: Some(*total_tokens),
                assistant_message: assistant_message.clone(),
                tool_call_id: None,
                tool_name: None,
                output: None,
                output_len: None,
                errored: None,
                tokens_before: None,
                tokens_after: None,
            },
            SessionEvent::ToolCallCompleted {
                round_index,
                tool_name,
                tool_call_id,
                output_len,
                errored,
                output,
            } => TranscriptEntry {
                seq: self.next_seq,
                ts: now_rfc3339(),
                entry_type: TranscriptEntryType::ToolResult,
                text: None,
                attachment_count: 0,
                round: Some(*round_index),
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                assistant_message: None,
                tool_call_id: Some(tool_call_id.clone()),
                tool_name: Some(tool_name.clone()),
                output: output.clone(),
                output_len: Some(*output_len),
                errored: Some(*errored),
                tokens_before: None,
                tokens_after: None,
            },
            SessionEvent::CompactionCompleted {
                compacted: true,
                estimated_tokens_before,
                estimated_tokens_after,
                ..
            }
            | SessionEvent::ToolWaitCompactionCompleted {
                compacted: true,
                estimated_tokens_before,
                estimated_tokens_after,
                ..
            } => TranscriptEntry {
                seq: self.next_seq,
                ts: now_rfc3339(),
                entry_type: TranscriptEntryType::Compaction,
                text: None,
                attachment_count: 0,
                round: None,
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                assistant_message: None,
                tool_call_id: None,
                tool_name: None,
                output: None,
                output_len: None,
                errored: None,
                tokens_before: Some(*estimated_tokens_before),
                tokens_after: Some(*estimated_tokens_after),
            },
            // All other events are not transcript-relevant
            _ => return Ok(None),
        };

        self.append(&entry)?;
        self.next_seq += 1;
        Ok(Some(entry))
    }

    /// Total number of entries in the transcript.
    pub fn len(&self) -> usize {
        self.next_seq
    }

    /// List transcript entries in reverse order (newest first) with pagination.
    /// `offset` is from the newest end: offset=0 means the latest entry.
    /// Returns skeleton entries (no full tool output or assistant message).
    pub fn list(&self, offset: usize, limit: usize) -> Result<Vec<TranscriptEntrySkeleton>> {
        if self.next_seq == 0 || offset >= self.next_seq {
            return Ok(Vec::new());
        }
        // Compute the seq range we need (in forward order)
        let newest_seq = self.next_seq.saturating_sub(1);
        let start_seq = newest_seq.saturating_sub(offset + limit - 1);
        let end_seq = newest_seq.saturating_sub(offset);

        let entries = self.read_range(start_seq, end_seq + 1)?;
        let mut skeletons: Vec<TranscriptEntrySkeleton> =
            entries.into_iter().map(|e| e.to_skeleton()).collect();
        skeletons.reverse(); // newest first
        Ok(skeletons)
    }

    /// Get full detail for a range of seq numbers [seq_start, seq_end).
    pub fn get_detail(&self, seq_start: usize, seq_end: usize) -> Result<Vec<TranscriptEntry>> {
        self.read_range(seq_start, seq_end)
    }

    fn append(&self, entry: &TranscriptEntry) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open transcript {}", self.path.display()))?;
        let line = serde_json::to_string(entry).context("failed to serialize transcript entry")?;
        writeln!(file, "{}", line)
            .with_context(|| format!("failed to write to transcript {}", self.path.display()))?;
        Ok(())
    }

    fn read_range(&self, seq_start: usize, seq_end: usize) -> Result<Vec<TranscriptEntry>> {
        if !self.path.exists() || seq_start >= seq_end {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open transcript {}", self.path.display()))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for (line_index, line) in reader.lines().enumerate() {
            if line_index < seq_start {
                continue;
            }
            if line_index >= seq_end {
                break;
            }
            let line = line.with_context(|| {
                format!(
                    "failed to read line {} of transcript {}",
                    line_index,
                    self.path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        line_index,
                        error = %err,
                        "skipping malformed transcript entry"
                    );
                }
            }
        }
        Ok(entries)
    }
}

fn count_lines(path: &Path) -> Result<usize> {
    let file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0usize;
    for line in reader.lines() {
        let _ = line?;
        count += 1;
    }
    Ok(count)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn transcript_append_and_list() {
        let tmp = TempDir::new().unwrap();
        let mut t = SessionTranscript::open(tmp.path()).unwrap();
        assert_eq!(t.len(), 0);

        // Record a user message
        let event = SessionEvent::UserMessageReceived {
            text: Some("hello world".to_string()),
            attachment_count: 0,
        };
        let entry = t.record_event(&event).unwrap();
        assert!(entry.is_some());
        assert_eq!(t.len(), 1);

        // Record a model call
        let event = SessionEvent::ModelCallCompleted {
            round_index: 0,
            tool_call_count: 0,
            prompt_tokens: 1000,
            completion_tokens: 200,
            total_tokens: 1200,
            assistant_message: Some(ChatMessage::text("assistant", "hi there")),
        };
        let entry = t.record_event(&event).unwrap();
        assert!(entry.is_some());
        assert_eq!(t.len(), 2);

        // List newest first
        let list = t.list(0, 10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].seq, 1); // newest first
        assert_eq!(list[1].seq, 0);
        assert_eq!(list[0].entry_type, TranscriptEntryType::ModelCall);
        assert_eq!(list[1].entry_type, TranscriptEntryType::UserMessage);

        // Get detail
        let detail = t.get_detail(0, 2).unwrap();
        assert_eq!(detail.len(), 2);
        assert_eq!(detail[0].seq, 0);
        assert!(detail[0].text.as_deref() == Some("hello world"));
        assert!(detail[1].assistant_message.is_some());

        // Pagination: offset=1, limit=1 should give only the user message
        let page = t.list(1, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].seq, 0);
    }

    #[test]
    fn transcript_reopen_preserves_seq() {
        let tmp = TempDir::new().unwrap();
        {
            let mut t = SessionTranscript::open(tmp.path()).unwrap();
            let event = SessionEvent::UserMessageReceived {
                text: Some("msg1".to_string()),
                attachment_count: 0,
            };
            t.record_event(&event).unwrap();
            let event = SessionEvent::UserMessageReceived {
                text: Some("msg2".to_string()),
                attachment_count: 0,
            };
            t.record_event(&event).unwrap();
            assert_eq!(t.len(), 2);
        }
        // Reopen
        let t = SessionTranscript::open(tmp.path()).unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn non_transcript_events_ignored() {
        let tmp = TempDir::new().unwrap();
        let mut t = SessionTranscript::open(tmp.path()).unwrap();
        let event = SessionEvent::RoundStarted {
            round_index: 0,
            message_count: 5,
        };
        let entry = t.record_event(&event).unwrap();
        assert!(entry.is_none());
        assert_eq!(t.len(), 0);
    }
}
