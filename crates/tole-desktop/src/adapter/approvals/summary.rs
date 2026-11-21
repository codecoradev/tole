//! The action summary of #1041: the client's type to the model's mirror.

use tole_client::ActionSummary as Wire;
use tole_desktop_model::approvals::ActionSummary;

/// The summary field for field. `kind` and `effect` stay the server's codes (a
/// code this version does not know is kept: the model words it as the unknown
/// tool). `inside_workspace` defaults to `false` on the wire when absent, which
/// is the careful reading ("outside the folder"). Nothing is cut here.
pub fn summary(s: &Wire) -> ActionSummary {
    ActionSummary {
        kind: s.kind.clone(),
        target: s.target.clone(),
        effect: s.effect.clone(),
        inside_workspace: s.inside_workspace,
        target_truncated: s.target_truncated,
        preview: s.preview.clone(),
        preview_truncated: s.preview_truncated,
    }
}
