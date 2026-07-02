//! VS Code launch mapping (desktop-only): turn a transport-agnostic
//! [`RemoteWorkspace`] locator into the concrete `code --remote` argv and
//! resolve the local `code` binary. The only place `code` / `ssh-remote+` are
//! named — core stays editor-agnostic (AGENTS.md one rule).

use remora_core::RemoteWorkspace;

use crate::launch::{resolve_binary, PathProbe};

/// macOS bundles the CLI here; a GUI-launched Remora won't have it on PATH.
#[allow(dead_code)]
const CODE_EXTRA_CANDIDATES: &[&str] =
    &["/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"];

/// Build the argv the desktop spawns to open `target` in VS Code, resolving
/// the local `code` binary to an absolute path. `Err` (display string) when
/// `code` is not installed / not on the bare GUI PATH.
#[allow(dead_code)]
pub fn launch_argv(target: &RemoteWorkspace, probe: &dyn PathProbe) -> Result<Vec<String>, String> {
    let code = resolve_binary("code", CODE_EXTRA_CANDIDATES, probe).ok_or_else(|| {
        "VS Code (`code`) not found — install VS Code or add `code` to your PATH".to_string()
    })?;
    match target {
        RemoteWorkspace::Ssh { authority, path } => Ok(vec![
            code.to_string_lossy().into_owned(),
            "--remote".into(),
            format!("ssh-remote+{authority}"),
            path.clone(),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::PathProbe;
    use remora_core::RemoteWorkspace;
    use std::collections::BTreeSet;
    use std::path::Path;

    struct FakeProbe(BTreeSet<&'static str>);
    impl PathProbe for FakeProbe {
        fn is_file(&self, path: &Path) -> bool {
            self.0.contains(path.to_str().unwrap_or_default())
        }
    }

    #[test]
    fn ssh_locator_becomes_code_remote_argv() {
        let probe = FakeProbe(BTreeSet::from(["/usr/local/bin/code"]));
        let target = RemoteWorkspace::Ssh {
            authority: "nathan@hermes:2222".into(),
            path: "/home/nathan/wt/api/fix-login".into(),
        };
        let argv = launch_argv(&target, &probe).expect("argv");
        assert_eq!(
            argv,
            [
                "/usr/local/bin/code",
                "--remote",
                "ssh-remote+nathan@hermes:2222",
                "/home/nathan/wt/api/fix-login",
            ]
        );
    }

    #[test]
    fn resolves_code_from_macos_app_bundle_fallback() {
        let probe = FakeProbe(BTreeSet::from([
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        ]));
        let target = RemoteWorkspace::Ssh {
            authority: "hermes".into(),
            path: "/w".into(),
        };
        let argv = launch_argv(&target, &probe).expect("argv");
        assert_eq!(
            argv[0],
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"
        );
        assert_eq!(argv[2], "ssh-remote+hermes");
    }

    #[test]
    fn missing_code_is_a_clear_error() {
        let probe = FakeProbe(BTreeSet::new());
        let target = RemoteWorkspace::Ssh {
            authority: "hermes".into(),
            path: "/w".into(),
        };
        let err = launch_argv(&target, &probe).expect_err("no code");
        assert!(
            err.contains("code"),
            "message should name the binary: {err}"
        );
    }
}
