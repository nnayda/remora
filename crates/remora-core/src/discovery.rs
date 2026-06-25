//! Transport-agnostic discovery: turn sandbox output (tmux env, git worktree
//! list) into `SessionMeta`. Pure — no `SshHost`, no argv (mirrors
//! `spawn_plan`, so kubectl reuses it). Everything here is untrusted input
//! (ADR-0004): unparseable metadata maps to `None`, forged paths are dropped.

use std::collections::HashSet;

use remora_protocol::{ProjectId, SessionId, SessionMeta, SessionState, WorkspaceMode};

use crate::naming::parse_worktree_path;

/// Upper bound on a discovered metadata string echoed into UI/logs. Forged
/// env can be arbitrarily large; bound it like `InvalidIdError` does.
const MAX_METADATA_LEN: usize = 256;

/// Untrusted, display-only session metadata read from tmux env (ADR-0004).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredEnv {
    pub agent: Option<String>,
    pub created_at: Option<u64>,
    pub workspace_path: Option<String>,
}

/// `Some(value)` if the discovered string is non-empty, within the cap, and
/// free of control bytes; otherwise `None`. The single sanitize-and-bound
/// rule for every untrusted discovered string (env values AND a stopped
/// worktree path), so forged data can't bloat or inject escapes downstream.
fn clean_metadata(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > MAX_METADATA_LEN || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_string())
    }
}

/// Field separator for an inline `list-sessions` metadata row (#108). A tab can
/// never appear inside a sanitized value (`clean_metadata` rejects control
/// bytes), so it cleanly separates the name field from the `#{E:}`-expanded env
/// fields. A NEWLINE in a forged value is a different story — it can fabricate a
/// whole extra row — which is why the row's name is treated as untrusted and
/// cross-checked against the names-only listing in `run_list`, not trusted by
/// position (see [`parse_session_line`]). Shared with the format string the
/// transport builds in `list_sessions_tokens`, so the writer and reader agree
/// on the layout by construction.
pub const SESSION_FIELD_SEP: &str = "\t";

/// True if a field still carries an unexpanded tmux format token (`#{…}`) —
/// what a tmux too old for `#{E:}` (< 3.0) could echo verbatim instead of the
/// variable's value. Treated as absent so the old-tmux path degrades to empty
/// metadata (the #108 graceful version cliff) rather than surfacing the literal
/// `#{E:REMORA_AGENT}` as a bogus agent name. No legitimate slug/path/number
/// value contains `#{`, so this never drops real metadata.
fn looks_unexpanded(value: &str) -> bool {
    value.contains("#{")
}

/// [`clean_metadata`] for an inline `#{E:}` field, additionally dropping a value
/// that still looks like an unexpanded `#{…}` token (the old-tmux cliff, #108).
fn clean_inline(value: &str) -> Option<String> {
    if looks_unexpanded(value) {
        None
    } else {
        clean_metadata(value)
    }
}

/// Parses one `tmux list-sessions -F` row carrying inline `#{E:}` session
/// metadata — `name <SEP> agent <SEP> workspace <SEP> created_at` (#108) — into
/// the raw session name and its [`DiscoveredEnv`]. The name is UNTRUSTED: a
/// newline inside a forged `#{E:}` value can fabricate an entire extra row, so
/// the caller (`run_list`) accepts a row's metadata only for a name already in
/// the trusted names-only listing, then re-validates via `parse_tmux_session_name`.
/// Missing trailing fields (a session with no metadata, or tmux < 3.0 expanding
/// `#{E:}` to nothing) parse as empty/`None`. `agent`/`workspace_path` pass
/// through [`clean_inline`]; `created_at` parses to `u64` or `None`.
///
/// `splitn(4, …)` caps the split so a forged value smuggling in the SEPARATOR
/// (a tab) corrupts only its own row's metadata fields. The field order here is
/// the contract with `list_sessions_tokens`'s format string; the two must move
/// together.
pub fn parse_session_line(line: &str) -> (&str, DiscoveredEnv) {
    let mut fields = line.splitn(4, SESSION_FIELD_SEP);
    let name = fields.next().unwrap_or("");
    let agent = fields.next().and_then(clean_inline);
    let workspace_path = fields.next().and_then(clean_inline);
    let created_at = fields.next().and_then(|v| v.parse::<u64>().ok());
    (
        name,
        DiscoveredEnv {
            agent,
            created_at,
            workspace_path,
        },
    )
}

/// Parses `git worktree list --porcelain` for one project, returning each
/// surviving remora worktree as `(session id, real absolute path)`. The path
/// comes from the porcelain `worktree <abs-path>` line; the convention match
/// is delegated to [`parse_worktree_path`]. `project` is the trusted config
/// id; non-remora worktrees (the repo's own checkout, foreign paths) are
/// skipped.
pub fn parse_worktree_list(output: &str, project: &ProjectId) -> Vec<(SessionId, String)> {
    let mut found = Vec::new();
    for line in output.lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        if let Some(session) = parse_worktree_path(path, project) {
            found.push((session, path.to_string()));
        }
    }
    found
}

