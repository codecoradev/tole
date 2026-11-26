//! Tests of the registry builders.

use super::*;

#[cfg(all(test, feature = "shell-tools"))]
mod gh_repo_tests {
    use super::*;

    #[test]
    fn parses_https_ssh_and_git_suffix() {
        assert_eq!(
            tole_cli::tool_roster::github_repo_from_remote_url("https://github.com/foo/bar.git")
                .as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            tole_cli::tool_roster::github_repo_from_remote_url("https://github.com/foo/bar")
                .as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            tole_cli::tool_roster::github_repo_from_remote_url("git@github.com:foo/bar.git")
                .as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            tole_cli::tool_roster::github_repo_from_remote_url(
                "https://user:token@github.com/Foo/Bar.git"
            )
            .as_deref(),
            Some("Foo/Bar")
        );
    }

    #[test]
    fn rejects_non_github_and_garbage() {
        assert!(tole_cli::tool_roster::github_repo_from_remote_url(
            "https://gitlab.com/foo/bar.git"
        )
        .is_none());
        assert!(tole_cli::tool_roster::github_repo_from_remote_url(
            "https://github.com/only-owner"
        )
        .is_none());
        assert!(tole_cli::tool_roster::github_repo_from_remote_url("not a url").is_none());
        assert!(
            tole_cli::tool_roster::github_repo_from_remote_url("https://github.com/-bad/name")
                .is_none()
        );
    }

    /// #352: only a host that is EXACTLY github.com counts; the string
    /// "github.com" inside a longer hostname, userinfo or the path must not
    /// redirect the gh tool to an attacker-named repo.
    #[test]
    fn host_must_be_exactly_github_com() {
        let parse = tole_cli::tool_roster::github_repo_from_remote_url;
        for bad in [
            "https://notgithub.com/owner/name",
            "https://github.com.evil.io/owner/name",
            "ssh://git@github.com.evil.io/o/r",
            "https://evil.io/github.com/owner/name",
            "https://gitea.example/github.com/owner/name.git",
            "https://github.com@evil.io/owner/name",
            "https://user:github.com@evil.io/owner/name",
            "ssh://git@evil.io:github.com/o/r",
            "git@evil.io:github.com/o/r",
            "git@github.com.evil.io:o/r",
            "evil.io:github.com:o/r",
            "https://githubxcom/owner/name",
            "https://gist.github.com/owner/name",
            "github.com/owner/name",
        ] {
            assert!(parse(bad).is_none(), "{bad} must not resolve to a repo");
        }
    }

    #[test]
    fn accepts_the_real_github_forms() {
        let parse = tole_cli::tool_roster::github_repo_from_remote_url;
        for (url, want) in [
            ("https://github.com/o/r", "o/r"),
            ("https://github.com/o/r/", "o/r"),
            ("https://GitHub.COM/o/r.git", "o/r"),
            ("https://github.com:443/o/r.git", "o/r"),
            ("https://user:tok@github.com/o/r.git", "o/r"),
            ("ssh://git@github.com/o/r.git", "o/r"),
            ("ssh://git@github.com:22/o/r.git", "o/r"),
            ("git@github.com:o/r.git", "o/r"),
            ("github.com:o/r", "o/r"),
            ("git://github.com/o/r.git", "o/r"),
        ] {
            assert_eq!(parse(url).as_deref(), Some(want), "{url}");
        }
    }

    #[test]
    fn gh_tool_not_registered_without_detected_repo() {
        // Issue #293: a non-GitHub cwd must yield NO gh tool (no
        // hardcoded fallback repo).
        let dir = crate::test_dir::TestDir::new("tole-gh-none");
        let reg = tool_roster::build(
            tole_core::approval::AllowlistApprover::allow_only(vec![]),
            tool_roster::Roster::new(dir.to_path_buf()),
        )
        .unwrap();
        assert!(reg.get("gh").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_repo_from_checkout() {
        // A real checkout: init + remote origin, then detect.
        let dir = crate::test_dir::TestDir::new("tole-gh-detect");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/detected/owner-name.git",
        ]);
        let detected = tole_cli::tool_roster::detect_github_repo(&dir);
        assert_eq!(detected.as_deref(), Some("detected/owner-name"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod chat_todo_tests {
    use super::*;

    /// Issue #276 regression: the chat face registers both todo tools
    /// (and only todo_read in plan mode).
    #[test]
    fn chat_registers_todo_tools_and_respects_plan_mode() {
        let mut reg =
            ToolRegistry::with_approver(tole_core::approval::AllowlistApprover::allow_only(vec![]));
        tool_roster::register_todo(&mut reg, &[], false).unwrap();
        assert!(reg.get("todo_read").is_some());
        assert!(reg.get("todo_write").is_some());
        let mut plan = ToolRegistry::new();
        tool_roster::register_todo(&mut plan, &[], true).unwrap();
        assert!(plan.get("todo_read").is_some());
        assert!(plan.get("todo_write").is_none());
    }

    /// Issue #294 regression: the server-level registry of the MCP HTTP
    /// face must be read-only under plan mode (absent, not just denied).
    #[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
    #[test]
    fn mcp_server_registry_is_read_only_under_plan_mode() {
        use tole_core::tool::Risk;
        let normal = build_server_registry_for_mcp(None, false).unwrap();
        assert!(
            normal.specs().iter().any(|s| normal
                .get(s["function"]["name"].as_str().unwrap())
                .unwrap()
                .risk()
                != Risk::ReadOnly),
            "non-plan registry must expose write tools"
        );
        let plan = build_server_registry_for_mcp(None, true).unwrap();
        let specs = plan.specs();
        assert!(!specs.is_empty());
        for spec in &specs {
            let name = spec["function"]["name"].as_str().unwrap();
            assert_eq!(
                plan.get(name).unwrap().risk(),
                Risk::ReadOnly,
                "{name} must not survive plan mode"
            );
        }
    }

    /// cora 0.19 scan MAJOR: the MCP HTTP face's server-level file tools
    /// must be rooted at `--workspace`, not the process cwd.
    #[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
    #[test]
    fn mcp_server_registry_file_tools_are_rooted_at_workspace() {
        let ws = crate::test_dir::TestDir::new("tole-mcp-ws");
        std::fs::write(ws.join("only-in-ws.txt"), "rooted").unwrap();
        let ws_arg = ws.to_str().unwrap().to_string();
        let reg = build_server_registry_for_mcp(Some(&ws_arg), false).unwrap();
        let out = reg
            .get("read_file")
            .unwrap()
            .execute(serde_json::json!({ "path": "only-in-ws.txt" }))
            .unwrap();
        assert_eq!(out["content"], serde_json::json!("rooted"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
