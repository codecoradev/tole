//! The Skills and Connected tools pages, one PNG per state (`skills-*.png`,
//! `tools-*.png`): the scenes of the model, each drawn as the window draws the
//! page the skills driver keeps. Compared with boards S7 and S8.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use tole_desktop_model::{skills_scenes, SkillsPane, SkillsScene};

use super::adapter::offscreen::Offscreen;
use super::harness::{Rendered, WindowSize};
use super::shell::ShellView;

/// The page of `scene` as the app would hand it over. While a call is out both
/// routes say so, which is what Refresh shows.
fn view_of(scene: &SkillsScene) -> SkillsPane {
    SkillsPane {
        page: scene.page.clone(),
        skills_loading: scene.call_out,
        tools_loading: scene.call_out,
    }
}

/// Renders one scene into `dir`.
fn render(scene: &SkillsScene, dir: &Path, window: WindowSize) -> Result<Rendered, String> {
    let at = |what: &str, err: String| format!("{}: {what}: {err}", scene.name);
    let (state, view) = (scene.shell(), view_of(scene));
    let mut offscreen = Offscreen::open(window.width, window.height, move |_cx| {
        ShellView::with_skills(state, view)
    })
    .map_err(|e| at("open", e))?;
    for key in scene.keys.split_whitespace() {
        offscreen.press(key).map_err(|e| at("press", e))?;
    }
    let path = dir.join(format!("{}.png", scene.name));
    let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
    let bytes = fs::metadata(&path)
        .map_err(|e| at("stat", e.to_string()))?
        .len();
    Ok(Rendered {
        name: scene.name,
        board: scene.board,
        keys: scene.keys,
        path,
        width,
        height,
        bytes,
        rgba,
    })
}

/// Renders every scene into `dir` (which must exist) and adds a line for each
/// to `index.tsv`, when there is one.
pub fn render_skills(dir: &Path, window: WindowSize) -> Result<Vec<Rendered>, String> {
    let mut done = Vec::new();
    for scene in skills_scenes() {
        done.push(render(&scene, dir, window)?);
    }
    let index = dir.join("index.tsv");
    if index.exists() {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .map_err(|e| format!("{}: {e}", index.display()))?;
        for r in &done {
            writeln!(
                file,
                "{}.png\t{}\t{}\t{}\t{}\t{}",
                r.name, r.board, r.keys, r.width, r.height, r.bytes
            )
            .map_err(|e| format!("{}: {e}", index.display()))?;
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::super::harness::focus_walk;
    use super::*;
    use std::collections::HashSet;
    use tole_desktop_model::tokens::{size, Rgb, DARK};
    use tole_desktop_model::{shell_captures, Badge, FocusTarget, Tab};

    /// GPUI's test platform draws at a fixed scale of 2.
    const SCALE: u32 = 2;

    fn rgb(c: Rgb) -> [u8; 4] {
        [c.r(), c.g(), c.b(), 255]
    }

    /// Whether a pixel of `colour` lies in the main area (right of the sidebar,
    /// below the header), where the page is; the sidebar's own amber note is
    /// outside it.
    fn page_has(r: &Rendered, colour: Rgb) -> bool {
        let (x0, y0) = (
            (size::SIDEBAR_WIDTH as u32 + 1) * SCALE,
            size::TOPBAR_HEIGHT as u32 * SCALE,
        );
        (y0..r.height).any(|y| (x0..r.width).any(|x| r.pixel(x, y) == rgb(colour)))
    }

    #[test]
    fn the_images_do_not_clash_with_the_other_captures() {
        let mine: HashSet<_> = skills_scenes().iter().map(|s| s.name).collect();
        for c in shell_captures() {
            assert!(!mine.contains(c.name), "{}", c.name);
        }
        assert!(mine.iter().all(|n| !n.starts_with("live-")));
    }

    #[test]
    fn the_pane_hands_over_its_page_and_the_call_out() {
        for scene in skills_scenes() {
            let v = view_of(&scene);
            assert_eq!(v.page, scene.page);
            assert_eq!(v.refreshing(), scene.call_out);
        }
        let empty = skills_scenes()
            .into_iter()
            .find(|s| s.name == "skills-tab-empty")
            .unwrap();
        assert_eq!(view_of(&empty).page.skills_waiting(), Badge::Count(0));
    }

    /// Tab walks the page's own controls after the sidebar, in the model's
    /// order: the two tabs, then Refresh, and wraps to the toggle.
    #[test]
    #[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
    fn tab_visits_the_tabs_and_refresh_after_the_sidebar() {
        let state = skills_scenes()[0].shell();
        let order = state.tab_order();
        let keys = vec!["tab"; order.len() + 1];
        let seen = focus_walk(state, &keys, WindowSize::default()).expect("walk");
        let seen: Vec<_> = seen.into_iter().map(|t| t.expect("a stop")).collect();
        assert_eq!(seen[..order.len()], order[..]);
        assert_eq!(seen[order.len()], order[0], "wraps");
        assert_eq!(
            order[order.len() - 3..],
            [
                FocusTarget::SkillsTab(Tab::Skills),
                FocusTarget::SkillsTab(Tab::ConnectedTools),
                FocusTarget::SkillsRefresh
            ]
        );
    }

    /// Each scene draws, and what stands out is what the page says: amber for a
    /// card that asks for care, red only for a failure or a refusal, green only
    /// for what is approved or trusted.
    #[test]
    #[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
    fn the_scenes_draw_the_colours_of_what_they_say() {
        let dir = std::env::temp_dir().join(format!("tole-skills-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Tall enough that the whole list is on screen: a group below the fold
        // would not be drawn at all.
        let tall = WindowSize {
            width: 1200.0,
            height: 2000.0,
        };
        let shots = render_skills(&dir, tall).expect("render");
        let by = |name: &str| shots.iter().find(|r| r.name == name).expect(name);
        let (amber, danger, good) = (DARK.amber, DARK.danger, DARK.ok);
        let review = by("skills-tab-review");
        assert!(page_has(review, amber) && page_has(review, good) && page_has(review, danger));
        let empty = by("skills-tab-empty");
        assert!(!page_has(empty, amber) && !page_has(empty, danger) && !page_has(empty, good));
        assert!(
            page_has(by("skills-tab-partial"), amber),
            "a partial read warns"
        );
        assert!(page_has(by("skills-tab-failed"), danger));
        assert!(!page_has(by("skills-tab-failed"), good));
        // Rows under a failed load are never drawn: no card colour at all.
        for name in ["skills-tab-failed-with-rows", "tools-tab-failed-with-rows"] {
            let r = by(name);
            assert!(page_has(r, danger), "{name} says it failed");
            assert!(
                !page_has(r, amber) && !page_has(r, good),
                "{name} drew rows"
            );
        }
        let reading = by("skills-tab-reading");
        assert!(!page_has(reading, good) && !page_has(reading, danger));
        let tools = by("tools-tab-table");
        assert!(page_has(tools, good) && page_has(tools, amber));
        assert!(!page_has(by("tools-tab-empty"), amber));
        assert!(page_has(by("tools-tab-partial"), amber));
        assert!(page_has(by("tools-tab-failed"), danger));
        assert!(
            page_has(by("skills-focus-refresh"), DARK.accent),
            "the focus ring"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
