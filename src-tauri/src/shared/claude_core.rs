use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader as StdBufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::backend::events::{AppServerEvent, EventSink};
use crate::providers;
use crate::shared::process_core::tokio_command;
use crate::types::{AppSettings, ProviderKind, WorkspaceEntry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeMessageRecord {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeTurnRecord {
    pub(crate) id: String,
    #[serde(rename = "startedAt")]
    pub(crate) started_at: i64,
    #[serde(rename = "completedAt")]
    pub(crate) completed_at: Option<i64>,
    pub(crate) items: Vec<ClaudeMessageRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeThreadRecord {
    pub(crate) id: String,
    pub(crate) cwd: String,
    pub(crate) preview: String,
    #[serde(rename = "createdAt")]
    pub(crate) created_at: i64,
    #[serde(rename = "updatedAt")]
    pub(crate) updated_at: i64,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) turns: Vec<ClaudeTurnRecord>,
}

pub(crate) type ClaudeThreadsStore = Arc<Mutex<HashMap<String, Vec<ClaudeThreadRecord>>>>;
pub(crate) type ClaudeTurnCancelsStore = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;

const CLAUDE_THREADS_FILE_NAME: &str = "claude_threads.json";
const CLAUDE_ARCHIVED_THREADS_FILE_NAME: &str = "claude_archived_threads.json";
const CLAUDE_HISTORY_ROOT: &str = ".claude/projects";
const MAX_IMPORTED_TURNS_PER_THREAD: usize = 200;

pub(crate) fn claude_threads_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CLAUDE_THREADS_FILE_NAME)
}

pub(crate) fn read_threads_snapshot(
    path: &Path,
) -> Result<HashMap<String, Vec<ClaudeThreadRecord>>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let data = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&data).map_err(|error| error.to_string())
}

fn write_threads_snapshot(
    path: &Path,
    threads: &HashMap<String, Vec<ClaudeThreadRecord>>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let data = serde_json::to_string_pretty(threads).map_err(|error| error.to_string())?;
    std::fs::write(path, data).map_err(|error| error.to_string())
}

fn claude_archived_threads_path(claude_threads_path: &Path) -> PathBuf {
    claude_threads_path.with_file_name(CLAUDE_ARCHIVED_THREADS_FILE_NAME)
}

fn read_archived_threads_snapshot(path: &Path) -> Result<HashMap<String, Vec<String>>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let data = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&data).map_err(|error| error.to_string())
}

fn write_archived_threads_snapshot(
    path: &Path,
    snapshot: &HashMap<String, Vec<String>>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let data = serde_json::to_string_pretty(snapshot).map_err(|error| error.to_string())?;
    std::fs::write(path, data).map_err(|error| error.to_string())
}

fn archived_id_variants(thread_id: &str) -> Vec<String> {
    let trimmed = thread_id.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Some(suffix) = trimmed.strip_prefix("claude-thread-") {
        return vec![trimmed.to_string(), suffix.to_string()];
    }
    vec![
        trimmed.to_string(),
        format!("claude-thread-{trimmed}"),
    ]
}

fn is_archived_thread_id(archived_ids: &HashSet<String>, thread_id: &str) -> bool {
    archived_id_variants(thread_id)
        .into_iter()
        .any(|id| archived_ids.contains(&id))
}

fn read_archived_thread_ids_for_workspace(
    claude_threads_path: &Path,
    workspace_id: &str,
) -> HashSet<String> {
    let archived_path = claude_archived_threads_path(claude_threads_path);
    let snapshot = read_archived_threads_snapshot(&archived_path).unwrap_or_default();
    snapshot
        .get(workspace_id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn persist_archived_thread_id_for_workspace(
    claude_threads_path: &Path,
    workspace_id: &str,
    thread_id: &str,
) -> Result<(), String> {
    let archived_path = claude_archived_threads_path(claude_threads_path);
    let mut snapshot = read_archived_threads_snapshot(&archived_path)?;
    let entry = snapshot.entry(workspace_id.to_string()).or_default();
    let mut merged: HashSet<String> = entry.iter().cloned().collect();
    for id in archived_id_variants(thread_id) {
        merged.insert(id);
    }
    let mut values = merged.into_iter().collect::<Vec<_>>();
    values.sort();
    *entry = values;
    write_archived_threads_snapshot(&archived_path, &snapshot)
}

async fn persist_threads_store(
    claude_threads: &ClaudeThreadsStore,
    path: &Path,
) -> Result<(), String> {
    let snapshot = claude_threads.lock().await.clone();
    write_threads_snapshot(path, &snapshot)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn cancel_key(workspace_id: &str, thread_id: &str) -> String {
    format!("{workspace_id}:{thread_id}")
}

fn parse_cli_args(raw: Option<&str>) -> Result<Vec<String>, String> {
    let raw = match raw {
        Some(value) if !value.trim().is_empty() => value.trim(),
        _ => return Ok(Vec::new()),
    };
    shell_words::split(raw)
        .map_err(|error| format!("Invalid Claude args: {error}"))
        .map(|args| args.into_iter().filter(|arg| !arg.is_empty()).collect())
}

fn strip_ansi_sequences(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                while let Some(value) = chars.next() {
                    if ('@'..='~').contains(&value) {
                        break;
                    }
                }
            }
            continue;
        }
        output.push(ch);
    }
    output
}

