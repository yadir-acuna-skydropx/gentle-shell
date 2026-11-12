//! The real explainer backend: wires the keystore, config, context caps and the
//! Anthropic client into the engine's [`ExplainBackend`] trait.
use std::path::Path;

use crate::anthropic::{Client, Message};
use crate::config::ExplainConfig;
use crate::context;
use crate::engine::{CancelFlag, ExplainBackend, ExplainInput};
use crate::keystore::KeyStore;
use crate::protocol::KeyState;

/// Backend that streams Claude Haiku completions over the Anthropic API.
pub struct AnthropicBackend {
    client: Option<Client>,
    cfg: ExplainConfig,
    /// Resolved model id, or empty when unresolved / no key.
    model: String,
    /// Conversation so far (persisted across `ask`), starting with the explain.
    convo: Vec<Message>,
}

impl AnthropicBackend {
    /// Builds the backend from the key store and the user's home (for config),
    /// using the environment-resolved API base.
    pub fn new(store: &dyn KeyStore, home: &Path) -> Self {
        Self::new_with_base(store, home, &crate::anthropic::base_url())
    }

    /// Builds the backend against an explicit API `base` (tests).
    pub fn new_with_base(store: &dyn KeyStore, home: &Path, base: &str) -> Self {
        let cfg = ExplainConfig::load_from(home);
        let client = match store.load() {
            Ok(Some(key)) if !key.trim().is_empty() => Some(Client::new(key, base)),
            _ => None,
        };
        let mut backend = AnthropicBackend { client, cfg, model: String::new(), convo: Vec::new() };
        // Resolve the model up front so `status` can report it; failures leave
        // the model empty and are surfaced only when a request is made.
        if let Some(m) = backend.cfg.model.clone() {
            backend.model = m;
        } else if let Some(client) = &backend.client {
            if let Ok(id) = client.resolve_model("haiku", None) {
                backend.model = id;
            }
        }
        backend
    }

    fn ensure_model(&mut self) -> Result<String, String> {
        if !self.model.is_empty() {
            return Ok(self.model.clone());
        }
        let client = self.client.as_ref().ok_or_else(missing_key_msg)?;
        let id = client.resolve_model("haiku", self.cfg.model.as_deref()).map_err(|e| e.to_string())?;
        self.model = id.clone();
        Ok(id)
    }

    /// Streams one turn, forwarding deltas and accumulating the assistant reply.
    fn stream_turn(
        &mut self,
        system: &str,
        sink: &mut dyn FnMut(&str),
        cancel: &CancelFlag,
    ) -> Result<(), String> {
        let model = self.ensure_model()?;
        let client = self.client.as_ref().ok_or_else(missing_key_msg)?;
        let mut acc = String::new();
        let mut forward = |text: &str| {
            acc.push_str(text);
            sink(text);
        };
        let cancelled = || cancel.is_cancelled();
        let result = client.stream_message(system, &self.convo, &model, &mut forward, &cancelled);
        match result {
            Ok(()) => {
                if !cancel.is_cancelled() {
                    self.convo.push(Message::assistant(acc));
                }
                Ok(())
            }
            Err(e) => {
                // Drop the user turn we could not answer so the conversation stays consistent.
                self.convo.pop();
                Err(e.to_string())
            }
        }
    }
}

fn missing_key_msg() -> String {
    "no Anthropic API key — run `gentle-explain key set`".to_string()
}

impl ExplainBackend for AnthropicBackend {
    fn status(&self) -> (KeyState, String) {
        let key = if self.client.is_some() { KeyState::Set } else { KeyState::Missing };
        (key, self.model.clone())
    }

    fn explain(&mut self, input: &ExplainInput, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<(), String> {
        if self.client.is_none() {
            return Err(missing_key_msg());
        }
        let ctx = context::cap_context(&input.selection, &input.context_before, &input.context_after);
        let user = context::build_explain_prompt(&input.path, &input.language, &ctx);
        let system = context::system_prompt(&self.cfg.lang);
        self.convo = vec![Message::user(user)];
        self.stream_turn(&system, sink, cancel)
    }

    fn ask(&mut self, question: &str, sink: &mut dyn FnMut(&str), cancel: &CancelFlag) -> Result<(), String> {
        if self.client.is_none() {
            return Err(missing_key_msg());
        }
        if self.convo.is_empty() {
            return Err("select code and explain it first, then ask a follow-up".to_string());
        }
        // Answer in the language of the question, falling back to the config default.
        let lang = context::detect_lang(question, &self.cfg.lang);
        let system = context::system_prompt(&lang);
        self.convo.push(Message::user(question.to_string()));
        self.stream_turn(&system, sink, cancel)
    }
}

impl ExplainConfig {
    /// Convenience loader used by the backend (config file + env override).
    pub fn load_from(home: &Path) -> ExplainConfig {
        crate::config::load(home)
    }
}
