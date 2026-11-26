//! The MCP server specs of a session: the auto-detected presets merged
//! under the explicit `--mcp-server` flags.

use tole_cli::tool_roster::binary_available;

/// Auto-detected MCP presets: server specs attached with zero
/// configuration. Currently just the local `cora mcp` server when the
/// `cora` binary is on PATH — the full code-intel surface (callers,
/// impact, dead-code, review) without a manual flag.
#[cfg(feature = "mcp")]
pub(crate) fn auto_mcp_specs() -> Vec<String> {
    if binary_available("cora") {
        vec!["cora=cora mcp".to_string()]
    } else {
        Vec::new()
    }
}

/// The names of the connected servers a discussion's helper child starts with:
/// the `mcp_server` list of the config and, unless `no_auto_mcp` is set, the
/// auto presets (`auto` is injected like for [`merge_mcp_specs`]). A server
/// given only with the `--mcp-server` flag of this process is not one of them:
/// the helper is a separate `tole mission` that does not receive the flag.
#[cfg(feature = "mcp")]
pub(crate) fn helper_mcp_servers(
    cfg: Option<&tole_cli::config::Config>,
    auto: Vec<String>,
) -> Vec<String> {
    let explicit = cfg.and_then(|c| c.mcp_server.clone()).unwrap_or_default();
    let no_auto = cfg.and_then(|c| c.no_auto_mcp).unwrap_or(false);
    merge_mcp_specs(&explicit, no_auto, auto)
        .iter()
        .filter_map(|s| s.split('=').next())
        .map(str::to_string)
        .collect()
}

/// Merge explicit `--mcp-server` specs over the auto-detected presets.
/// An explicit spec with the same server name wins — the user said what
/// they meant. `auto` is injected so the merge is unit-testable without
/// depending on which binaries this machine happens to have.
#[cfg(feature = "mcp")]
pub(crate) fn merge_mcp_specs(
    explicit: &[String],
    no_auto_mcp: bool,
    auto: Vec<String>,
) -> Vec<String> {
    let mut specs = explicit.to_vec();
    if !no_auto_mcp {
        let explicit_names: Vec<&str> = explicit
            .iter()
            .map(|s| s.split('=').next().unwrap_or_default())
            .collect();
        for spec in auto {
            let name = spec.split('=').next().unwrap_or_default();
            if !explicit_names.contains(&name) {
                specs.push(spec);
            }
        }
    }
    specs
}

#[cfg(all(test, feature = "mcp"))]
mod mcp_preset_tests {
    use super::{helper_mcp_servers, merge_mcp_specs};

    #[test]
    fn auto_spec_appended_when_no_explicit() {
        let merged = merge_mcp_specs(&[], false, vec!["cora=cora mcp".into()]);
        assert_eq!(merged, vec!["cora=cora mcp".to_string()]);
    }

    #[test]
    fn explicit_same_name_wins_over_auto() {
        let merged = merge_mcp_specs(
            &["cora=/opt/other/cora mcp --strict".to_string()],
            false,
            vec!["cora=cora mcp".into()],
        );
        assert_eq!(
            merged,
            vec!["cora=/opt/other/cora mcp --strict".to_string()]
        );
    }

    #[test]
    fn unrelated_explicit_and_auto_coexist() {
        let merged = merge_mcp_specs(
            &["fs=npx -y fs-server".to_string()],
            false,
            vec!["cora=cora mcp".into()],
        );
        assert_eq!(merged, vec!["fs=npx -y fs-server", "cora=cora mcp"]);
    }

    #[test]
    fn no_auto_flag_drops_presets_keeps_explicit() {
        let merged = merge_mcp_specs(
            &["fs=npx -y fs-server".to_string()],
            true,
            vec!["cora=cora mcp".into()],
        );
        assert_eq!(merged, vec!["fs=npx -y fs-server".to_string()]);
    }

    #[test]
    fn helpers_get_the_config_servers_and_the_preset_never_the_flag() {
        let cfg = |src: &str| tole_cli::config::parse(src, std::path::Path::new("u.toml")).unwrap();
        let auto = || vec!["cora=cora mcp".to_string()];
        // No config at all: only the preset (the `--mcp-server` flag of this
        // process is not an input here).
        assert_eq!(helper_mcp_servers(None, auto()), ["cora"]);
        let c = cfg("mcp_server = [\"nginjen=nginjen-mcp --x\"]\n");
        assert_eq!(helper_mcp_servers(Some(&c), auto()), ["nginjen", "cora"]);
        let c = cfg("mcp_server = [\"nginjen=nginjen-mcp\"]\nno_auto_mcp = true\n");
        assert_eq!(helper_mcp_servers(Some(&c), auto()), ["nginjen"]);
    }

    #[test]
    fn malformed_explicit_spec_treated_as_its_own_name() {
        // No '=' at all: the whole string counts as the name, so the
        // preset still attaches (parse() will reject the malformed spec
        // downstream with its normal error — this merge never panics).
        let merged = merge_mcp_specs(&["not-a-spec".into()], false, vec!["cora=cora mcp".into()]);
        assert_eq!(merged, vec!["not-a-spec", "cora=cora mcp"]);
    }
}
