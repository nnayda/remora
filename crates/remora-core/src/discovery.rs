//! Transport-agnostic discovery: turn sandbox output (tmux env, git worktree
//! list) into `SessionMeta`. Pure — no `SshHost`, no argv (mirrors
//! `spawn_plan`, so kubectl reuses it). Everything here is untrusted input
//! (ADR-0004): unparseable metadata maps to `None`, forged paths are dropped.

use std::collections::HashSet;

use remora_protocol::{ProjectId, SessionId, SessionMeta, SessionState, WorkspaceMode};

use crate::naming;

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

/// One worktree as reported by `git worktree list --porcelain`. `branch` is the
/// short branch name (no `refs/heads/`), or `None` when the worktree is detached
/// or bare. Untrusted (ADR-0004): used as a discovery hint, cross-checked, never
/// turned into a command blindly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
}

/// Parse `git worktree list --porcelain` into one [`WorktreeInfo`] per worktree.
/// Records are blank-line separated; a record starts at `worktree <path>` and a
/// `branch refs/heads/<name>` line (if any, before the next `worktree`) gives the
/// branch. `detached`/`bare` records yield `branch: None`.
pub fn parse_worktree_porcelain(output: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeInfo> = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = cur.take() {
                out.push(done);
            }
            cur = Some(WorktreeInfo {
                path: path.to_string(),
                branch: None,
            });
        } else if let Some(refname) = line.strip_prefix("branch refs/heads/") {
            if let Some(c) = cur.as_mut() {
                c.branch = Some(refname.to_string());
            }
        }
        // `HEAD`, `detached`, `bare`, `locked`, blank lines: ignored (branch
        // stays None for detached/bare, which is the intended "nameless").
    }
    if let Some(done) = cur.take() {
        out.push(done);
    }
    out
}

