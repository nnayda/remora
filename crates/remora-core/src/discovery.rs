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

/// Parses `tmux show-environment -t <name>` output. Lines are `KEY=VALUE`
/// (tmux emits `-KEY` for vars unset in the session; those have no `=` and
/// are skipped). Duplicate keys: last wins. `created_at` parses to `u64` or
/// `None`; `agent`/`workspace_path` go through [`clean_metadata`].
pub fn parse_session_environment(output: &str) -> DiscoveredEnv {
    let mut env = DiscoveredEnv::default();
    for line in output.lines() {
        // `str::lines()` already strips `\r\n`; an embedded `\r` would be a
        // control byte and is rejected by `clean_metadata`.
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "REMORA_AGENT" => env.agent = clean_metadata(value),
            "REMORA_WORKSPACE" => env.workspace_path = clean_metadata(value),
            "REMORA_CREATED_AT" => env.created_at = value.parse::<u64>().ok(),
            _ => {}
        }
    }
    env
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
            let has_worktree = worktree_keys.contains(&(project_id.clone(), session_id.clone()));
            SessionMeta {
                workspace: Some(if has_worktree {
                    WorkspaceMode::Worktree
                } else {
                    WorkspaceMode::Shared
                }),
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
    fn parses_remora_env_vars() {
        let out = "REMORA_AGENT=claude\nREMORA_WORKSPACE=/home/dev/.remora/worktrees/api/x\n\
                   REMORA_CREATED_AT=1765500000\nSHELL=/bin/bash\n-PATH\n";
        let env = parse_session_environment(out);
        assert_eq!(env.agent.as_deref(), Some("claude"));
        assert_eq!(
            env.workspace_path.as_deref(),
            Some("/home/dev/.remora/worktrees/api/x")
        );
        assert_eq!(env.created_at, Some(1_765_500_000));
    }

    /// The reader matches env keys as string literals; the writer
    /// (`spawn_plan`) emits them via the `naming::ENV_*` constants. Nothing in
    /// the type system couples the two, so a rename on the write side would
    /// silently break discovery. This test links them: it builds the env block
    /// from the constants the writer uses and asserts the reader recognizes
    /// every field — fail here if the literals in `parse_session_environment`
    /// ever drift from `naming::ENV_*`.
    #[test]
    fn reads_exactly_the_keys_the_writer_emits() {
        use crate::naming::{ENV_AGENT, ENV_CREATED_AT, ENV_WORKSPACE};
        let out =
            format!("{ENV_AGENT}=claude\n{ENV_WORKSPACE}=/wt/api/x\n{ENV_CREATED_AT}=1765500000\n");
        let env = parse_session_environment(&out);
        assert_eq!(env.agent.as_deref(), Some("claude"));
        assert_eq!(env.workspace_path.as_deref(), Some("/wt/api/x"));
        assert_eq!(env.created_at, Some(1_765_500_000));
    }

    #[test]
    fn env_duplicate_key_last_wins() {
        let env = parse_session_environment("REMORA_AGENT=first\nREMORA_AGENT=second\n");
        assert_eq!(env.agent.as_deref(), Some("second"));
    }

    #[test]
    fn env_garbage_maps_to_none() {
        // Non-numeric created_at, control byte in agent, over-length workspace.
        let huge = "x".repeat(MAX_METADATA_LEN + 1);
        let out = format!(
            "REMORA_CREATED_AT=not-a-number\nREMORA_AGENT=cla\x07ude\nREMORA_WORKSPACE={huge}\n"
        );
        let env = parse_session_environment(&out);
        assert_eq!(env.created_at, None);
        assert_eq!(env.agent, None);
        assert_eq!(env.workspace_path, None);
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
        let metas = join(live, stopped);
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
        let metas = join(live, worktrees);
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
    }

    #[test]
    fn join_stamps_shared_mode_for_live_session_without_a_worktree() {
        let p = ProjectId::new("scratch").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let metas = join(vec![(p, s, DiscoveredEnv::default())], vec![]);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Shared));
    }

    #[test]
    fn join_stamps_worktree_mode_for_stopped_session() {
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let metas = join(vec![], vec![(p, s, "~/.remora/worktrees/api/s1".into())]);
        assert_eq!(metas[0].state, SessionState::Stopped);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
    }
}
