//! The one unit test of `config::apply` that reads its private `*_ENV` tables.
//! The rest moved to `tests/config_apply_unit/` (#1249).

use super::*;

#[test]
fn provider_env_prefix_table_matches_core() {
    assert_eq!(tole_core::openai::ENV_PREFIXES, ["TOLE", "OPENAI"]);
    for (i, p) in tole_core::openai::ENV_PREFIXES.iter().enumerate() {
        assert_eq!(BASE_URL_ENV[i], format!("{p}_BASE_URL"));
        assert_eq!(MODEL_ENV[i], format!("{p}_MODEL"));
        assert_eq!(API_KEY_ENV[i], format!("{p}_API_KEY"));
    }
}