fn is_server_token(value: &str) -> bool {
    let token = value.trim();
    !token.is_empty()
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn is_jsonrpc_payload(value: &Value) -> bool {
    let has_jsonrpc_shape = value.get("method").and_then(Value::as_str).is_some()
        || value.get("result").is_some()
        || value.get("error").is_some();
    let has_request_shape = value.get("id").is_some() || value.get("params").is_some();
    has_jsonrpc_shape && has_request_shape
}

fn is_debug_jsonrpc_line(line: &str) -> bool {
    let cleaned = strip_ansi_sequences(line);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return false;
    }

    let brace_index = match trimmed.find('{') {
        Some(index) => index,
        None => return false,
    };
    let (prefix, json_part) = trimmed.split_at(brace_index);
    if !is_server_token(prefix) {
        return false;
    }

    let value: Value = match serde_json::from_str(json_part) {
        Ok(value) => value,
        Err(_) => return false,
    };
    is_jsonrpc_payload(&value)
}

fn is_debug_jsonrpc_message(message: &str) -> bool {
    if is_debug_jsonrpc_line(message) {
        return true;
    }

    let cleaned = strip_ansi_sequences(message);
    let mut lines = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    if !is_server_token(first) {
        return false;
    }
    let Some(second) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    if !second.starts_with('{') {
        return false;
    }
    let value: Value = match serde_json::from_str(second) {
        Ok(value) => value,
        Err(_) => return false,
    };
    is_jsonrpc_payload(&value)
}

fn legacy_prefixed_session_id(thread_id: &str) -> Option<String> {
    let suffix = thread_id.strip_prefix("claude-thread-")?;
    if Uuid::parse_str(suffix).is_ok() {
        Some(suffix.to_string())
    } else {
        None
    }
}

fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.timestamp_millis())
}

fn preview_from_text(text: &str) -> String {
    let single_line = text.trim().replace('\n', " ");
    if single_line.len() <= 120 {
        return single_line;
    }
    format!("{}...", &single_line[..117])
}

fn thread_summary(thread: &ClaudeThreadRecord) -> Value {
    json!({
        "id": thread.id,
        "cwd": thread.cwd,
        "preview": thread.preview,
        "createdAt": thread.created_at,
        "updatedAt": thread.updated_at,
        "name": thread.name,
    })
}

fn thread_resume_payload(thread: &ClaudeThreadRecord) -> Value {
    let turns = thread
        .turns
        .iter()
        .map(|turn| {
            let items = turn
                .items
                .iter()
                .map(|item| {
                    if item.role == "user" {
                        json!({
                            "id": item.id,
                            "type": "userMessage",
                            "content": [{ "type": "text", "text": item.text }],
                        })
                    } else {
                        json!({
                            "id": item.id,
                            "type": "agentMessage",
                            "text": item.text,
                        })
                    }
                })
                .collect::<Vec<_>>();
            json!({
                "id": turn.id,
                "startedAt": turn.started_at,
                "completedAt": turn.completed_at,
                "items": items,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id": thread.id,
        "cwd": thread.cwd,
        "preview": thread.preview,
        "createdAt": thread.created_at,
        "updatedAt": thread.updated_at,
        "name": thread.name,
        "turns": turns,
    })
}

fn emit<E: EventSink>(event_sink: &E, workspace_id: &str, method: &str, params: Value) {
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: workspace_id.to_string(),
        message: json!({
            "method": method,
            "params": params,
        }),
    });
}

fn encode_workspace_for_claude_projects(workspace_path: &str) -> Option<String> {
    let mut encoded = String::new();
    let mut last_dash = false;
    for ch in workspace_path.chars() {
        if ch.is_ascii_alphanumeric() {
            encoded.push(ch);
            last_dash = false;
        } else if !last_dash {
            encoded.push('-');
            last_dash = true;
        }
    }
    let encoded = encoded.trim_end_matches('-').to_string();
    if encoded.is_empty() {
        None
    } else {
        Some(encoded)
    }
}

fn claude_project_dir_for_workspace(workspace_path: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let encoded = encode_workspace_for_claude_projects(workspace_path)?;
    Some(PathBuf::from(home).join(CLAUDE_HISTORY_ROOT).join(encoded))
}

fn extract_text_from_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Array(entries) => entries.iter().find_map(|entry| {
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            if let Some(text) = entry.as_str() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            None
        }),
        _ => None,
    }
}

fn extract_message_text(record: &Value) -> Option<String> {
    let message = record.get("message")?;
    let content = message.get("content")?;
    extract_text_from_content(content)
}

#[derive(Debug, Clone)]
struct HistoryMessage {
    role: String,
    text: String,
    timestamp_ms: i64,
}

