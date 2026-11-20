use super::protocol::{Event, SpeakerWord};
use crate::output::{Output, SpeakerSpan};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, HashMap};

const MAX_PENDING: usize = 64;
const MAX_SPEAKERS: usize = 128;

/// One server session. Speaker display numbers come from Output and are never
/// reused after reconnecting, since the provider does not identify real people.
pub struct TranscriptState {
    diarize: bool,
    pub pending_speech: bool,
    server_session: String,
    pending: BTreeMap<u64, String>,
    last_emitted: Option<u64>,
    aliases: HashMap<String, String>,
    labels: HashMap<String, String>,
}

impl TranscriptState {
    pub fn new(diarize: bool) -> Self {
        Self {
            diarize,
            pending_speech: false,
            server_session: String::new(),
            pending: BTreeMap::new(),
            last_emitted: None,
            aliases: HashMap::new(),
            labels: HashMap::new(),
        }
    }
    pub fn start_session(&mut self, id: String) {
        self.server_session = id;
    }
    pub fn has_pending_labels(&self) -> bool {
        !self.pending.is_empty()
    }
    fn emitted(&self, index: u64) -> bool {
        self.last_emitted.is_some_and(|last| index <= last)
    }

    fn canonical(&self, id: &str) -> String {
        let mut current = id;
        for _ in 0..MAX_SPEAKERS {
            match self.aliases.get(current) {
                Some(next) => current = next,
                None => break,
            }
        }
        current.into()
    }
    fn label(&mut self, id: &str, output: &mut Output) -> Result<String> {
        if let Some(label) = self.labels.get(id) {
            return Ok(label.clone());
        }
        ensure!(
            self.labels.len() + self.aliases.len() < MAX_SPEAKERS,
            "Speaker tracking exceeded its session limit"
        );
        let label = output.allocate_speaker_label();
        self.labels.insert(id.into(), label.clone());
        Ok(label)
    }

    fn unknown(text: String) -> Vec<SpeakerSpan> {
        vec![SpeakerSpan {
            speaker_id: None,
            speaker_label: "Unknown speaker".into(),
            text,
        }]
    }

    fn spans(
        &mut self,
        text: &str,
        words: &[SpeakerWord],
        output: &mut Output,
    ) -> Result<Vec<SpeakerSpan>> {
        if words.is_empty() {
            return Ok(Self::unknown(text.into()));
        }
        let mut spans: Vec<SpeakerSpan> = Vec::new();
        let mut cursor = 0;
        for word in words {
            if word.text.is_empty() {
                continue;
            }
            let Some(offset) = text[cursor..].find(&word.text) else {
                tracing::warn!(
                    "Speaker words could not be aligned with final text; preserving text with an unknown speaker"
                );
                return Ok(Self::unknown(text.into()));
            };
            let end = cursor + offset + word.text.len();
            let id = if word.kind.as_deref() == Some("spacing") || word.text.trim().is_empty() {
                spans.last().and_then(|span| span.speaker_id.clone())
            } else {
                word.speaker_id.as_deref().map(|id| self.canonical(id))
            };
            if let Some(previous) = spans.last_mut().filter(|span| span.speaker_id == id) {
                previous.text.push_str(&text[cursor..end]);
            } else {
                let label = match &id {
                    Some(id) => self.label(id, output)?,
                    None => "Unknown speaker".into(),
                };
                spans.push(SpeakerSpan {
                    speaker_id: id,
                    speaker_label: label,
                    text: text[cursor..end].into(),
                });
            }
            cursor = end;
        }
        if let Some(last) = spans.last_mut() {
            last.text.push_str(&text[cursor..]);
        } else {
            return Ok(Self::unknown(text.into()));
        }
        Ok(spans)
    }