/// Joins live sessions (with metadata) and the full worktree set (live+stopped)
/// into the session list. A key present in both is `Live` (live wins). Each
/// session's `workspace` is `Some(Worktree)` iff a real worktree exists for it,
/// else `Some(Shared)`. Stopped sessions (worktree only) have no tmux env, so
/// `agent`/`created_at` are `None` and `workspace_path` is the sanitized real
/// path (R6). Sorted by `(project, session)` for determinism (matches the
/// fake).
pub fn join(
    live: Vec<(ProjectId, SessionId, DiscoveredEnv)>,
    worktrees: Vec<(ProjectId, SessionId, String)>,
    scanned: &HashSet<ProjectId>,
) -> Vec<SessionMeta> {
    let worktree_keys: HashSet<(ProjectId, SessionId)> = worktrees
        .iter()
        .map(|(p, s, _)| (p.clone(), s.clone()))
        .collect();
    let live_keys: HashSet<(ProjectId, SessionId)> = live
        .iter()
        .map(|(p, s, _)| (p.clone(), s.clone()))
        .collect();

    let mut metas: Vec<SessionMeta> = live
        .into_iter()
        .map(|(project_id, session_id, env)| {
            // Effective mode from real state. A surviving worktree ⇒ Worktree.
            // "No worktree" only proves Shared when the project was actually
            // scanned: a failed/absent worktree scan leaves the mode `None`
            // (unknown) so the client falls back to the project default, rather
            // than mislabeling a live worktree session as shared on a transient
            // scan error (which would wrongly hide Stop/Respawn).
            let has_worktree = worktree_keys.contains(&(project_id.clone(), session_id.clone()));
            let workspace = if has_worktree {
                Some(WorkspaceMode::Worktree)
            } else if scanned.contains(&project_id) {
                Some(WorkspaceMode::Shared)
            } else {
                None
            };
            SessionMeta {
                workspace,
                project_id,
                session_id,
                state: SessionState::Live,
                agent: env.agent,
                created_at: env.created_at,
                workspace_path: env.workspace_path,
            }
        })
        .collect();

    for (project_id, session_id, path) in worktrees {
        if live_keys.contains(&(project_id.clone(), session_id.clone())) {
            continue; // live wins; already stamped Worktree above
        }
        metas.push(SessionMeta {
            project_id,
            session_id,
            state: SessionState::Stopped,
            agent: None,
            created_at: None,
            workspace_path: clean_metadata(&path),
            // A surviving worktree IS a worktree session.
            workspace: Some(WorkspaceMode::Worktree),
        });
    }

    metas.sort_by(|a, b| {
        (a.project_id.as_str(), a.session_id.as_str())
            .cmp(&(b.project_id.as_str(), b.session_id.as_str()))
    });
    metas
}

#[cfg(test)]
mod tests {
    use remora_protocol::WorkspaceMode;

    use super::*;

    fn ids(project: &str, session: &str) -> (ProjectId, SessionId) {
        (
            ProjectId::new(project).expect("slug"),
            SessionId::new(session).expect("slug"),
        )
    }

    #[test]
    fn parses_inline_session_metadata() {
        let line = "remora_api_x\tclaude\t/home/dev/.remora/worktrees/api/x\t1765500000";
        let (name, env) = parse_session_line(line);
        assert_eq!(name, "remora_api_x");
        assert_eq!(env.agent.as_deref(), Some("claude"));
        assert_eq!(
            env.workspace_path.as_deref(),
            Some("/home/dev/.remora/worktrees/api/x")
        );
        assert_eq!(env.created_at, Some(1_765_500_000));
    }

    /// The reader is positional; the writer (`list_sessions_tokens`) emits the
    /// fields in a fixed order joined by `SESSION_FIELD_SEP`. This builds a row
    /// the way the writer does — name then agent/workspace/created_at in that
    /// order, `SESSION_FIELD_SEP`-joined — and asserts the reader recovers every
    /// field. Fails here if the field order or separator drifts between the two.
    #[test]
    fn reads_every_field_a_session_row_carries() {
        let row = ["remora_api_x", "claude", "/wt/api/x", "1765500000"].join(SESSION_FIELD_SEP);
        let (name, env) = parse_session_line(&row);
        assert_eq!(name, "remora_api_x");
        assert_eq!(env.agent.as_deref(), Some("claude"));
        assert_eq!(env.workspace_path.as_deref(), Some("/wt/api/x"));
        assert_eq!(env.created_at, Some(1_765_500_000));
    }

    #[test]
    fn inline_metadata_empty_when_absent_or_old_tmux() {
        // tmux < 3.0 expands `#{E:}` to empty: the row is the name then empty
        // fields. The session still lists (Live decided by the caller) with no
        // metadata — the #108 graceful version cliff.
        let (name, env) = parse_session_line("remora_api_x\t\t\t");
        assert_eq!(name, "remora_api_x");
        assert_eq!(env, DiscoveredEnv::default());
        // A bare name with no separators at all also yields empty metadata.
        let (name2, env2) = parse_session_line("remora_api_y");
        assert_eq!(name2, "remora_api_y");
        assert_eq!(env2, DiscoveredEnv::default());
    }