/// Joins live sessions (with metadata) and the full worktree set (live+stopped)
/// into the session list (A2′, #124). Matching is path-anchored: a live
/// session's `REMORA_WORKSPACE` is canonicalized and compared against each
/// worktree's real path. The primary checkout (canonical path == `project_paths`
/// entry) surfaces as `Shared`; other worktrees as `Worktree`. Branch identity
/// comes from the porcelain (`wt.branch`); `derive_session_id` derives the tmux
/// slug. Detached/nameless worktrees are skipped. Live sessions whose
/// `REMORA_WORKSPACE` matches no worktree become Shared rows (branch `None`).
/// Sorted by `(project, session)` for determinism (matches the fake).
pub fn join(
    live: Vec<(ProjectId, SessionId, DiscoveredEnv)>,
    worktrees: Vec<(ProjectId, WorktreeInfo)>,
    project_paths: &std::collections::HashMap<ProjectId, String>,
    home: &str,
    scanned: &HashSet<ProjectId>,
) -> Vec<SessionMeta> {
    // Index live sessions by their canonical workspace path so the join is
    // O(worktrees) rather than O(live × worktrees). Sessions with no
    // REMORA_WORKSPACE go into a separate bucket; they can never match a
    // worktree path so they always surface as unmatched-live rows.
    // Multiple live sessions can share the same canonical workspace path: shared-
    // mode sessions all set REMORA_WORKSPACE to the project root, so two+ shared
    // sessions on one project map to the same key. Using Vec avoids silently
    // overwriting all but one (the old single-value HashMap behaviour).
    let mut live_by_path: std::collections::HashMap<
        (ProjectId, String),
        Vec<(SessionId, DiscoveredEnv)>,
    > = std::collections::HashMap::new();
    let mut live_unmatched: Vec<(ProjectId, SessionId, DiscoveredEnv)> = Vec::new();
    for (p, s, env) in live {
        match env.workspace_path.as_deref() {
            Some(wp) => {
                let key = (p.clone(), canonicalize_remote_path(wp, home));
                live_by_path.entry(key).or_default().push((s, env));
            }
            None => live_unmatched.push((p, s, env)),
        }
    }

    let mut metas = Vec::new();

    for (project, wt) in worktrees {
        let cpath = canonicalize_remote_path(&wt.path, home);
        let is_primary = project_paths
            .get(&project)
            .map(|pp| pp == &cpath)
            .unwrap_or(false);
        let key = (project.clone(), cpath.clone());
        // Pop ONE live entry for this key: a worktree can be claimed by at most
        // one live session (the physical checkout is exclusive). Using `get_mut`
        // + `pop` rather than `remove` leaves remaining sessions in the vec for
        // the drain phase below, so none are silently dropped.
        //
        // LIVE match → use the real tmux slug as `session_id` so that reconnect
        // resolves correctly even after a branch rename (the tmux name never
        // changes mid-session). A live detached worktree is also surfaced (the
        // caller can still reconnect via the real slug).
        // STOPPED (no live match) → derive from the branch as before; skip
        // detached / nameless worktrees since they have no identity to respawn.
        let (session_id, state, agent, created_at) =
            match live_by_path.get_mut(&key).and_then(|v| v.pop()) {
                Some((sid, env)) => (sid, SessionState::Live, env.agent, env.created_at),
                None => {
                    let Some(sid) = naming::derive_session_id(wt.branch.as_deref()) else {
                        continue;
                    };
                    (sid, SessionState::Stopped, None, None)
                }
            };
        metas.push(SessionMeta {
            project_id: project,
            session_id,
            state,
            agent,
            created_at,
            workspace_path: clean_metadata(&cpath),
            workspace: Some(if is_primary {
                WorkspaceMode::Shared
            } else {
                WorkspaceMode::Worktree
            }),
            branch: wt.branch.as_deref().and_then(clean_metadata),
        });
    }

    // Live sessions whose REMORA_WORKSPACE matched no worktree → Shared rows.
    // "No worktree" only proves Shared when the project was actually scanned:
    // a failed scan leaves the mode `None` so the client falls back to the
    // project default rather than mislabeling a live session on a transient
    // scan error (which would wrongly hide Stop/Respawn).
    // Flatten: each vec entry is a session that was not claimed by a worktree,
    // so every one becomes its own Shared row (none are silently dropped).
    for ((project, _cpath), sessions) in live_by_path {
        for (sid, env) in sessions {
            let workspace = if scanned.contains(&project) {
                Some(WorkspaceMode::Shared)
            } else {
                None
            };
            metas.push(SessionMeta {
                project_id: project.clone(),
                session_id: sid,
                state: SessionState::Live,
                agent: env.agent,
                created_at: env.created_at,
                workspace_path: env.workspace_path.as_deref().and_then(clean_metadata),
                workspace,
                branch: None,
            });
        }
    }
    for (project, sid, env) in live_unmatched {
        let workspace = if scanned.contains(&project) {
            Some(WorkspaceMode::Shared)
        } else {
            None
        };
        metas.push(SessionMeta {
            project_id: project,
            session_id: sid,
            state: SessionState::Live,
            agent: env.agent,
            created_at: env.created_at,
            workspace_path: env.workspace_path.as_deref().and_then(clean_metadata),
            workspace,
            branch: None,
        });
    }

    metas.sort_by(|a, b| {
        (a.project_id.as_str(), a.session_id.as_str())
            .cmp(&(b.project_id.as_str(), b.session_id.as_str()))
    });
    metas
}