    pub fn handle(&mut self, event: Event, output: &mut Output) -> Result<()> {
        match event {
            Event::Partial { text, words } => {
                if !text.is_empty() {
                    self.pending_speech = true;
                }
                let text = if self.diarize && !text.is_empty() {
                    self.spans(&text, &words, output)?
                        .iter()
                        .map(|span| format!("{}: {}", span.speaker_label, span.text.trim()))
                        .collect::<Vec<_>>()
                        .join(" · ")
                } else {
                    text
                };
                output.partial(text)?;
            }
            Event::Committed {
                text,
                segment_index,
            } => {
                // A repeated old commit must not clear a newer live partial.
                if let Some(index) = segment_index {
                    if self.emitted(index) {
                        return Ok(());
                    }
                    if self.diarize
                        && let Some(existing) = self.pending.get(&index)
                    {
                        ensure!(
                            existing == &text,
                            "Server changed an already committed segment"
                        );
                        return Ok(());
                    }
                }
                self.pending_speech = false;
                output.partial(String::new())?;
                if !self.diarize {
                    if let Some(index) = segment_index {
                        if self.emitted(index) {
                            return Ok(());
                        }
                        self.last_emitted = Some(index);
                    }
                    output.committed(text)?;
                } else {
                    let Some(index) = segment_index else {
                        let spans = Self::unknown(text.clone());
                        output.committed_with_speakers(
                            text,
                            Some(self.server_session.clone()),
                            None,
                            Some(spans),
                        )?;
                        anyhow::bail!(
                            "Diarization was requested but a committed event lacked segment_index; final text was saved without speaker identification"
                        );
                    };
                    if self.emitted(index) {
                        return Ok(());
                    }
                    if let Some(existing) = self.pending.get(&index) {
                        ensure!(
                            existing == &text,
                            "Server changed an already committed segment"
                        );
                        return Ok(());
                    }
                    if self.pending.len() >= MAX_PENDING
                        || self.pending.values().map(String::len).sum::<usize>() + text.len()
                            > 512 * 1024
                    {
                        self.flush_unlabeled(output)?;
                        let spans = Self::unknown(text.clone());
                        self.last_emitted = Some(index);
                        output.committed_with_speakers(
                            text,
                            Some(self.server_session.clone()),
                            Some(index),
                            Some(spans),
                        )?;
                        anyhow::bail!(
                            "Speaker annotations are stalled; bounded pending transcript buffer is full. Committed text was preserved without speaker identification"
                        );
                    }
                    self.pending.insert(index, text);
                }
            }
            Event::Diarized {
                text,
                segment_index,
                words,
            } if self.diarize => {
                if self.emitted(segment_index) {
                    return Ok(());
                }
                let committed = self
                    .pending
                    .get(&segment_index)
                    .context("Speaker event has no matching committed segment")?;
                ensure!(
                    committed == &text,
                    "Speaker event text differs from its committed segment"
                );
                ensure!(
                    self.pending.first_key_value().map(|(index, _)| *index) == Some(segment_index),
                    "Speaker events arrived out of commit order"
                );
                let spans = self.spans(&text, &words, output)?;
                self.pending.remove(&segment_index);
                self.last_emitted = Some(segment_index);
                output.committed_with_speakers(
                    text,
                    Some(self.server_session.clone()),
                    Some(segment_index),
                    Some(spans),
                )?;
            }
            Event::SpeakersMerged {
                from_speaker_id,
                to_speaker_id,
            } if self.diarize => {
                let from = self.canonical(&from_speaker_id);
                let to = self.canonical(&to_speaker_id);
                if from == to {
                    return Ok(());
                }
                let from_label = self.label(&from, output)?;
                let to_label = self.label(&to, output)?;
                ensure!(
                    self.labels.len() + self.aliases.len() < MAX_SPEAKERS,
                    "Speaker merge tracking exceeded its session limit"
                );
                self.aliases.insert(from.clone(), to.clone());
                output.speaker_merge(&self.server_session, &from, &to, &from_label, &to_label)?;
            }
            Event::Warning { warning } => tracing::warn!("ElevenLabs: {warning}"),
            Event::Started { .. } => {
                anyhow::bail!("Unexpected second session_started on the same connection")
            }
            Event::Other | Event::Diarized { .. } | Event::SpeakersMerged { .. } => {
                tracing::debug!("Ignoring unrequested server metadata")
            }
        }
        Ok(())
    }

    /// Preserve committed text if a transport failure or shutdown deadline prevents
    /// receiving its speaker annotation. Never save an uncommitted partial.
    pub fn flush_unlabeled(&mut self, output: &mut Output) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        tracing::warn!(
            segments = self.pending.len(),
            "Speaker annotations were not received; preserving committed text with unknown speaker labels"
        );
        while let Some((index, text)) = self.pending.pop_first() {
            self.last_emitted = Some(index);
            let spans = Self::unknown(text.clone());
            output.committed_with_speakers(
                text,
                Some(self.server_session.clone()),
                Some(index),
                Some(spans),
            )?;
        }
        Ok(())
    }
}
