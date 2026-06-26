//! The versioned tmux session name shared by every transport and (later)
//! discovery. The name *is* the session's identity on the sandbox.

use std::hash::{Hash, Hasher};

use remora_protocol::{ProjectId, SessionId};

/// Builds the tmux session name for a session (ADR-0004):
/// `remora_<project-id>_<session-id>`. Ids are validated `[a-z0-9-]+`
/// slugs, so the `_`-separated name parses unambiguously. The inverse is
/// [`parse_tmux_session_name`].
pub fn tmux_session_name(project: &ProjectId, session: &SessionId) -> String {
    format!("remora_{}_{}", project.as_str(), session.as_str())
}

/// Parses a tmux session name back to its ids — the inverse of
/// [`tmux_session_name`]. Returns `None` for any name that isn't a
/// well-formed `remora_<project>_<session>` with both halves valid slugs.
/// Discovered names are untrusted (ADR-0004): forged or foreign names are
/// dropped here, never forwarded.
pub fn parse_tmux_session_name(name: &str) -> Option<(ProjectId, SessionId)> {
    let rest = name.strip_prefix("remora_")?;
    // Ids are `[a-z0-9-]+` (no `_`), so the first `_` is the only separator.
    let (project, session) = rest.split_once('_')?;
    let project = ProjectId::new(project).ok()?;
    let session = SessionId::new(session).ok()?;
    Some((project, session))
}

/// Parses a worktree path back to its session id — the inverse of
/// [`worktree_path`]. Returns `Some` only when `abs_path`'s trailing
/// components are `… / .remora / worktrees / <project> / <session>` with
/// `<project>` equal to `project` (the trusted caller id) and `<session>` a
/// valid slug. Matched on the path so a detached-HEAD worktree still resolves
/// (decision 3); only `<session>` comes from discovered bytes.
pub fn parse_worktree_path(abs_path: &str, project: &ProjectId) -> Option<SessionId> {
    // Discovery input is untrusted (ADR-0004) and this matches on *trailing*
    // segments, so require a real absolute (`/…`) or `~/…` path first —
    // otherwise a forged relative path whose tail happens to match the
    // convention (e.g. `tmp/.remora/worktrees/api/s1`) would parse.
    if !(abs_path.starts_with('/') || abs_path.starts_with("~/")) {
        return None;
    }
    let segments: Vec<&str> = abs_path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [.., dot_remora, worktrees, proj, session]
            if *dot_remora == ".remora"
                && *worktrees == "worktrees"
                && *proj == project.as_str() =>
        {
            SessionId::new(*session).ok()
        }
        _ => None,
    }
}

/// Worktree path convention (ADR-0004, versioned wire format):
/// `~/.remora/worktrees/<project-id>/<session-id>`. The `~` is expanded by
/// the transport, never stored expanded (stage-6 discovery round-trips it).
pub fn worktree_path(project: &ProjectId, session: &SessionId) -> String {
    format!(
        "~/.remora/worktrees/{}/{}",
        project.as_str(),
        session.as_str()
    )
}

/// Branch convention for a worktree session (ADR-0004): `remora/<session-id>`.
pub fn branch_name(session: &SessionId) -> String {
    format!("remora/{}", session.as_str())
}

