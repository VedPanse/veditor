//! Codex chat state and interaction helpers.

use crate::*;

impl CodexChat {
    /// Creates a fresh chat session scoped to the current working project.
    pub(crate) fn new(root: &Path, selected_file: &Path) -> Self {
        let working_project = selected_file.parent().unwrap_or(root);
        let working_label = relative_to_root(root, working_project);

        let mut chat = Self {
            messages: Vec::new(),
            input: String::new(),
            last_change_set: None,
            history_scroll: 0,
            change_scroll: 0,
        };
        chat.push_assistant(&format!(
			"minimal codex chat ready.\nworking project: {working_label}\nask something here and i will keep the selected project as context."
		));
        chat
    }

    /// Appends a resolved assistant message to the transcript.
    pub(crate) fn push_assistant(&mut self, content: &str) {
        self.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: content.to_string(),
            pending_request_id: None,
        });
    }

    /// Appends the placeholder message shown while a Codex request is in flight.
    pub(crate) fn push_pending(&mut self, request_id: u64) {
        self.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: "thinking...".to_string(),
            pending_request_id: Some(request_id),
        });
    }

    /// Updates the chat transcript after switching the active project root.
    pub(crate) fn switch_project(&mut self, root: &Path, selected_target: &Path) {
        let working_project = if selected_target.is_dir() {
            selected_target
        } else {
            selected_target.parent().unwrap_or(root)
        };
        let working_label = relative_to_root(root, working_project);
        self.last_change_set = None;
        self.history_scroll = usize::MAX;
        self.change_scroll = 0;
        self.push_assistant(&format!("switched project context to {working_label}."));
    }

    /// Replaces the pending assistant placeholder with the final response content.
    pub(crate) fn resolve_pending(&mut self, request_id: u64, content: String) {
        if let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.pending_request_id == Some(request_id))
        {
            message.content = content;
            message.pending_request_id = None;
            return;
        }

        self.push_assistant(&content);
    }

    /// Formats the current chat transcript for the Codex CLI request payload.
    pub(crate) fn api_transcript(&self) -> String {
        let mut lines = Vec::new();
        for message in &self.messages {
            if message.pending_request_id.is_some() {
                continue;
            }

            let role = match message.role {
                ChatRole::User => "User",
                ChatRole::Assistant => "Assistant",
            };
            lines.push(format!("{role}: {}", message.content));
        }
        lines.join("\n\n")
    }

    /// Stores the most recent Codex-generated change set for UI display and undo.
    pub(crate) fn set_change_set(&mut self, change_set: Option<CodexChangeSet>) {
        self.last_change_set = change_set;
        self.change_scroll = 0;
    }

    /// Clears the currently tracked change set.
    pub(crate) fn clear_change_set(&mut self) {
        self.last_change_set = None;
        self.change_scroll = 0;
    }

    /// Scrolls the chat history viewport by a signed delta.
    pub(crate) fn scroll_history(&mut self, delta: isize) {
        self.history_scroll = self.history_scroll.saturating_add_signed(delta);
    }

    /// Scrolls the Codex change list viewport by a signed delta.
    pub(crate) fn scroll_change_list(&mut self, delta: isize) {
        self.change_scroll = self.change_scroll.saturating_add_signed(delta);
    }
}