/// Make a worktree path comparable for the discovery join: expand a single
/// leading `~/` against the remote `$HOME`, and drop a trailing `/`. Porcelain
/// paths are already absolute (idempotent here); a live session's
/// `REMORA_WORKSPACE` is logical (`~/…`) and needs the expansion (A1, #124).
pub fn canonicalize_remote_path(path: &str, home: &str) -> String {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", home.trim_end_matches('/'), rest),
        None => path.to_string(),
    };
    expanded.trim_end_matches('/').to_string()
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

    use std::collections::HashMap;

    fn paths(pairs: &[(&str, &str)]) -> HashMap<ProjectId, String> {
        pairs
            .iter()
            .map(|&(p, path)| (ProjectId::new(p).expect("slug"), path.to_string()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // join tests — new path-anchored, branch-identity design (A2′, #124)
    // -----------------------------------------------------------------------

    #[test]
    fn live_worktree_join_uses_canonical_path_and_branch_identity() {
        let (proj, _sid) = ids("api", "ignored");
        // live REMORA_WORKSPACE is logical `~/…`; porcelain is absolute.
        let live = vec![(
            proj.clone(),
            SessionId::new("fix-login").expect("slug"),
            DiscoveredEnv {
                agent: Some("claude".into()),
                created_at: Some(1),
                workspace_path: Some("~/.remora/worktrees/api/fix-login".into()),
            },
        )];
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/.remora/worktrees/api/fix-login".into(),
                branch: Some("remora/fix-login".into()),
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            live,
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
        assert_eq!(metas[0].branch.as_deref(), Some("remora/fix-login"));
        assert_eq!(metas[0].session_id.as_str(), "fix-login"); // remora/ round-trips
        assert_eq!(metas[0].agent.as_deref(), Some("claude"));
    }

    #[test]
    fn primary_checkout_surfaces_as_shared_named_by_branch() {
        let proj = ProjectId::new("api").expect("slug");
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/api".into(),
                branch: Some("main".into()),
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            vec![],
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Shared)); // A2′
        assert_eq!(metas[0].branch.as_deref(), Some("main"));
        assert_eq!(metas[0].state, SessionState::Stopped); // no live session on it
    }

    #[test]
    fn hand_made_worktree_surfaces_as_stopped_worktree_session() {
        let proj = ProjectId::new("api").expect("slug");
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/scratch/spike".into(),
                branch: Some("feat/spike".into()),
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            vec![],
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].state, SessionState::Stopped);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
        assert_eq!(metas[0].branch.as_deref(), Some("feat/spike"));
        assert!(metas[0].session_id.as_str().starts_with("feat-spike-"));
    }

    #[test]
    fn detached_worktree_is_dropped() {
        let proj = ProjectId::new("api").expect("slug");
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/scratch/x".into(),
                branch: None,
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            vec![],
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        assert!(metas.is_empty()); // nameless stopped → not surfaced (PR-A limitation)
    }

    /// CRITICAL: A live session whose tmux slug is `fix-login` but whose
    /// worktree branch was renamed to `feat/renamed` must surface with
    /// `session_id == "fix-login"` (the real tmux name, so reconnect resolves)
    /// and `branch == Some("feat/renamed")` (current branch for display).
    /// The old code discarded the real sid and derived from the branch instead,
    /// which broke reconnect the moment the branch name changed (#124).
    #[test]
    fn live_worktree_uses_tmux_session_id_not_branch_after_rename() {
        let proj = ProjectId::new("api").expect("slug");
        // The tmux session was created when the branch was `remora/fix-login`,
        // so its slug is `fix-login`. The user later ran `git branch -m` so
        // the porcelain now reports `feat/renamed` — but the tmux name is
        // unchanged.
        let live = vec![(
            proj.clone(),
            SessionId::new("fix-login").expect("slug"),
            DiscoveredEnv {
                agent: Some("claude".into()),
                created_at: Some(2),
                workspace_path: Some("~/.remora/worktrees/api/fix-login".into()),
            },
        )];
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/.remora/worktrees/api/fix-login".into(),
                branch: Some("feat/renamed".into()),
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            live,
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].state, SessionState::Live);
        // session_id MUST be the real tmux slug, not a value derived from the
        // renamed branch — otherwise `naming::tmux_session_name(project, id)`
        // rebuilds a nonexistent session name.
        assert_eq!(metas[0].session_id.as_str(), "fix-login");
        // branch follows the worktree porcelain (display / future respawn hint).
        assert_eq!(metas[0].branch.as_deref(), Some("feat/renamed"));
        assert_eq!(metas[0].agent.as_deref(), Some("claude"));
    }

    /// A LIVE worktree that is detached (branch: None) must still be surfaced
    /// as Live with the real tmux session_id — the caller can reconnect via
    /// the tmux name even without a branch. The stopped-detached path (no live
    /// match) is what gets dropped; a live-detached worktree must not be.
    #[test]
    fn live_detached_worktree_is_surfaced_not_dropped() {
        let proj = ProjectId::new("api").expect("slug");
        let live = vec![(
            proj.clone(),
            SessionId::new("my-session").expect("slug"),
            DiscoveredEnv {
                agent: None,
                created_at: None,
                workspace_path: Some("/home/dev/.remora/worktrees/api/detached".into()),
            },
        )];
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/.remora/worktrees/api/detached".into(),
                branch: None, // detached HEAD
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            live,
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        assert_eq!(metas.len(), 1, "live detached worktree must not be dropped");
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].session_id.as_str(), "my-session");
        assert_eq!(metas[0].branch, None);
    }

    // Migrated from old join tests — behavior still holds under the new design.

    #[test]
    fn join_stamps_shared_mode_for_live_session_without_a_worktree() {
        // A live session not matched by any worktree path, but whose project
        // was scanned, surfaces as Shared (branch None).
        let p = ProjectId::new("scratch").expect("slug");
        let s = SessionId::new("s1").expect("slug");
        let scanned = std::collections::HashSet::from([p.clone()]);
        let metas = join(
            vec![(p, s, DiscoveredEnv::default())],
            vec![],
            &std::collections::HashMap::new(),
            "/home/dev",
            &scanned,
        );
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Shared));
    }

    #[test]
    fn join_drains_live_session_with_unmatched_workspace_path_as_shared() {
        // A live session whose REMORA_WORKSPACE is SET (`Some`) but canonicalizes
        // to a path that matches no worktree. This exercises the `live_by_path`
        // drain loop — distinct from
        // `join_stamps_shared_mode_for_live_session_without_a_worktree`, which
        // uses `workspace_path: None` and so exercises the *other* (`live_unmatched`)
        // loop. The session must surface as Live + Shared (its project was scanned)
        // with its `workspace_path` preserved verbatim.
        let proj = ProjectId::new("api").expect("slug");
        let live = vec![(
            proj.clone(),
            SessionId::new("s1").expect("slug"),
            DiscoveredEnv {
                agent: Some("claude".into()),
                created_at: Some(7),
                workspace_path: Some("/home/dev/elsewhere".into()), // matches no worktree
            },
        )];
        // A worktree that does NOT match the live session's path (it's the primary
        // checkout), so the live session is never popped and falls to the drain.
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/api".into(),
                branch: Some("main".into()),
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            live,
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        let live_row = metas
            .iter()
            .find(|m| m.session_id.as_str() == "s1")
            .expect("the unmatched live session must surface");
        assert_eq!(live_row.state, SessionState::Live);
        assert_eq!(live_row.workspace, Some(WorkspaceMode::Shared));
        assert_eq!(
            live_row.workspace_path.as_deref(),
            Some("/home/dev/elsewhere"),
            "the drained live session keeps its original workspace_path"
        );
        assert_eq!(live_row.branch, None);
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
            &std::collections::HashMap::new(),
            "/home/dev",
            &std::collections::HashSet::new(), // project not scanned
        );
        assert_eq!(metas[0].workspace, None);
    }

    /// REGRESSION GUARD: shared-mode sessions all set `REMORA_WORKSPACE` to the
    /// project root, so two+ shared sessions on one project resolve to the same
    /// `(project, canonical_path)` key. The old single-value HashMap silently
    /// dropped all but one; this test verifies that BOTH appear in the output.
    #[test]
    fn join_surfaces_all_live_sessions_sharing_a_path() {
        let proj = ProjectId::new("api").expect("slug");
        let s1 = SessionId::new("s1").expect("slug");
        let s2 = SessionId::new("s2").expect("slug");
        // Both sessions share the same workspace_path (the project root — the
        // shared-mode pattern: spawn_plan.rs WorkspaceMode::Shared => project.path).
        let live = vec![
            (
                proj.clone(),
                s1.clone(),
                DiscoveredEnv {
                    agent: Some("claude".into()),
                    created_at: Some(1),
                    workspace_path: Some("/home/dev/api".into()),
                },
            ),
            (
                proj.clone(),
                s2.clone(),
                DiscoveredEnv {
                    agent: Some("codex".into()),
                    created_at: Some(2),
                    workspace_path: Some("/home/dev/api".into()),
                },
            ),
        ];
        // Primary checkout at the project path; one worktree row claims one of them.
        let wts = vec![(
            proj.clone(),
            WorktreeInfo {
                path: "/home/dev/api".into(),
                branch: Some("main".into()),
            },
        )];
        let scanned: HashSet<ProjectId> = [proj.clone()].into_iter().collect();
        let metas = join(
            live,
            wts,
            &paths(&[("api", "/home/dev/api")]),
            "/home/dev",
            &scanned,
        );
        // Both sessions must appear — none silently dropped.
        assert_eq!(metas.len(), 2, "both sessions must surface: {metas:?}");
        let ids: HashSet<&str> = metas.iter().map(|m| m.session_id.as_str()).collect();
        assert!(ids.contains("s1"), "s1 missing from: {metas:?}");
        assert!(ids.contains("s2"), "s2 missing from: {metas:?}");
        // All rows are Live.
        assert!(
            metas.iter().all(|m| m.state == SessionState::Live),
            "all rows must be Live: {metas:?}"
        );
    }

    #[test]
    fn parses_path_and_branch_per_worktree() {
        let out = "\
worktree /home/dev/api
HEAD abc123
branch refs/heads/main

worktree /home/dev/.remora/worktrees/api/fix-login
HEAD def456
branch refs/heads/remora/fix-login

worktree /home/dev/scratch/spike
HEAD 999aaa
detached
";
        let got = parse_worktree_porcelain(out);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].path, "/home/dev/api");
        assert_eq!(got[0].branch.as_deref(), Some("main"));
        assert_eq!(got[1].branch.as_deref(), Some("remora/fix-login"));
        assert_eq!(got[2].path, "/home/dev/scratch/spike");
        assert_eq!(got[2].branch, None); // detached → nameless
    }

    #[test]
    fn parse_worktree_porcelain_handles_bare_and_trailing_newline() {
        let out = "worktree /home/dev/api\nbare\n\n";
        let got = parse_worktree_porcelain(out);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].branch, None);
    }

    #[test]
    fn parse_worktree_porcelain_empty_input_is_empty() {
        assert!(parse_worktree_porcelain("").is_empty());
    }

    #[test]
    fn parse_worktree_porcelain_captures_branch_on_locked_record() {
        // A `locked` line within a worktree record must not interfere with
        // capturing the branch: `locked` is ignored and the `branch refs/heads/…`
        // line still sets it. (Locked worktrees are real — e.g. a worktree on
        // removable media — and must still be discoverable by branch identity.)
        let out = "\
worktree /home/dev/.remora/worktrees/api/held
HEAD abc123
branch refs/heads/remora/held
locked
";
        let got = parse_worktree_porcelain(out);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "/home/dev/.remora/worktrees/api/held");
        assert_eq!(got[0].branch.as_deref(), Some("remora/held"));
    }

    #[test]
    fn parse_worktree_porcelain_ignores_orphan_branch_line() {
        // A `branch refs/heads/…` line with no preceding `worktree` line has no
        // record to attach to; it must be silently ignored — no panic, no phantom
        // worktree — and a following well-formed record still parses normally.
        let out = "branch refs/heads/orphan\n\nworktree /home/dev/api\nbranch refs/heads/main\n";
        let got = parse_worktree_porcelain(out);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "/home/dev/api");
        assert_eq!(got[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn canonicalize_expands_leading_tilde() {
        assert_eq!(
            canonicalize_remote_path("~/work/feat/login", "/home/dev"),
            "/home/dev/work/feat/login"
        );
    }

    #[test]
    fn canonicalize_leaves_absolute_untouched_and_trims_trailing_slash() {
        assert_eq!(
            canonicalize_remote_path("/home/dev/api/", "/home/dev"),
            "/home/dev/api"
        );
        assert_eq!(
            canonicalize_remote_path("/mnt/x/wt", "/home/dev"),
            "/mnt/x/wt"
        );
    }

    #[test]
    fn canonicalize_does_not_expand_bare_tilde_user() {
        // `~user` is not ours to expand; leave it (it will simply fail to match).
        assert_eq!(canonicalize_remote_path("~bob/x", "/home/dev"), "~bob/x");
    }
}
