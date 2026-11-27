use super::*;

/// The session operations behind [`TuiHost::apply`]: plain methods, so the
/// session can be driven (and tested) without going through a command.
impl TuiSession {
    pub fn models(&self) -> Result<Vec<String>, String> {
        tole_core::openai::fetch_model_ids(&self.cfg.base_url, &self.cfg.api_key)
    }

    pub fn set_model(&self, id: &str) -> Result<(), String> {
        let mut g = self.lock();
        crate::session_settings::set_model(&mut g.storage, id)?;
        // the context window belongs to the model: look it up again
        g.window = None;
        Ok(())
    }

    pub fn set_effort(&self, level: Option<&str>) -> Result<(), String> {
        crate::session_settings::set_effort(&mut self.lock().storage, level)
    }

    /// The forced pass of #481: the next prompt compacts first (when
    /// compaction is enabled). Not a mid-turn compaction.
    pub fn request_compaction(&self) -> Result<(), String> {
        crate::session_settings::request_compaction(&mut self.lock().storage)
    }

    pub fn new_session(&self) -> Result<String, String> {
        std::fs::create_dir_all(&self.sessions_dir).map_err(|e| e.to_string())?;
        let id = crate::session_host::new_session_id("s");
        let s = crate::session_host::create_session(
            &self.sessions_dir,
            &id,
            self.fresh_prompt.as_deref(),
            None,
            self.index.as_ref().map(|_| "tui"),
        )
        .map_err(|e| format!("creating session {id}: {e}"))?;
        Ok(self.swap(s))
    }

    pub fn sessions(&self) -> Result<Vec<SessionItem>, String> {
        let index = self.index.as_deref().ok_or("no session index")?;
        Ok(sessions_from(index, &self.session_id()))
    }

    pub fn switch_session(&self, id: &str) -> Result<String, String> {
        if !tole_core::storage::is_valid_session_id(id) {
            return Err(format!("invalid session id {id:?}"));
        }
        if self.session_id() == id {
            return Ok(id.to_string());
        }
        let file = self.session_file(id);
        if !file.is_file() {
            return Err(format!("session {id} not found"));
        }
        let s = JsonlStorage::open(&file).map_err(|e| format!("opening {id}: {e}"))?;
        Ok(self.swap(s))
    }

    /// Where session `id` lives: its index record, else the sessions dir.
    fn session_file(&self, id: &str) -> PathBuf {
        self.index
            .as_deref()
            .and_then(|i| crate::session_index::find(i, id))
            .map(|e| e.file)
            .unwrap_or_else(|| self.sessions_dir.join(format!("{id}.jsonl")))
    }

    /// `/fork [entry]` (#463, over #210): fork the CURRENT session at the
    /// settled entry `at` (default: the last settled one, as `tole branch`
    /// does) into a new session in the sessions dir, and switch to it.
    ///
    /// Safe while this process holds the source's writer lock:
    /// `JsonlStorage::fork` only replays the source read-only (no lock, no
    /// repair, never a write) and the new file takes its own sidecar lock,
    /// so there is no self-lock (#475) and the source is not touched. The
    /// turn lock is held across the whole operation, so no turn appends to
    /// the source while it is read. An unknown or unsettled entry, or a
    /// source with nothing settled yet, is refused with the core's error and
    /// the session stays as it was (a failed fork leaves no file behind).
    pub fn fork_session(&self, at: Option<&str>) -> Result<String, String> {
        let g = self.lock();
        let src_id = g.storage.session_id().to_string();
        let src = self.session_file(&src_id);
        std::fs::create_dir_all(&self.sessions_dir).map_err(|e| e.to_string())?;
        let id = crate::session_host::new_session_id("s");
        let s = JsonlStorage::fork(&src, &self.sessions_dir, &id, at)
            .map_err(|e| format!("forking {src_id}: {e}"))?;
        if self.index.is_some() {
            crate::session_index::record(&self.sessions_dir, &id, "branch");
        }
        drop(g);
        Ok(self.swap(s))
    }

    /// `/tree` (#463): the current session's family (its parent chain up to
    /// the root and every fork below it), one picker row per session,
    /// indented by depth with the open one marked. Built from the same
    /// `forked_from` data as `sessions --tree`, over the sessions dir; the
    /// open session is read from the live handle, not its file.
    pub fn tree(&self) -> Result<Vec<SessionItem>, String> {
        let me = {
            let g = self.lock();
            crate::session_index::TreeNode::of(g.storage.session_id(), &g.storage)
        };
        let current = me.id.clone();
        let mut nodes: Vec<_> = crate::session_index::tree_nodes(&self.sessions_dir)
            .into_iter()
            .filter(|n| n.id != current)
            .collect();
        nodes.push(me);
        Ok(tree_items(nodes, &current))
    }

    pub fn status_lines(&self) -> Vec<String> {
        let mut g = self.lock();
        let Inner {
            storage, window, ..
        } = &mut *g;
        let model = crate::turn_runner::session_model(&*storage, &self.cfg.model);
        let mut src = CachedWindow {
            catalog: ModelsCatalog::new(&self.cfg, &model),
            slot: window,
        };
        let mut out = vec![format!("session: {}", storage.session_id())];
        out.extend(
            session_status(&*storage, Some(&self.cfg.model), &self.compaction, &mut src).lines(),
        );
        out
    }

    /// `/copy` (#463): the last assistant answer of the open session.
    pub fn last_answer(&self) -> Option<String> {
        crate::export::last_answer(self.lock().storage.entries())
    }

    /// `/export [path]` (#463): write the open session's transcript under
    /// the workspace (jailed, never overwriting); returns the path written.
    /// Reads the in-memory replay; the session file is not touched.
    pub fn export(&self, path: Option<&str>) -> Result<String, String> {
        let g = self.lock();
        crate::export::export(
            g.storage.entries(),
            g.storage.session_id(),
            &self.workspace,
            path,
        )
        .map(|p| p.display().to_string())
    }

    pub fn set_name(&self, name: &str) -> Result<(), String> {
        crate::session_settings::set_name(&mut self.lock().storage, name)
    }

    /// Queue an image for the NEXT user message (#559): `spec` is a path
    /// inside the workspace jail or an inline `data:image/…;base64,…` URL
    /// (spilled to the data dir), exactly like `--image`. It is resolved
    /// NOW, so a bad path is refused at attach time, and the reference
    /// (never the blob) is what the next turn records. Returns the resolved
    /// reference for the UI to show.
    pub fn attach_image(&self, spec: &str) -> Result<ImageRef, String> {
        let r = crate::turn_runner::resolve_image(spec)?;
        self.attach_image_ref(r.clone());
        Ok(r)
    }

    /// Queue an already-resolved reference for the next user message.
    pub fn attach_image_ref(&self, image: ImageRef) {
        self.lock().pending_images.push(image);
    }

    /// The images the next user message will carry, in attach order.
    pub fn pending_images(&self) -> Vec<ImageRef> {
        self.lock().pending_images.clone()
    }

    /// The pending images as the UI shows them.
    pub(super) fn image_items(&self) -> Vec<tole_tui::ImageItem> {
        self.pending_images()
            .into_iter()
            .map(|r| tole_tui::ImageItem {
                path: r.path,
                mime: r.mime,
                bytes: r.bytes,
            })
            .collect()
    }

    /// Drop every queued image; returns how many were queued.
    pub fn clear_pending_images(&self) -> usize {
        std::mem::take(&mut self.lock().pending_images).len()
    }
}