fn flush_history_turn(
    turns: &mut Vec<ClaudeTurnRecord>,
    thread_id: &str,
    turn_index: usize,
    pending_user: Option<(String, i64)>,
    pending_assistant: Option<(String, i64)>,
) {
    if pending_user.is_none() && pending_assistant.is_none() {
        return;
    }

    let started_at = pending_user
        .as_ref()
        .map(|(_, timestamp)| *timestamp)
        .or_else(|| pending_assistant.as_ref().map(|(_, timestamp)| *timestamp))
        .unwrap_or_else(now_ms);
    let completed_at = pending_assistant
        .as_ref()
        .map(|(_, timestamp)| (*timestamp).max(started_at));

    let mut items = Vec::new();
    if let Some((text, _)) = pending_user {
        items.push(ClaudeMessageRecord {
            id: format!("claude-history-user-{thread_id}-{turn_index}"),
            role: "user".to_string(),
            text,
        });
    }
    if let Some((text, _)) = pending_assistant {
        items.push(ClaudeMessageRecord {
            id: format!("claude-history-assistant-{thread_id}-{turn_index}"),
            role: "assistant".to_string(),
            text,
        });
    }
    if items.is_empty() {
        return;
    }

    turns.push(ClaudeTurnRecord {
        id: format!("claude-history-turn-{thread_id}-{turn_index}"),
        started_at,
        completed_at,
        items,
    });
}

fn build_turns_from_history_messages(
    thread_id: &str,
    history_messages: &[HistoryMessage],
) -> Vec<ClaudeTurnRecord> {
    let mut turns = Vec::new();
    let mut pending_user: Option<(String, i64)> = None;
    let mut pending_assistant: Option<(String, i64)> = None;
    let mut turn_index = 0usize;

    for message in history_messages {
        match message.role.as_str() {
            "user" => {
                if pending_user.is_some() || pending_assistant.is_some() {
                    flush_history_turn(
                        &mut turns,
                        thread_id,
                        turn_index,
                        pending_user.take(),
                        pending_assistant.take(),
                    );
                    turn_index += 1;
                }
                pending_user = Some((message.text.clone(), message.timestamp_ms));
            }
            "assistant" => {
                pending_assistant = Some((message.text.clone(), message.timestamp_ms));
            }
            _ => {}
        }
    }

    if pending_user.is_some() || pending_assistant.is_some() {
        flush_history_turn(
            &mut turns,
            thread_id,
            turn_index,
            pending_user.take(),
            pending_assistant.take(),
        );
    }

    if turns.len() > MAX_IMPORTED_TURNS_PER_THREAD {
        let start = turns.len() - MAX_IMPORTED_TURNS_PER_THREAD;
        return turns[start..].to_vec();
    }
    turns
}

fn parse_claude_history_thread_file(
    path: &Path,
    fallback_workspace_path: &str,
) -> Option<ClaudeThreadRecord> {
    let file = File::open(path).ok()?;
    let metadata = file.metadata().ok();
    let mut reader = StdBufReader::new(file);

    let mut line = String::new();
    let thread_id = path.file_stem()?.to_string_lossy().to_string();
    let mut cwd = fallback_workspace_path.to_string();
    let mut created_at: Option<i64> = None;
    let mut updated_at: Option<i64> = None;
    let mut first_user_text: Option<String> = None;
    let mut last_assistant_text: Option<String> = None;
    let mut history_messages: Vec<HistoryMessage> = Vec::new();
    let mut saw_user_message = false;
    let mut fallback_timestamp_counter = 0i64;

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).ok()?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_str(trimmed) {
            Ok(record) => record,
            Err(_) => continue,
        };

        if let Some(session_id) = record.get("sessionId").and_then(Value::as_str) {
            let session_id = session_id.trim();
            if session_id.is_empty() || session_id != thread_id {
                continue;
            }
        }
        if let Some(record_cwd) = record.get("cwd").and_then(Value::as_str) {
            if !record_cwd.trim().is_empty() {
                cwd = record_cwd.to_string();
            }
        }
        let parsed_timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms);
        if let Some(timestamp_ms) = parsed_timestamp {
            created_at = Some(created_at.map_or(timestamp_ms, |value| value.min(timestamp_ms)));
            updated_at = Some(updated_at.map_or(timestamp_ms, |value| value.max(timestamp_ms)));
        }

        let effective_timestamp = parsed_timestamp.unwrap_or_else(|| {
            fallback_timestamp_counter += 1;
            updated_at
                .unwrap_or_else(now_ms)
                .saturating_add(fallback_timestamp_counter)
        });

        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = extract_message_text(&record) {
                    if is_debug_jsonrpc_message(&text) {
                        continue;
                    }
                    if first_user_text.is_none() {
                        first_user_text = Some(text.clone());
                    }
                    saw_user_message = true;
                    history_messages.push(HistoryMessage {
                        role: "user".to_string(),
                        text,
                        timestamp_ms: effective_timestamp,
                    });
                }
            }
            Some("assistant") => {
                if let Some(text) = extract_message_text(&record) {
                    if is_debug_jsonrpc_message(&text) {
                        continue;
                    }
                    if !saw_user_message {
                        continue;
                    }
                    last_assistant_text = Some(text.clone());
                    history_messages.push(HistoryMessage {
                        role: "assistant".to_string(),
                        text,
                        timestamp_ms: effective_timestamp,
                    });
                }
            }
            _ => {}
        }
    }

    let fallback_timestamp = metadata
        .and_then(|entry| entry.modified().ok())
        .and_then(|modified| {
            modified
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis() as i64)
        })
        .unwrap_or_else(now_ms);
    let created_at = created_at.unwrap_or(fallback_timestamp);
    let updated_at = updated_at.unwrap_or(created_at.max(fallback_timestamp));
    let preview_source = first_user_text
        .or(last_assistant_text)
        .unwrap_or_else(|| thread_id.clone());
    let turns = build_turns_from_history_messages(&thread_id, &history_messages);
    if turns.is_empty() {
        return None;
    }

    Some(ClaudeThreadRecord {
        id: thread_id,
        cwd,
        preview: preview_from_text(&preview_source),
        created_at,
        updated_at,
        name: None,
        turns,
    })
}