    #[test]
    fn inline_metadata_rejects_unexpanded_format_token() {
        // A tmux too old to understand `#{E:}` might echo the literal token; it
        // must not surface as a bogus agent name / path (#108 version cliff).
        let line = "remora_api_x\t#{E:REMORA_AGENT}\t#{E:REMORA_WORKSPACE}\t#{E:REMORA_CREATED_AT}";
        let (_, env) = parse_session_line(line);
        assert_eq!(env, DiscoveredEnv::default());
    }

    #[test]
    fn inline_metadata_garbage_maps_to_none() {
        // Control byte in agent, over-length workspace, non-numeric created_at.
        let huge = "x".repeat(MAX_METADATA_LEN + 1);
        let line = format!("remora_api_x\tcla\x07ude\t{huge}\tnot-a-number");
        let (_, env) = parse_session_line(&line);
        assert_eq!(env.agent, None);
        assert_eq!(env.workspace_path, None);
        assert_eq!(env.created_at, None);
    }

    #[test]
    fn parses_worktree_list_porcelain() {
        let api = ProjectId::new("api").expect("slug");
        let out = "worktree /home/dev/api\nHEAD abc\nbranch refs/heads/main\n\n\
                   worktree /home/dev/.remora/worktrees/api/fix-login\nHEAD def\nbranch refs/heads/remora/fix-login\n\n\
                   worktree /home/dev/.remora/worktrees/api/detached\nHEAD 123\ndetached\n";
        let found = parse_worktree_list(out, &api);
        let sessions: Vec<&str> = found.iter().map(|(s, _)| s.as_str()).collect();
        // Main worktree skipped; both remora worktrees found incl. the detached one.
        assert_eq!(sessions, vec!["fix-login", "detached"]);
        assert_eq!(found[0].1, "/home/dev/.remora/worktrees/api/fix-login");
    }

    #[test]
    fn join_live_wins_over_stopped_and_sorts() {
        let (ap, af) = ids("api", "fix-login");
        let (ap2, aa) = ids("api", "add-tests");
        let live = vec![(
            ap.clone(),
            af.clone(),
            DiscoveredEnv {
                agent: Some("claude".into()),
                created_at: Some(1),
                workspace_path: Some("/wt/api/fix-login".into()),
            },
        )];
        // add-tests is stopped; fix-login appears in BOTH (live must win).
        let stopped = vec![
            (
                ap2,
                aa,
                "/home/dev/.remora/worktrees/api/add-tests".to_string(),
            ),
            (
                ap,
                af,
                "/home/dev/.remora/worktrees/api/fix-login".to_string(),
            ),
        ];
        let metas = join(live, stopped, &std::collections::HashSet::new());
        // Sorted: add-tests then fix-login. fix-login is Live (not duplicated).
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].session_id.as_str(), "add-tests");
        assert_eq!(metas[0].state, SessionState::Stopped);
        assert_eq!(
            metas[0].workspace_path.as_deref(),
            Some("/home/dev/.remora/worktrees/api/add-tests")
        );
        assert_eq!(metas[0].agent, None);
        assert_eq!(metas[1].session_id.as_str(), "fix-login");
        assert_eq!(metas[1].state, SessionState::Live);
        assert_eq!(metas[1].agent.as_deref(), Some("claude"));
    }

    #[test]
    fn join_stamps_worktree_mode_for_live_session_with_a_worktree() {
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let live = vec![(p.clone(), s.clone(), DiscoveredEnv::default())];
        let worktrees = vec![(p.clone(), s.clone(), "~/.remora/worktrees/api/s1".into())];
        let metas = join(
            live,
            worktrees,
            &std::collections::HashSet::from([p.clone()]),
        );
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
    }

    #[test]
    fn join_stamps_shared_mode_for_live_session_without_a_worktree() {
        let p = ProjectId::new("scratch").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let scanned = std::collections::HashSet::from([p.clone()]);
        let metas = join(vec![(p, s, DiscoveredEnv::default())], vec![], &scanned);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Shared));
    }

    #[test]
    fn join_stamps_worktree_mode_for_stopped_session() {
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let metas = join(
            vec![],
            vec![(p, s, "~/.remora/worktrees/api/s1".into())],
            &std::collections::HashSet::new(),
        );
        assert_eq!(metas[0].state, SessionState::Stopped);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
    }

    #[test]
    fn join_leaves_mode_unknown_when_project_scan_failed() {
        // A live session whose project's worktree scan did NOT complete must not
        // be mislabeled Shared: its mode is unknown (None), so the client falls
        // back to the project default rather than wrongly hiding Stop/Respawn.
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let metas = join(
            vec![(p, s, DiscoveredEnv::default())],
            vec![],
            &std::collections::HashSet::new(), // project not scanned
        );
        assert_eq!(metas[0].workspace, None);
    }
}
