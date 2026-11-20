//! The identity of a sandbox profile as the plugin lock records it (#750,
//! design `sandbox-design.md` §6.1): its name and a hash of what it grants
//! after path resolution. An approval binds both, so a profile edited in
//! the user config (a wider network, another read or write grant, a path
//! that now resolves elsewhere) is not the one the owner approved.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

use super::{Network, Policy, Profile, Workspace};

/// A profile's name and [`Profile::hash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileId {
    pub name: String,
    pub hash: String,
}

impl Profile {
    /// `sha256:<hex>` of the effective grants: workspace mode, network and
    /// the extra read and write paths, each path resolved the way a launch
    /// resolves it (symlinks followed as far as the path exists), sorted and
    /// de-duplicated, every field length-prefixed. Pure but for reading the
    /// file system to resolve links. `protect` only narrows a writable
    /// workspace and is not a grant, so it is not part of the hash.
    pub fn hash(&self) -> String {
        let workspace = match self.workspace {
            Workspace::None => "none",
            Workspace::Read => "read",
            Workspace::Write => "write",
        };
        let network = match self.network {
            Network::None => "none",
            Network::Host => "host",
        };
        let resolved = |v: &[PathBuf]| -> Vec<String> {
            let mut out: Vec<String> = v
                .iter()
                .map(|p| {
                    crate::paths::canon_lenient(p)
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            out.sort();
            out.dedup();
            out
        };
        let mut h = Sha256::new();
        h.update(b"tole-sandbox-profile-v1\0");
        let mut field = |bytes: &[u8]| {
            h.update((bytes.len() as u64).to_be_bytes());
            h.update(bytes);
        };
        field(workspace.as_bytes());
        field(network.as_bytes());
        for set in [resolved(&self.read), resolved(&self.write)] {
            field(&(set.len() as u64).to_be_bytes());
            for p in &set {
                field(p.as_bytes());
            }
        }
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }
}

impl Policy {
    /// The identity of the profile CONFIGURED for `server`, or `None` when
    /// none applies: the sandbox is off or the owner skips it. It says
    /// nothing about whether a sandbox can actually be applied; approval and
    /// admission use [`Policy::applied_profile`].
    pub fn profile_id(&self, server: &str) -> Option<ProfileId> {
        if self.mode == super::Mode::Off || self.skip.iter().any(|s| s == server) {
            return None;
        }
        let (name, profile) = self.profile_for(server);
        Some(ProfileId {
            name: name.to_string(),
            hash: profile.hash(),
        })
    }
}

impl Policy {
    /// The identity of the profile `server` actually runs under: as
    /// [`Policy::profile_id`], and `None` unless `avail` is a usable backend.
    /// With no working backend a server in mode `optional` runs unwrapped, so
    /// no profile applies to it, and an approval records (and admission
    /// compares) exactly that. The one helper approval, admission, `list`
    /// and `diff` share, so they cannot disagree. (Whether the server is a
    /// stdio one is the caller's to say.)
    pub fn applied_profile(&self, server: &str, avail: &super::Availability) -> Option<ProfileId> {
        matches!(avail, super::Availability::Usable { .. })
            .then(|| self.profile_id(server))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::builtin_profile;
    use crate::test_dir::TestDir;

    #[test]
    fn the_hash_follows_every_grant_and_ignores_order_and_duplicates() {
        let dir = TestDir::new("tole-profile-hash");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let base = builtin_profile("server").unwrap();
        let h = |f: &dyn Fn(&mut Profile)| {
            let mut p = base.clone();
            f(&mut p);
            p.hash()
        };
        let plain = base.hash();
        assert!(plain.starts_with("sha256:") && plain.len() == 71);
        assert_ne!(plain, h(&|p| p.network = Network::Host));
        assert_ne!(plain, h(&|p| p.workspace = Workspace::Write));
        assert_ne!(plain, h(&|p| p.read.push(a.clone())));
        assert_ne!(plain, h(&|p| p.write.push(a.clone())));
        // The same path as a read or a write grant is not the same grant.
        assert_ne!(
            h(&|p| p.read.push(a.clone())),
            h(&|p| p.write.push(a.clone()))
        );
        // Order and duplicates do not matter.
        assert_eq!(
            h(&|p| p.read = vec![a.clone(), b.clone()]),
            h(&|p| p.read = vec![b.clone(), a.clone(), a.clone()])
        );
        // `protect` narrows and is not a grant.
        assert_eq!(plain, h(&|p| p.protect.push(a.clone())));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_now_points_elsewhere_changes_the_hash() {
        let dir = TestDir::new("tole-profile-link");
        let (a, b, link) = (dir.join("a"), dir.join("b"), dir.join("link"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let mut p = builtin_profile("server").unwrap();
        p.read = vec![link.clone()];
        let before = p.hash();
        // The resolved target is what is hashed ...
        let mut direct = p.clone();
        direct.read = vec![a.clone()];
        assert_eq!(before, direct.hash());
        // ... so a retargeted link is another grant.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert_ne!(before, p.hash());
    }

    #[test]
    fn a_policy_names_the_profile_a_server_runs_in_and_none_when_it_is_not_sandboxed() {
        let mut p = crate::sandbox::Policy {
            mode: crate::sandbox::Mode::Required,
            profile_name: "server".into(),
            profile: builtin_profile("server").unwrap(),
            servers: Default::default(),
            skip: vec!["mine".into()],
            max_servers: 4,
            idle_secs: 60,
        };
        let strict = crate::sandbox::ServerProfile {
            name: "strict".into(),
            profile: builtin_profile("strict").unwrap(),
        };
        p.servers.insert("tight".into(), strict.clone());
        let id = p.profile_id("any").unwrap();
        assert_eq!(id.name, "server");
        assert_eq!(id.hash, p.profile.hash());
        assert_eq!(p.profile_id("tight").unwrap().hash, strict.profile.hash());
        assert!(p.profile_id("mine").is_none());
        p.mode = crate::sandbox::Mode::Off;
        assert!(p.profile_id("any").is_none());
    }
}