fn scan_claude_history_threads(workspace_path: &str) -> Vec<ClaudeThreadRecord> {
    let project_dir = match claude_project_dir_for_workspace(workspace_path) {
        Some(path) => path,
        None => return Vec::new(),
    };
    if !project_dir.exists() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(project_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut by_id: HashMap<String, ClaudeThreadRecord> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("jsonl")
        ) {
            continue;
        }
        let Some(thread) = parse_claude_history_thread_file(&path, workspace_path) else {
            continue;
        };
        let should_replace = by_id
            .get(&thread.id)
            .map(|existing| existing.updated_at < thread.updated_at)
            .unwrap_or(true);
        if should_replace {
            by_id.insert(thread.id.clone(), thread);
        }
    }

    let mut threads = by_id.into_values().collect::<Vec<_>>();
    threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    threads
}

async fn import_history_threads_for_workspace(
    claude_threads: &ClaudeThreadsStore,
    claude_threads_path: &Path,
    workspace_id: &str,
    workspace_path: &str,
) -> Result<bool, String> {
    let archived_ids = read_archived_thread_ids_for_workspace(claude_threads_path, workspace_id);
    let workspace_path = workspace_path.to_string();
    let workspace_path_for_scan = workspace_path.clone();
    let imported =
        tokio::task::spawn_blocking(move || scan_claude_history_threads(&workspace_path_for_scan))
            .await
            .map_err(|error| format!("failed to scan Claude history: {error}"))?;
    if imported.is_empty() {
        return Ok(false);
    }

    let mut changed = false;
    {
        let mut store = claude_threads.lock().await;
        let threads = store.entry(workspace_id.to_string()).or_default();
        for mut imported_thread in imported {
            if is_archived_thread_id(&archived_ids, &imported_thread.id) {
                continue;
            }
            imported_thread.cwd = workspace_path.clone();
            let legacy_id = format!("claude-thread-{}", imported_thread.id);
            if let Some(existing) = threads
                .iter_mut()
                .find(|thread| thread.id == imported_thread.id || thread.id == legacy_id)
            {
                let mut updated = false;
                if existing.created_at <= 0 && imported_thread.created_at > 0 {
                    existing.created_at = imported_thread.created_at;
                    updated = true;
                }
                if imported_thread.updated_at > existing.updated_at {
                    existing.updated_at = imported_thread.updated_at;
                    if existing.preview.trim().is_empty() {
                        existing.preview = imported_thread.preview.clone();
                    }
                    if !imported_thread.turns.is_empty() {
                        existing.turns = imported_thread.turns.clone();
                    }
                    updated = true;
                }
                if existing.cwd != workspace_path {
                    existing.cwd = workspace_path.clone();
                    updated = true;
                }
                if existing.turns.is_empty() && !imported_thread.turns.is_empty() {
                    existing.turns = imported_thread.turns.clone();
                    updated = true;
                }
                if updated {
                    changed = true;
                }
                continue;
            }
            threads.push(imported_thread);
            changed = true;
        }
        if changed {
            threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }
    }

    if changed {
        persist_threads_store(claude_threads, claude_threads_path).await?;
    }
    Ok(changed)
}

async fn prune_placeholder_threads_for_workspace(
    claude_threads: &ClaudeThreadsStore,
    claude_threads_path: &Path,
    workspace_id: &str,
) -> Result<bool, String> {
    let archived_ids = read_archived_thread_ids_for_workspace(claude_threads_path, workspace_id);
    let mut changed = false;
    {
        let mut store = claude_threads.lock().await;
        let Some(threads) = store.get_mut(workspace_id) else {
            return Ok(false);
        };
        let before = threads.len();
        threads.retain(|thread| {
            let mut has_any_user_message = false;
            let mut has_non_debug_user_message = false;
            for turn in &thread.turns {
                for item in &turn.items {
                    if item.role != "user" {
                        continue;
                    }
                    has_any_user_message = true;
                    if !is_debug_jsonrpc_message(&item.text) {
                        has_non_debug_user_message = true;
                        break;
                    }
                }
                if has_non_debug_user_message {
                    break;
                }
            }

            let looks_like_import_placeholder = thread.turns.is_empty()
                && thread.name.is_none()
                && thread.preview.trim() == thread.id;
            let looks_like_debug_bootstrap_thread =
                has_any_user_message && !has_non_debug_user_message;
            let is_archived = is_archived_thread_id(&archived_ids, &thread.id);
            !(looks_like_import_placeholder || looks_like_debug_bootstrap_thread || is_archived)
        });
        if threads.len() != before {
            changed = true;
            threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }
    }
    if changed {
        persist_threads_store(claude_threads, claude_threads_path).await?;
    }
    Ok(changed)
}

