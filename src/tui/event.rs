use crate::core::ControllerEvent;

use super::app::{App, LogEntry, PendingPermission, ToolOutcome};

impl App {
    pub(super) fn handle_agent_event(&mut self, event: ControllerEvent) {
        match event {
            ControllerEvent::Usage {
                in_tokens,
                out_tokens,
                cache_write,
                cache_read,
            } => {
                self.push(LogEntry::Usage {
                    in_tokens,
                    out_tokens,
                    cache_write,
                    cache_read,
                });
            }
            ControllerEvent::AssistantText { text } => {
                self.push(LogEntry::Assistant(text));
                self.push(LogEntry::Blank);
            }
            ControllerEvent::AssistantThinking { text } => {
                self.push(LogEntry::Thinking(text));
                self.push(LogEntry::Blank);
            }
            ControllerEvent::ToolUseStart { id, name, summary } => {
                self.status = format!("running {name}");
                self.push(LogEntry::ToolCall {
                    id,
                    name,
                    summary,
                    result: None,
                });
            }
            ControllerEvent::PermissionRequest {
                tool_use_id,
                tool_name,
                summary,
            } => {
                self.pending = Some(PendingPermission {
                    tool_use_id,
                    tool_name,
                    summary,
                    scroll: 0,
                });
            }
            ControllerEvent::PermissionResolved { tool_name, allow } => {
                // Clear here too: on replay there was no keypress to take `pending`.
                self.pending = None;
                self.push(if allow {
                    LogEntry::Allow(tool_name)
                } else {
                    LogEntry::Deny(tool_name)
                });
                self.auto_scroll = true;
            }
            ControllerEvent::UserMessage { text, sender } => {
                // Rendered from the broker's echo, so live input and replay share one path.
                self.push(LogEntry::User { text, sender });
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
            ControllerEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let outcome = ToolOutcome { content, is_error };
                // Resolve the matching ToolCall newest-first.
                let idx = self.log.iter().rposition(
                    |e| matches!(e, LogEntry::ToolCall { id: eid, result: None, .. } if *eid == id),
                );
                match idx {
                    Some(i) => {
                        if let LogEntry::ToolCall { result, .. } = &mut self.log[i] {
                            *result = Some(outcome);
                        }
                    }
                    None => {
                        // No pending ToolCall (shouldn't happen) — show it anyway.
                        self.push(LogEntry::ToolCall {
                            id,
                            name: "(unknown tool)".into(),
                            summary: String::new(),
                            result: Some(outcome),
                        });
                    }
                }
                self.push(LogEntry::Blank);
                self.status = "thinking".into();
            }
            ControllerEvent::TurnComplete => {
                self.status.clear();
            }
            ControllerEvent::MaxIterations => {
                self.push(LogEntry::Warn("hit MAX_ITERATIONS".into()));
                self.status.clear();
            }
            ControllerEvent::Notice { text } => {
                // One Info row per line — Info flattens newlines, so split to stay readable.
                for line in text.lines() {
                    self.push(LogEntry::Info(line.to_string()));
                }
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
            ControllerEvent::Warn { text } => {
                self.push(LogEntry::Warn(text));
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
            ControllerEvent::Error { message } => {
                self.push(LogEntry::Error(message));
                self.status.clear();
            }
            // Adopt the daemon's context as the header (replayed first on attach).
            ControllerEvent::SessionInfo {
                model,
                cwd,
                git_branch,
                session_id,
                session_name,
            } => {
                self.model = model;
                self.cwd_display = cwd;
                self.git_branch = git_branch;
                self.session_id = session_id;
                self.session_name = session_name;
            }
        }
    }
}
