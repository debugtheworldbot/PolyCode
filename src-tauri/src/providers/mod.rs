use std::path::PathBuf;

use crate::codex::args::resolve_workspace_codex_args;
use crate::codex::home::resolve_workspace_codex_home;
use crate::types::{AppSettings, ProviderKind, WorkspaceEntry};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderCapabilities {
    pub(crate) list_threads: bool,
    pub(crate) resume_thread: bool,
    pub(crate) interrupt_turn: bool,
    pub(crate) model_list: bool,
}

#[allow(dead_code)]
pub(crate) fn capabilities(provider: &ProviderKind) -> ProviderCapabilities {
    match provider {
        ProviderKind::Codex => ProviderCapabilities {
            list_threads: true,
            resume_thread: true,
            interrupt_turn: true,
            model_list: true,
        },
        ProviderKind::Claude => ProviderCapabilities {
            list_threads: true,
            resume_thread: true,
            interrupt_turn: true,
            model_list: false,
        },
        ProviderKind::Gemini => ProviderCapabilities {
            list_threads: false,
            resume_thread: false,
            interrupt_turn: false,
            model_list: false,
        },
    }
}

pub(crate) fn resolve_workspace_provider(
    entry: &WorkspaceEntry,
    app_settings: Option<&AppSettings>,
) -> ProviderKind {
    if let Some(provider) = entry.settings.provider.clone() {
        return provider;
    }
    app_settings
        .and_then(|settings| settings.default_provider.clone())
        .unwrap_or_default()
}

pub(crate) fn resolve_runtime_config(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> (
    ProviderKind,
    Option<String>,
    Option<String>,
    Option<PathBuf>,
) {
    let provider = resolve_workspace_provider(entry, app_settings);
    match provider {
        ProviderKind::Codex => {
            let default_bin = resolve_codex_bin(entry, parent_entry, app_settings);
            let args = resolve_workspace_codex_args(entry, parent_entry, app_settings);
            let home = resolve_workspace_codex_home(entry, parent_entry);
            (provider, default_bin, args, home)
        }
        ProviderKind::Claude => (
            provider,
            resolve_claude_bin(entry, parent_entry, app_settings),
            resolve_claude_args(entry, parent_entry, app_settings),
            None,
        ),
        ProviderKind::Gemini => (
            provider,
            resolve_gemini_bin(entry, parent_entry, app_settings),
            resolve_gemini_args(entry, parent_entry, app_settings),
            None,
        ),
    }
}

pub(crate) fn resolve_claude_runtime_config(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> (Option<String>, Option<String>) {
    (
        resolve_claude_bin(entry, parent_entry, app_settings),
        resolve_claude_args(entry, parent_entry, app_settings),
    )
}

pub(crate) fn ensure_provider_spawn_supported(provider: &ProviderKind) -> Result<(), String> {
    match provider {
        ProviderKind::Codex | ProviderKind::Claude => Ok(()),
        _ => Err(format!(
            "Provider `{}` is not implemented yet. Currently only `codex` and `claude` sessions are supported.",
            provider.as_str()
        )),
    }
}

fn normalize_optional(value: Option<&str>) -> Option<String> {
    match value {
        Some(raw) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        _ => None,
    }
}

fn resolve_codex_bin(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> Option<String> {
    normalize_optional(entry.codex_bin.as_deref())
        .or_else(|| {
            if entry.kind.is_worktree() {
                parent_entry.and_then(|parent| normalize_optional(parent.codex_bin.as_deref()))
            } else {
                None
            }
        })
        .or_else(|| {
            app_settings.and_then(|settings| normalize_optional(settings.codex_bin.as_deref()))
        })
}

fn resolve_claude_bin(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> Option<String> {
    normalize_optional(entry.settings.claude_bin.as_deref())
        .or_else(|| {
            if entry.kind.is_worktree() {
                parent_entry
                    .and_then(|parent| normalize_optional(parent.settings.claude_bin.as_deref()))
            } else {
                None
            }
        })
        .or_else(|| {
            app_settings.and_then(|settings| normalize_optional(settings.claude_bin.as_deref()))
        })
}

fn resolve_claude_args(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> Option<String> {
    normalize_optional(entry.settings.claude_args.as_deref())
        .or_else(|| {
            if entry.kind.is_worktree() {
                parent_entry
                    .and_then(|parent| normalize_optional(parent.settings.claude_args.as_deref()))
            } else {
                None
            }
        })
        .or_else(|| {
            app_settings.and_then(|settings| normalize_optional(settings.claude_args.as_deref()))
        })
}

fn resolve_gemini_bin(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> Option<String> {
    normalize_optional(entry.settings.gemini_bin.as_deref())
        .or_else(|| {
            if entry.kind.is_worktree() {
                parent_entry
                    .and_then(|parent| normalize_optional(parent.settings.gemini_bin.as_deref()))
            } else {
                None
            }
        })
        .or_else(|| {
            app_settings.and_then(|settings| normalize_optional(settings.gemini_bin.as_deref()))
        })
}

fn resolve_gemini_args(
    entry: &WorkspaceEntry,
    parent_entry: Option<&WorkspaceEntry>,
    app_settings: Option<&AppSettings>,
) -> Option<String> {
    normalize_optional(entry.settings.gemini_args.as_deref())
        .or_else(|| {
            if entry.kind.is_worktree() {
                parent_entry
                    .and_then(|parent| normalize_optional(parent.settings.gemini_args.as_deref()))
            } else {
                None
            }
        })
        .or_else(|| {
            app_settings.and_then(|settings| normalize_optional(settings.gemini_args.as_deref()))
        })
}