fn resolve_parent_entry(
    workspaces: &HashMap<String, WorkspaceEntry>,
    entry: &WorkspaceEntry,
) -> Option<WorkspaceEntry> {
    entry
        .parent_id
        .as_ref()
        .and_then(|parent_id| workspaces.get(parent_id))
        .cloned()
}

async fn resolve_workspace_context(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    app_settings: &Mutex<AppSettings>,
    workspace_id: &str,
) -> Result<(WorkspaceEntry, Option<WorkspaceEntry>, AppSettings), String> {
    let entry_and_parent = {
        let workspaces = workspaces.lock().await;
        let entry = workspaces
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| "workspace not found".to_string())?;
        let parent_entry = resolve_parent_entry(&workspaces, &entry);
        (entry, parent_entry)
    };
    let settings = app_settings.lock().await.clone();
    Ok((entry_and_parent.0, entry_and_parent.1, settings))
}

async fn ensure_workspace_provider_is_claude(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    app_settings: &Mutex<AppSettings>,
    workspace_id: &str,
) -> Result<(WorkspaceEntry, Option<WorkspaceEntry>, AppSettings), String> {
    let (entry, parent_entry, settings) =
        resolve_workspace_context(workspaces, app_settings, workspace_id).await?;
    let provider = providers::resolve_workspace_provider(&entry, Some(&settings));
    if !matches!(provider, ProviderKind::Claude) {
        return Err(format!(
            "workspace `{}` is configured for provider `{}`",
            workspace_id,
            provider.as_str()
        ));
    }
    Ok((entry, parent_entry, settings))
}

pub(crate) async fn start_thread_core<E: EventSink>(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    app_settings: &Mutex<AppSettings>,
    claude_threads: &ClaudeThreadsStore,
    claude_threads_path: &Path,
    workspace_id: String,
    event_sink: E,
) -> Result<Value, String> {
    let (entry, _parent_entry, _settings) =
        ensure_workspace_provider_is_claude(workspaces, app_settings, &workspace_id).await?;
    let timestamp = now_ms();
    let thread = ClaudeThreadRecord {
        id: Uuid::new_v4().to_string(),
        cwd: entry.path.clone(),
        preview: String::new(),
        created_at: timestamp,
        updated_at: timestamp,
        name: None,
        turns: Vec::new(),
    };
    {
        let mut store = claude_threads.lock().await;
        let threads = store.entry(workspace_id.clone()).or_default();
        threads.insert(0, thread.clone());
    }
    persist_threads_store(claude_threads, claude_threads_path).await?;
    emit(
        &event_sink,
        &workspace_id,
        "thread/started",
        json!({
            "thread": thread_summary(&thread),
        }),
    );
    Ok(json!({
        "result": {
            "thread": thread_summary(&thread),
        }
    }))
}

pub(crate) async fn resume_thread_core(
    claude_threads: &ClaudeThreadsStore,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    let store = claude_threads.lock().await;
    let threads = store
        .get(&workspace_id)
        .ok_or_else(|| "thread not found".to_string())?;
    let thread = threads
        .iter()
        .find(|thread| thread.id == thread_id)
        .ok_or_else(|| "thread not found".to_string())?;
    Ok(json!({
        "result": {
            "thread": thread_resume_payload(thread),
        }
    }))
}

pub(crate) async fn list_threads_core(
    claude_threads: &ClaudeThreadsStore,
    claude_threads_path: &Path,
    workspace_id: String,
    workspace_path: String,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<Value, String> {
    let _ = import_history_threads_for_workspace(
        claude_threads,
        claude_threads_path,
        &workspace_id,
        &workspace_path,
    )
    .await;
    let _ =
        prune_placeholder_threads_for_workspace(claude_threads, claude_threads_path, &workspace_id)
            .await;

    let offset = cursor
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = limit.unwrap_or(20).max(1).min(100) as usize;

    let mut threads = {
        let store = claude_threads.lock().await;
        store.get(&workspace_id).cloned().unwrap_or_default()
    };
    threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let data = threads
        .iter()
        .skip(offset)
        .take(limit)
        .map(thread_summary)
        .collect::<Vec<_>>();
    let next_offset = offset + data.len();
    let next_cursor = if next_offset < threads.len() {
        Some(next_offset.to_string())
    } else {
        None
    };
    Ok(json!({
        "result": {
            "data": data,
            "nextCursor": next_cursor,
        }
    }))
}

fn build_prompt(text: &str, images: Option<Vec<String>>) -> String {
    let mut prompt = text.trim().to_string();
    let image_lines = images
        .unwrap_or_default()
        .into_iter()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    if !image_lines.is_empty() {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str("Attached image paths:\n");
        for path in image_lines {
            prompt.push_str("- ");
            prompt.push_str(&path);
            prompt.push('\n');
        }
    }
    prompt
}

fn prepare_command(bin: Option<String>, args: Option<String>, cwd: &PathBuf) -> Result<tokio::process::Command, String> {
    let executable = bin
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "claude".to_string());
    let mut command = tokio_command(executable);
    command.current_dir(cwd);
    let parsed = parse_cli_args(args.as_deref())?;
    if !parsed.is_empty() {
        command.args(parsed);
    }
    Ok(command)
}

