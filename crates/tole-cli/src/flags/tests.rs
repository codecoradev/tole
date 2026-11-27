//! Tests of the trust-preset, serve-URL and server-face flag helpers.

use super::*;
use crate::{Cli, Command};
use clap::Parser;
use std::path::Path;
use tole_cli::trust::expand_trust;

#[cfg(test)]
mod trust_preset_tests {
    use super::*;

    #[test]
    fn internal_expands_to_expected_patterns() {
        let pats = expand_trust(&["internal".to_string()]).unwrap();
        for want in [
            "uteke_*",
            "cora_search",
            "mcp_cora_*",
            "verify_package",
            "job_*",
            "agent_poll",
            "tole_session_*",
        ] {
            assert!(
                pats.iter().any(|p| p == want),
                "internal missing {want}: {pats:?}"
            );
        }
        // must NOT include write-capable native tools
        assert!(!pats.iter().any(|p| p == "write_file"));
        assert!(!pats.iter().any(|p| p == "run_command"));
        // agent_poll (Write since #300) is allowlisted by exact name so the
        // poll loop stays unattended; agent_start must keep prompting.
        assert!(!pats.iter().any(|p| p == "agent_*" || p == "agent_start"));
    }

    #[test]
    fn workspace_is_read_only_plus_the_jailed_file_writers() {
        let ws = expand_trust(&["workspace".to_string()]).unwrap();
        let ro = expand_trust(&["read_only".to_string()]).unwrap();
        for p in &ro {
            assert!(ws.contains(p), "workspace lacks read_only's {p}");
        }
        // The writers only through the path guard, never bare.
        assert!(ws.iter().any(|p| p == "write_file@workspace"));
        assert!(ws.iter().any(|p| p == "edit_file@workspace"));
        for never in [
            "write_file",
            "edit_file",
            "delete_file",
            "run_command",
            "git",
            "gh",
            "agent_start",
        ] {
            assert!(!ws.iter().any(|p| p == never), "{never}: {ws:?}");
        }
        assert!(!ws.iter().any(|p| p.contains('*')), "{ws:?}");
    }

    #[test]
    fn workspace_preset_refuses_home_and_filesystem_root() {
        use tole_cli::trust::check_workspace_root;
        let ws = vec!["workspace".to_string()];
        let home = crate::test_dir::TestDir::new("tole-ws-home");
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert!(check_workspace_root(&ws, Some(&project), Some(&home)).is_ok());
        let e = check_workspace_root(&ws, Some(&home), Some(&home)).unwrap_err();
        assert!(
            e.to_string().contains("your home or a filesystem root"),
            "{e}"
        );
        assert!(check_workspace_root(&ws, Some(Path::new("/")), None).is_err());
        // The implicit cwd is never enough.
        let e = check_workspace_root(&ws, None, Some(&home)).unwrap_err();
        assert!(e.to_string().contains("needs an explicit workspace"), "{e}");
        // Other presets never look at the root.
        assert!(check_workspace_root(&["read_only".into()], None, None).is_ok());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn none_expands_to_empty() {
        assert!(expand_trust(&["none".to_string()]).unwrap().is_empty());
        assert!(expand_trust(&[]).unwrap().is_empty());
    }

    #[test]
    fn unknown_preset_is_a_loud_error() {
        let err = expand_trust(&["internalx".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown trust preset"));
        assert!(err.to_string().contains("internal"));
    }

    #[test]
    fn case_insensitive_and_union() {
        let pats = expand_trust(&["INTERNAL".to_string(), "read_only".to_string()]).unwrap();
        assert!(pats.iter().any(|p| p == "uteke_*"));
        assert!(pats.iter().any(|p| p == "read_file"));
    }

    #[test]
    fn with_trust_appends_preserving_user_patterns() {
        let out = with_trust(
            vec!["write_file".to_string()],
            &["uteke_*".to_string(), "cora_search".to_string()],
        );
        assert_eq!(out, vec!["write_file", "uteke_*", "cora_search"]);
    }

    #[test]
    fn trust_env_used_when_flag_absent() {
        let flags = resolve_trust_flags(&[], Some("internal".to_string()));
        assert_eq!(flags, vec!["internal"]);
        // commas and whitespace both separate presets
        let flags = resolve_trust_flags(&[], Some("internal, read_only".to_string()));
        assert_eq!(flags, vec!["internal", "read_only"]);
    }

    #[test]
    fn trust_flag_wins_over_env() {
        let flags = resolve_trust_flags(&["read_only".to_string()], Some("internal".to_string()));
        assert_eq!(flags, vec!["read_only"], "explicit flag replaces the env");
    }

    #[test]
    fn trust_env_end_to_end_expands() {
        // the dispatch path: env-only resolves, then expands
        let flags = resolve_trust_flags(&[], Some(" internal ".to_string()));
        let pats = expand_trust(&flags).unwrap();
        assert!(pats.iter().any(|p| p == "uteke_*"));
    }

    #[test]
    fn serve_url_flag_wins_over_env() {
        let u = resolve_serve_url(
            Some("http://flag:1".to_string()),
            Some("http://env:2".to_string()),
        );
        assert_eq!(u, "http://flag:1");
    }

    #[test]
    fn serve_url_env_wins_over_default() {
        let u = resolve_serve_url(None, Some("http://remote:9".to_string()));
        assert_eq!(u, "http://remote:9");
    }

    #[test]
    fn serve_url_empty_env_is_ignored() {
        assert_eq!(
            resolve_serve_url(None, Some(String::new())),
            DEFAULT_SERVE_URL
        );
    }

    #[test]
    fn serve_url_default_when_neither() {
        assert_eq!(resolve_serve_url(None, None), "http://127.0.0.1:7801");
    }

    #[test]
    fn approvals_parse_without_url_leaves_it_unset() {
        let cli = Cli::try_parse_from(["tole", "approvals", "list"]).unwrap();
        match cli.command {
            Command::Approvals { url, .. } => assert!(url.is_none()),
            _ => panic!("expected Approvals"),
        }
        let cli =
            Cli::try_parse_from(["tole", "approvals", "list", "--url", "http://x:1"]).unwrap();
        match cli.command {
            Command::Approvals { url, .. } => assert_eq!(url.as_deref(), Some("http://x:1")),
            _ => panic!("expected Approvals"),
        }
    }

    #[test]
    fn trust_env_typo_is_a_loud_error() {
        let flags = resolve_trust_flags(&[], Some("internalx".to_string()));
        let err = expand_trust(&flags).unwrap_err();
        assert!(err.to_string().contains("unknown trust preset"));
    }
}

#[cfg(all(test, feature = "mcp"))]
mod server_face_flag_tests {
    use super::*;

    #[test]
    fn clean_flags_pass() {
        assert!(check_client_session_flags("serve", &[], false, false).is_ok());
    }

    #[test]
    fn skill_flag_bails_loudly() {
        let err =
            check_client_session_flags("mcp", &[PathBuf::from("/tmp/SKILL.md")], false, false)
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--skill"), "{msg}");
        assert!(msg.contains("tole mcp"), "{msg}");
    }

    #[test]
    fn no_skills_flag_bails_loudly() {
        let err = check_client_session_flags("acp", &[], true, false).unwrap_err();
        assert!(err.to_string().contains("--no-skills"));
    }

    #[test]
    fn explicit_mcp_server_bails_loudly() {
        let err = check_client_session_flags("serve", &[], false, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--mcp-server"), "{msg}");
        assert!(msg.contains("tole serve"), "{msg}");
    }
}