/// Derive the internal `session_id` slug from a worktree's branch (Mechanism A,
/// #124 / ADR-0015). `session_id` is never displayed (the branch is); it is the
/// tmux-name token and respawn lock. A `remora/<slug>` branch round-trips to
/// `<slug>` so existing remora worktrees match their live tmux session_id; any
/// other branch is slugified with a short deterministic hash suffix so that two
/// branches which slugify the same (e.g. `feat/login` vs `feat-login`) stay
/// distinct. `None`/detached → `None` (the worktree is nameless; see ADR-0015).
pub fn derive_session_id(branch: Option<&str>) -> Option<SessionId> {
    let branch = branch?;
    if let Some(rest) = branch.strip_prefix("remora/") {
        if let Ok(id) = SessionId::new(rest) {
            return Some(id);
        }
    }
    let base: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of '-' and trim, so the slug is well-formed.
    let mut base: String = base
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if base.is_empty() {
        base.push('x');
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    branch.hash(&mut hasher);
    let suffix = format!("{:08x}", hasher.finish() as u32);
    // `DefaultHasher` is not portable across Rust versions; that is fine — the id
    // only needs to be stable within one running discovery process, and is
    // recomputed every poll. (If cross-version stability is ever needed, swap in
    // a fixed hash.)
    SessionId::new(format!("{base}-{suffix}")).ok()
}

/// Session-environment metadata keys (ADR-0004, versioned wire format).
/// Stage-5 spawn writes them; discovery reads them back inline via tmux's
/// `#{E:VAR}` expansion in `list-sessions` (#108).
pub const ENV_AGENT: &str = "REMORA_AGENT";
pub const ENV_WORKSPACE: &str = "REMORA_WORKSPACE";
pub const ENV_CREATED_AT: &str = "REMORA_CREATED_AT";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_follows_the_versioned_convention() {
        let project = ProjectId::new("api").expect("valid slug");
        let session = SessionId::new("fix-login").expect("valid slug");
        assert_eq!(
            worktree_path(&project, &session),
            "~/.remora/worktrees/api/fix-login"
        );
    }

    #[test]
    fn branch_name_is_remora_slash_session() {
        let session = SessionId::new("fix-login").expect("valid slug");
        assert_eq!(branch_name(&session), "remora/fix-login");
    }

    #[test]
    fn env_var_names_are_stable_wire_format() {
        assert_eq!(ENV_AGENT, "REMORA_AGENT");
        assert_eq!(ENV_WORKSPACE, "REMORA_WORKSPACE");
        assert_eq!(ENV_CREATED_AT, "REMORA_CREATED_AT");
    }

    #[test]
    fn formats_as_remora_project_session() {
        let project = ProjectId::new("api").expect("valid slug");
        let session = SessionId::new("fix-login").expect("valid slug");
        assert_eq!(
            tmux_session_name(&project, &session),
            "remora_api_fix-login"
        );

        // Hyphenated slugs are unambiguous because `_` is the only separator.
        let project = ProjectId::new("web-app").expect("valid slug");
        let session = SessionId::new("add-tests").expect("valid slug");
        assert_eq!(
            tmux_session_name(&project, &session),
            "remora_web-app_add-tests"
        );
    }

    #[test]
    fn parses_a_valid_tmux_session_name() {
        let (p, s) = parse_tmux_session_name("remora_api_fix-login").expect("valid name");
        assert_eq!(p.as_str(), "api");
        assert_eq!(s.as_str(), "fix-login");

        // Hyphenated slugs round-trip; `_` is the only separator.
        let (p, s) = parse_tmux_session_name("remora_web-app_add-tests").expect("valid name");
        assert_eq!((p.as_str(), s.as_str()), ("web-app", "add-tests"));
    }

    #[test]
    fn tmux_session_name_round_trips_through_parse() {
        let project = ProjectId::new("web-app").expect("slug");
        let session = SessionId::new("add-tests").expect("slug");
        let name = tmux_session_name(&project, &session);
        assert_eq!(parse_tmux_session_name(&name), Some((project, session)));
    }

    #[test]
    fn rejects_malformed_tmux_session_names() {
        for bad in [
            "remora_api",     // no session
            "remora__x",      // empty project
            "remora_api_",    // empty session
            "remora_api_a_b", // session id can't contain `_`
            "main",           // not a remora session
            "remora_API_x",   // upper-case is not a slug
            "",
        ] {
            assert_eq!(parse_tmux_session_name(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn parses_a_worktree_path_to_its_session() {
        let api = ProjectId::new("api").expect("slug");
        // Absolute (git-expanded) path matches the trailing convention.
        assert_eq!(
            parse_worktree_path("/home/dev/.remora/worktrees/api/fix-login", &api)
                .map(|s| s.as_str().to_string()),
            Some("fix-login".to_string())
        );
        // The `~`-prefixed convention form parses too (same trailing 4 segments).
        assert_eq!(
            parse_worktree_path("~/.remora/worktrees/api/fix-login", &api)
                .map(|s| s.as_str().to_string()),
            Some("fix-login".to_string())
        );
        // A detached-HEAD worktree has the same path, so it still resolves.
    }

    #[test]
    fn rejects_non_convention_worktree_paths() {
        let api = ProjectId::new("api").expect("slug");
        for bad in [
            "/home/dev/api",                            // the main worktree
            "/home/dev/.remora/worktrees/api",          // missing session
            "/home/dev/.remora/worktrees/other/x",      // wrong project
            "/home/dev/.config/worktrees/api/x",        // not `.remora`
            "/home/dev/.remora/worktrees/api/Bad_Slug", // session not a slug
            "tmp/.remora/worktrees/api/s1",             // relative, not absolute
            "~user/.remora/worktrees/api/s1",           // `~user`, not `~/`
        ] {
            assert_eq!(
                parse_worktree_path(bad, &api),
                None,
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn derive_session_id_strips_remora_prefix_to_round_trip() {
        // A remora-spawned worktree's branch is `remora/<slug>`; deriving must
        // return exactly that slug so it equals the live tmux-name session_id.
        assert_eq!(
            derive_session_id(Some("remora/fix-login")).map(|s| s.as_str().to_string()),
            Some("fix-login".to_string())
        );
    }

    #[test]
    fn derive_session_id_sanitizes_and_hashes_other_branches() {
        let a = derive_session_id(Some("feat/login")).expect("slug");
        let b = derive_session_id(Some("feat-login")).expect("slug");
        // Both are valid slugs, deterministic, and distinct (hash disambiguates
        // what slugification would otherwise collide).
        assert!(a.as_str().starts_with("feat-login-"));
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(derive_session_id(Some("feat/login")).expect("deterministic"), a); // deterministic
    }

    #[test]
    fn derive_session_id_is_none_for_detached_or_absent() {
        assert_eq!(derive_session_id(None), None);
    }

    #[test]
    fn derive_session_id_remora_prefix_with_invalid_slug_falls_through() {
        // `remora/` followed by a non-slug is NOT treated as a round-trip.
        let s = derive_session_id(Some("remora/Bad_Slug")).expect("slug");
        assert!(s.as_str().starts_with("remora-bad-slug-"));
    }
}