async fn finalize_turn(
    claude_threads: &ClaudeThreadsStore,
    workspace_id: &str,
    thread_id: &str,
    turn_id: &str,
    assistant_item_id: &str,
    assistant_text: &str,
) {
    let mut store = claude_threads.lock().await;
    let threads = match store.get_mut(workspace_id) {
        Some(threads) => threads,
        None => return,
    };
    let thread = match threads.iter_mut().find(|thread| thread.id == thread_id) {
        Some(thread) => thread,
        None => return,
    };
    let updated_at = now_ms();
    thread.updated_at = updated_at;
    if !assistant_text.trim().is_empty() {
        thread.preview = preview_from_text(assistant_text);
    }
    if let Some(turn) = thread.turns.iter_mut().find(|turn| turn.id == turn_id) {
        turn.completed_at = Some(updated_at);
        if let Some(item) = turn
            .items
            .iter_mut()
            .find(|item| item.id == assistant_item_id && item.role == "assistant")
        {
            item.text = assistant_text.to_string();
        }
    }
}

pub(crate) async fn send_user_message_core<E: EventSink>(
    workspaces: &Mutex<HashMap<String, WorkspaceEntry>>,
    app_settings: &Mutex<AppSettings>,
    claude_threads: &ClaudeThreadsStore,
    claude_turn_cancels: &ClaudeTurnCancelsStore,
    claude_threads_path: &Path,
    workspace_id: String,
    thread_id: String,
    text: String,
    images: Option<Vec<String>>,
    event_sink: E,
) -> Result<Value, String> {
    if text.trim().is_empty() && images.as_ref().map(|items| items.is_empty()).unwrap_or(true) {
        return Err("empty user message".to_string());
    }

    let (entry, parent_entry, settings) =
        resolve_workspace_context(workspaces, app_settings, &workspace_id).await?;
    let (claude_bin, claude_args) =
        providers::resolve_claude_runtime_config(&entry, parent_entry.as_ref(), Some(&settings));
    let prompt = build_prompt(&text, images);

    let turn_id = format!("claude-turn-{}", Uuid::new_v4());
    let user_item_id = format!("claude-user-{}", Uuid::new_v4());
    let assistant_item_id = format!("claude-assistant-{}", Uuid::new_v4());
    let started_at = now_ms();
    let thread_has_turns = {
        let mut store = claude_threads.lock().await;
        let threads = store
            .get_mut(&workspace_id)
            .ok_or_else(|| "thread not found".to_string())?;
        let thread = threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .ok_or_else(|| "thread not found".to_string())?;
        let had_turns = !thread.turns.is_empty();
        thread.updated_at = started_at;
        thread.turns.push(ClaudeTurnRecord {
            id: turn_id.clone(),
            started_at,
            completed_at: None,
            items: vec![
                ClaudeMessageRecord {
                    id: user_item_id.clone(),
                    role: "user".to_string(),
                    text: text.clone(),
                },
                ClaudeMessageRecord {
                    id: assistant_item_id.clone(),
                    role: "assistant".to_string(),
                    text: String::new(),
                },
            ],
        });
        had_turns
    };
    persist_threads_store(claude_threads, claude_threads_path).await?;

    emit(
        &event_sink,
        &workspace_id,
        "turn/started",
        json!({
            "threadId": thread_id,
            "turn": { "id": turn_id, "threadId": thread_id },
        }),
    );
    emit(
        &event_sink,
        &workspace_id,
        "item/started",
        json!({
            "threadId": thread_id,
            "item": {
                "id": user_item_id,
                "type": "userMessage",
                "content": [{ "type": "text", "text": text }],
            },
        }),
    );
    emit(
        &event_sink,
        &workspace_id,
        "item/completed",
        json!({
            "threadId": thread_id,
            "item": {
                "id": user_item_id,
                "type": "userMessage",
                "content": [{ "type": "text", "text": text }],
            },
        }),
    );
    emit(
        &event_sink,
        &workspace_id,
        "item/started",
        json!({
            "threadId": thread_id,
            "item": {
                "id": assistant_item_id,
                "type": "agentMessage",
                "text": "",
            },
        }),
    );

    let key = cancel_key(&workspace_id, &thread_id);
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    {
        let mut cancels = claude_turn_cancels.lock().await;
        if let Some(existing) = cancels.remove(&key) {
            let _ = existing.send(());
        }
        cancels.insert(key.clone(), cancel_tx);
    }

    let workspace_id_for_task = workspace_id.clone();
    let thread_id_for_task = thread_id.clone();
    let turn_id_for_task = turn_id.clone();
    let assistant_item_id_for_task = assistant_item_id.clone();
    let explicit_session_id = if let Some(legacy) = legacy_prefixed_session_id(&thread_id) {
        Some(legacy)
    } else if !thread_has_turns && Uuid::parse_str(&thread_id).is_ok() {
        Some(thread_id.clone())
    } else {
        None
    };
    let resume_session_id = if explicit_session_id.is_none() && !thread_id.trim().is_empty() {
        Some(thread_id.clone())
    } else {
        None
    };
    let cwd = PathBuf::from(entry.path.clone());
    let claude_threads_clone = Arc::clone(claude_threads);
    let claude_turn_cancels_clone = Arc::clone(claude_turn_cancels);
    let claude_threads_path = claude_threads_path.to_path_buf();
    let event_sink_clone = event_sink.clone();

    tokio::spawn(async move {
        let mut aggregated = String::new();
        let mut command = match prepare_command(claude_bin, claude_args, &cwd) {
            Ok(command) => command,
            Err(error) => {
                emit(
                    &event_sink_clone,
                    &workspace_id_for_task,
                    "error",
                    json!({
                        "threadId": thread_id_for_task,
                        "turnId": turn_id_for_task,
                        "error": { "message": error },
                        "willRetry": false,
                    }),
                );
                let mut cancels = claude_turn_cancels_clone.lock().await;
                cancels.remove(&key);
                return;
            }
        };
        command.arg("-p").arg(prompt);
        // Force plain text output so UI rendering doesn't ingest structured/debug streams.
        command.arg("--output-format").arg("text");
        if let Some(session_id) = &explicit_session_id {
            command.arg("--session-id").arg(session_id);
        } else if let Some(session_id) = &resume_session_id {
            command.arg("--resume").arg(session_id);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("Failed to start Claude CLI: {error}");
                emit(
                    &event_sink_clone,
                    &workspace_id_for_task,
                    "error",
                    json!({
                        "threadId": thread_id_for_task,
                        "turnId": turn_id_for_task,
                        "error": { "message": message },
                        "willRetry": false,
                    }),
                );
                let mut cancels = claude_turn_cancels_clone.lock().await;
                cancels.remove(&key);
                return;
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let message = "Claude CLI missing stdout".to_string();
                emit(
                    &event_sink_clone,
                    &workspace_id_for_task,
                    "error",
                    json!({
                        "threadId": thread_id_for_task,
                        "turnId": turn_id_for_task,
                        "error": { "message": message },
                        "willRetry": false,
                    }),
                );
                let mut cancels = claude_turn_cancels_clone.lock().await;
                cancels.remove(&key);
                return;
            }
        };
        let stderr = child.stderr.take();
        let stderr_handle = tokio::spawn(async move {
            let mut output = String::new();
            if let Some(stderr) = stderr {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&line);
                }
            }
            output
        });

        let mut lines = BufReader::new(stdout).lines();
        let mut pending_server_token: Option<String> = None;
        let mut canceled = false;
        let mut read_error: Option<String> = None;
        loop {
            match cancel_rx.try_recv() {
                Ok(_) | Err(TryRecvError::Closed) => {
                    canceled = true;
                    let _ = child.kill().await;
                    break;
                }
                Err(TryRecvError::Empty) => {}
            }

            match timeout(Duration::from_millis(120), lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let normalized_line = strip_ansi_sequences(&line).trim().to_string();
                    if normalized_line.is_empty() {
                        continue;
                    }

                    if let Some(server_token) = pending_server_token.take() {
                        let candidate = format!("{server_token}\n{normalized_line}");
                        if !is_debug_jsonrpc_message(&candidate) {
                            let pending_delta = if aggregated.is_empty() {
                                server_token
                            } else {
                                format!("\n{server_token}")
                            };
                            aggregated.push_str(&pending_delta);
                            emit(
                                &event_sink_clone,
                                &workspace_id_for_task,
                                "item/agentMessage/delta",
                                json!({
                                    "threadId": thread_id_for_task,
                                    "itemId": assistant_item_id_for_task,
                                    "delta": pending_delta,
                                }),
                            );
                        } else {
                            continue;
                        }
                    }

                    if is_server_token(&normalized_line) {
                        pending_server_token = Some(normalized_line);
                        continue;
                    }

                    if is_debug_jsonrpc_line(&normalized_line) {
                        continue;
                    }

                    let delta = if aggregated.is_empty() {
                        normalized_line
                    } else {
                        format!("\n{normalized_line}")
                    };
                    aggregated.push_str(&delta);
                    emit(
                        &event_sink_clone,
                        &workspace_id_for_task,
                        "item/agentMessage/delta",
                        json!({
                            "threadId": thread_id_for_task,
                            "itemId": assistant_item_id_for_task,
                            "delta": delta,
                        }),
                    );
                }
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    read_error = Some(format!("Failed reading Claude output: {error}"));
                    break;
                }
                Err(_) => continue,
            }
        }
        if let Some(server_token) = pending_server_token.take() {
            let delta = if aggregated.is_empty() {
                server_token
            } else {
                format!("\n{server_token}")
            };
            aggregated.push_str(&delta);
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "item/agentMessage/delta",
                json!({
                    "threadId": thread_id_for_task,
                    "itemId": assistant_item_id_for_task,
                    "delta": delta,
                }),
            );
        }

        let status = child.wait().await.ok();
        let stderr_output = stderr_handle.await.unwrap_or_default();
        finalize_turn(
            &claude_threads_clone,
            &workspace_id_for_task,
            &thread_id_for_task,
            &turn_id_for_task,
            &assistant_item_id_for_task,
            &aggregated,
        )
        .await;
        let _ = persist_threads_store(&claude_threads_clone, &claude_threads_path).await;

        if canceled {
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "item/completed",
                json!({
                    "threadId": thread_id_for_task,
                    "item": {
                        "id": assistant_item_id_for_task,
                        "type": "agentMessage",
                        "text": aggregated,
                    },
                }),
            );
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "turn/completed",
                json!({
                    "threadId": thread_id_for_task,
                    "turn": { "id": turn_id_for_task, "threadId": thread_id_for_task },
                }),
            );
            let mut cancels = claude_turn_cancels_clone.lock().await;
            cancels.remove(&key);
            return;
        }

        if let Some(error) = read_error {
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "error",
                json!({
                    "threadId": thread_id_for_task,
                    "turnId": turn_id_for_task,
                    "error": { "message": error },
                    "willRetry": false,
                }),
            );
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "turn/completed",
                json!({
                    "threadId": thread_id_for_task,
                    "turn": { "id": turn_id_for_task, "threadId": thread_id_for_task },
                }),
            );
            let mut cancels = claude_turn_cancels_clone.lock().await;
            cancels.remove(&key);
            return;
        }

        let success = status.map(|value| value.success()).unwrap_or(false);
        if success {
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "item/completed",
                json!({
                    "threadId": thread_id_for_task,
                    "item": {
                        "id": assistant_item_id_for_task,
                        "type": "agentMessage",
                        "text": aggregated,
                    },
                }),
            );
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "turn/completed",
                json!({
                    "threadId": thread_id_for_task,
                    "turn": { "id": turn_id_for_task, "threadId": thread_id_for_task },
                }),
            );
        } else {
            let message = if !stderr_output.trim().is_empty() {
                stderr_output
            } else {
                "Claude CLI failed.".to_string()
            };
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "error",
                json!({
                    "threadId": thread_id_for_task,
                    "turnId": turn_id_for_task,
                    "error": { "message": message },
                    "willRetry": false,
                }),
            );
            emit(
                &event_sink_clone,
                &workspace_id_for_task,
                "turn/completed",
                json!({
                    "threadId": thread_id_for_task,
                    "turn": { "id": turn_id_for_task, "threadId": thread_id_for_task },
                }),
            );
        }

        let mut cancels = claude_turn_cancels_clone.lock().await;
        cancels.remove(&key);
    });

    Ok(json!({
        "result": {
            "turn": { "id": turn_id, "threadId": thread_id }
        }
    }))
}

pub(crate) async fn turn_interrupt_core(
    claude_turn_cancels: &ClaudeTurnCancelsStore,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    let key = cancel_key(&workspace_id, &thread_id);
    let cancel = {
        let mut cancels = claude_turn_cancels.lock().await;
        cancels.remove(&key)
    };
    if let Some(cancel) = cancel {
        let _ = cancel.send(());
    }
    Ok(json!({ "result": { "ok": true } }))
}

pub(crate) async fn archive_thread_core(
    claude_threads: &ClaudeThreadsStore,
    claude_threads_path: &Path,
    workspace_id: String,
    thread_id: String,
) -> Result<Value, String> {
    persist_archived_thread_id_for_workspace(claude_threads_path, &workspace_id, &thread_id)?;
    let mut store = claude_threads.lock().await;
    if let Some(threads) = store.get_mut(&workspace_id) {
        threads.retain(|thread| thread.id != thread_id);
    }
    drop(store);
    persist_threads_store(claude_threads, claude_threads_path).await?;
    Ok(json!({ "result": { "ok": true } }))
}

pub(crate) async fn set_thread_name_core(
    claude_threads: &ClaudeThreadsStore,
    claude_threads_path: &Path,
    workspace_id: String,
    thread_id: String,
    name: String,
) -> Result<Value, String> {
    let mut store = claude_threads.lock().await;
    let threads = store
        .get_mut(&workspace_id)
        .ok_or_else(|| "thread not found".to_string())?;
    let thread = threads
        .iter_mut()
        .find(|thread| thread.id == thread_id)
        .ok_or_else(|| "thread not found".to_string())?;
    let trimmed = name.trim().to_string();
    thread.name = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.clone())
    };
    thread.updated_at = now_ms();
    let thread_name = thread.name.clone();
    drop(store);
    persist_threads_store(claude_threads, claude_threads_path).await?;
    Ok(json!({
        "result": {
            "threadId": thread_id,
            "threadName": thread_name,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{is_debug_jsonrpc_line, is_debug_jsonrpc_message};

    #[test]
    fn detects_prefixed_jsonrpc_debug_line() {
        let line = r#"app-server {"id":1,"method":"initialize","params":{"foo":"bar"}}"#;
        assert!(is_debug_jsonrpc_line(line));
    }

    #[test]
    fn ignores_plain_assistant_text() {
        let line = "Here is the answer to your question.";
        assert!(!is_debug_jsonrpc_line(line));
    }

    #[test]
    fn ignores_json_without_prefixed_server_token() {
        let line = r#"{"id":1,"method":"initialize","params":{"foo":"bar"}}"#;
        assert!(!is_debug_jsonrpc_line(line));
    }

    #[test]
    fn detects_multiline_jsonrpc_debug_message() {
        let message = "app-server\n{\"id\":1,\"method\":\"initialize\",\"params\":{\"foo\":\"bar\"}}";
        assert!(is_debug_jsonrpc_message(message));
    }
}
