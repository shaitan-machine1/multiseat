//! Direct-access desired state, owned only by the coordinator thread.
//! A failed ACL write never resurrects a removed client lease.
use crate::acl::{AclOps, NodeIdentity};
use seat_udev::SeatName;
use seatd_server::ClientId;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

/// Canonical direct-access paths, including static seat0 nodes. The topology
/// owner must exclude brokered devices before supplying this inventory.
pub type DirectNodes = BTreeMap<SeatName, BTreeSet<PathBuf>>;

#[derive(Clone)]
struct Lease {
    seat: SeatName,
    uid: u32,
}

pub struct AccessManager<A: AclOps> {
    acl: A,
    leases: BTreeMap<ClientId, Lease>,
    references: BTreeMap<(SeatName, u32), usize>,
    nodes: BTreeMap<PathBuf, SeatName>,
    managed: BTreeSet<PathBuf>,
    pending_clears: BTreeSet<PathBuf>,
    applied: BTreeMap<PathBuf, BTreeSet<u32>>,
    identities: BTreeMap<PathBuf, NodeIdentity>,
}

impl<A: AclOps> AccessManager<A> {
    /// Clear stale named-user ACLs before seats can be published. No persisted
    /// client or grant inventory is loaded because no connection survives restart.
    pub fn new(acl: A, nodes: DirectNodes) -> io::Result<Self> {
        let mut manager = Self {
            acl,
            leases: BTreeMap::new(),
            references: BTreeMap::new(),
            nodes: BTreeMap::new(),
            managed: BTreeSet::new(),
            pending_clears: BTreeSet::new(),
            applied: BTreeMap::new(),
            identities: BTreeMap::new(),
        };
        manager.set_nodes(nodes)?;
        Ok(manager)
    }

    /// Retain desired topology even if convergence fails so it can be retried.
    /// Clear old ownership before granting any new-seat user on a moved path.
    pub fn set_nodes(&mut self, nodes: DirectNodes) -> io::Result<()> {
        let mut wanted = BTreeMap::new();
        for (seat, paths) in nodes {
            for path in paths {
                if wanted.insert(path, seat.clone()).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "direct-access path belongs to multiple seats",
                    ));
                }
            }
        }
        for (path, seat) in &self.nodes {
            if wanted.get(path) != Some(seat) {
                self.pending_clears.insert(path.clone());
            }
        }
        self.managed.extend(wanted.keys().cloned());
        self.nodes = wanted;
        self.reconcile()
    }

    pub fn acquire(&mut self, client: ClientId, seat: SeatName, uid: u32) -> io::Result<()> {
        if self.leases.contains_key(&client) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "client already has an access lease",
            ));
        }
        self.leases.insert(
            client.clone(),
            Lease {
                seat: seat.clone(),
                uid,
            },
        );
        *self.references.entry((seat, uid)).or_default() += 1;
        if let Err(error) = self.reconcile() {
            self.remove_lease(&client);
            if let Err(rollback) = self.reconcile() {
                tracing::error!(%client, %rollback, "ACL lease creation rollback needs retry");
            }
            return Err(error);
        }
        Ok(())
    }

    /// Idempotent and irreversible even when ACL convergence fails.
    pub fn revoke(&mut self, client: &ClientId) -> io::Result<()> {
        self.remove_lease(client);
        self.reconcile()
    }

    pub fn revoke_seat(&mut self, seat: &SeatName) -> io::Result<()> {
        let clients = self
            .leases
            .iter()
            .filter(|(_, lease)| &lease.seat == seat)
            .map(|(client, _)| client.clone())
            .collect::<Vec<_>>();
        for client in clients {
            self.remove_lease(&client);
        }
        self.reconcile()
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.leases.clear();
        self.references.clear();
        self.reconcile()
    }

    pub fn reconcile(&mut self) -> io::Result<()> {
        let mut first_error = None;
        for path in self.pending_clears.clone() {
            match self.apply(&path, &BTreeSet::new()) {
                Ok(()) => {
                    self.pending_clears.remove(&path);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        // A failed old-seat clear blocks additions everywhere, but must not
        // prevent attempting UID removal on unrelated nodes.
        let additions_blocked = first_error.is_some();
        let mut forgotten = Vec::new();
        for path in self.managed.clone() {
            if self.pending_clears.contains(&path) {
                continue;
            }
            let users = self
                .nodes
                .get(&path)
                .map(|seat| self.users(seat))
                .unwrap_or_default();
            if additions_blocked
                && !self
                    .applied
                    .get(&path)
                    .is_some_and(|old| users.is_subset(old))
                && !users.is_empty()
            {
                continue;
            }
            match self.apply(&path, &users) {
                Ok(()) if !self.nodes.contains_key(&path) => forgotten.push(path),
                Ok(()) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        for path in forgotten {
            self.managed.remove(&path);
            self.applied.remove(&path);
            self.identities.remove(&path);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn users(&self, seat: &SeatName) -> BTreeSet<u32> {
        self.references
            .keys()
            .filter(|(owner, _)| owner == seat)
            .map(|(_, uid)| *uid)
            .collect()
    }

    fn apply(&mut self, path: &PathBuf, users: &BTreeSet<u32>) -> io::Result<()> {
        // A pathname/UID success cache cannot prove the current inode or ACL
        // is unchanged. Revalidate on topology events AND periodic retries.
        // Keep the last successful UID set only for the failed-clear barrier.
        let clearing_old = self.pending_clears.contains(path) || !self.nodes.contains_key(path);
        let expected = clearing_old
            .then(|| self.identities.get(path).copied())
            .flatten();
        match self.acl.apply_users(path, users, expected) {
            Ok(identity) => {
                if let Some(identity) = identity {
                    self.identities.insert(path.clone(), identity);
                } else {
                    self.identities.remove(path);
                }
                self.applied.insert(path.clone(), users.clone());
                Ok(())
            }
            Err(failure) => {
                // A backend failure may have changed the pinned inode's ACL.
                // Retain its identity even though successful UID state is lost.
                if let Some(identity) = failure.identity {
                    self.identities.insert(path.clone(), identity);
                }
                let error = failure.error;
                if error.kind() == io::ErrorKind::NotFound && !self.nodes.contains_key(path) {
                    self.applied.insert(path.clone(), BTreeSet::new());
                    return Ok(());
                }
                self.applied.remove(path);
                tracing::error!(path = %path.display(), %error, "direct-access ACL convergence failed");
                Err(error)
            }
        }
    }

    fn remove_lease(&mut self, client: &ClientId) {
        if let Some(lease) = self.leases.remove(client) {
            let key = (lease.seat, lease.uid);
            let count = self
                .references
                .get_mut(&key)
                .expect("lease has a UID reference");
            *count -= 1;
            if *count == 0 {
                self.references.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeState {
        applied: BTreeMap<PathBuf, BTreeSet<u32>>,
        calls: Vec<(PathBuf, BTreeSet<u32>)>,
        fail: BTreeSet<PathBuf>,
    }

    #[derive(Clone, Default)]
    struct FakeAcl(Rc<RefCell<FakeState>>);

    impl AclOps for FakeAcl {
        fn set_users(&self, path: &Path, users: &BTreeSet<u32>) -> io::Result<()> {
            let mut state = self.0.borrow_mut();
            state.calls.push((path.into(), users.clone()));
            // Deliberately modify state even on error to exercise rollback.
            state.applied.insert(path.into(), users.clone());
            if state.fail.contains(path) {
                Err(io::Error::other("fake ACL failure"))
            } else {
                Ok(())
            }
        }
    }

    fn seat(name: &str) -> SeatName {
        SeatName::parse(name).unwrap()
    }
    fn client(name: &str) -> ClientId {
        ClientId::new(name)
    }
    fn nodes(entries: &[(&str, &[&str])]) -> DirectNodes {
        entries
            .iter()
            .map(|(name, paths)| (seat(name), paths.iter().map(PathBuf::from).collect()))
            .collect()
    }
    fn users(acl: &FakeAcl, path: &str) -> BTreeSet<u32> {
        acl.0.borrow().applied[Path::new(path)].clone()
    }

    #[test]
    fn same_path_replacement_and_external_acl_reset_are_repaired() {
        use crate::acl::LibAcl;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("node");
        std::fs::write(&path, b"").unwrap();
        let inventory = BTreeMap::from([(seat("seat1"), BTreeSet::from([path.clone()]))]);
        let mut access = AccessManager::new(LibAcl, inventory.clone()).unwrap();
        access
            .acquire(client("lease"), seat("seat1"), 424242)
            .unwrap();
        let first = access.identities[&path];
        // Simulate a remove/add batch whose final pathname inventory is equal.
        std::fs::rename(&path, directory.path().join("old")).unwrap();
        std::fs::write(&path, b"").unwrap();
        access.set_nodes(inventory).unwrap();
        assert_ne!(access.identities[&path], first);
        assert!(crate::acl::test_acl_text(&path).contains("user:424242:rw-"));
        LibAcl.set_users(&path, &BTreeSet::new()).unwrap();
        access.reconcile().unwrap();
        assert!(crate::acl::test_acl_text(&path).contains("user:424242:rw-"));
        access.revoke(&client("lease")).unwrap();
        assert!(!crate::acl::test_acl_text(&path).contains("user:424242:"));
    }

    #[test]
    fn failed_write_to_replacement_retains_its_identity_for_cleanup() {
        use crate::acl::{ApplyFailure, LibAcl};
        #[derive(Clone)]
        struct UncertainAcl(Rc<RefCell<bool>>);
        impl AclOps for UncertainAcl {
            fn set_users(&self, path: &Path, users: &BTreeSet<u32>) -> io::Result<()> {
                self.apply_users(path, users, None)
                    .map(|_| ())
                    .map_err(|failure| failure.error)
            }
            fn apply_users(
                &self,
                path: &Path,
                users: &BTreeSet<u32>,
                expected: Option<NodeIdentity>,
            ) -> Result<Option<NodeIdentity>, ApplyFailure> {
                let identity = LibAcl.apply_users(path, users, expected)?;
                if *self.0.borrow() {
                    Err(ApplyFailure {
                        identity,
                        error: io::Error::other("uncertain write"),
                    })
                } else {
                    Ok(identity)
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("node");
        std::fs::write(&path, b"").unwrap();
        let fail = Rc::new(RefCell::new(false));
        let mut access = AccessManager::new(
            UncertainAcl(fail.clone()),
            BTreeMap::from([(seat("seat1"), BTreeSet::from([path.clone()]))]),
        )
        .unwrap();
        access
            .acquire(client("original"), seat("seat1"), 424242)
            .unwrap();
        let old = access.identities[&path];
        std::fs::rename(&path, directory.path().join("old")).unwrap();
        std::fs::write(&path, b"").unwrap();
        *fail.borrow_mut() = true;
        assert!(
            access
                .acquire(client("rejected"), seat("seat1"), 424243)
                .is_err()
        );
        assert_ne!(access.identities[&path], old);
        assert!(crate::acl::test_acl_text(&path).contains("user:424242:rw-"));
        *fail.borrow_mut() = false;
        access.set_nodes(BTreeMap::new()).unwrap();
        assert!(!crate::acl::test_acl_text(&path).contains("user:424242:"));
        assert!(!access.leases.contains_key(&client("rejected")));
    }

    #[test]
    fn startup_clears_stale_users_and_failure_prevents_initialization() {
        let acl = FakeAcl::default();
        acl.0
            .borrow_mut()
            .applied
            .insert("/direct".into(), BTreeSet::from([999]));
        let inventory = nodes(&[("seat0", &["/direct", "/static"])]);
        AccessManager::new(acl.clone(), inventory.clone()).unwrap();
        assert!(users(&acl, "/direct").is_empty());
        assert!(users(&acl, "/static").is_empty());
        acl.0.borrow_mut().fail.insert("/static".into());
        assert!(AccessManager::new(acl, inventory).is_err());
    }

    #[test]
    fn duplicate_uids_reference_count_and_hotplug_uses_the_complete_union() {
        let acl = FakeAcl::default();
        let mut access = AccessManager::new(acl.clone(), nodes(&[("seat1", &["/a"])])).unwrap();
        for (name, uid) in [("a", 1000), ("b", 1000), ("c", 1001)] {
            access.acquire(client(name), seat("seat1"), uid).unwrap();
        }
        assert_eq!(users(&acl, "/a"), BTreeSet::from([1000, 1001]));
        access.revoke(&client("a")).unwrap();
        assert_eq!(users(&acl, "/a"), BTreeSet::from([1000, 1001]));
        access
            .set_nodes(nodes(&[
                ("seat1", &["/a", "/hot"]),
                ("seat0", &["/static"]),
            ]))
            .unwrap();
        assert_eq!(users(&acl, "/hot"), BTreeSet::from([1000, 1001]));
        assert!(users(&acl, "/static").is_empty());
        access.revoke(&client("b")).unwrap();
        assert_eq!(users(&acl, "/a"), BTreeSet::from([1001]));
        access.revoke_seat(&seat("seat1")).unwrap();
        assert!(users(&acl, "/hot").is_empty());
    }

    #[test]
    fn duplicate_client_and_ambiguous_topology_do_not_mutate_live_leases() {
        let acl = FakeAcl::default();
        let mut access = AccessManager::new(acl.clone(), nodes(&[("seat1", &["/a"])])).unwrap();
        access
            .acquire(client("existing"), seat("seat1"), 1000)
            .unwrap();
        assert!(
            access
                .acquire(client("existing"), seat("seat1"), 1001)
                .is_err()
        );
        assert!(
            access
                .set_nodes(nodes(&[("seat1", &["/a"]), ("seat9", &["/a"])]))
                .is_err()
        );
        assert_eq!(users(&acl, "/a"), BTreeSet::from([1000]));
        access.revoke(&client("existing")).unwrap();
        assert!(access.references.is_empty());
        assert!(users(&acl, "/a").is_empty());
    }

    #[test]
    fn a_failed_move_does_not_block_unrelated_uid_removal() {
        let acl = FakeAcl::default();
        let mut access =
            AccessManager::new(acl.clone(), nodes(&[("seat9", &["/a", "/b"])])).unwrap();
        access
            .acquire(client("source"), seat("seat9"), 1000)
            .unwrap();
        access
            .acquire(client("target"), seat("seat1"), 1001)
            .unwrap();
        acl.0.borrow_mut().fail.insert("/a".into());
        assert!(
            access
                .set_nodes(nodes(&[("seat1", &["/a"]), ("seat9", &["/b"])]))
                .is_err()
        );
        assert!(access.revoke(&client("source")).is_err());
        assert!(users(&acl, "/b").is_empty());
        assert_eq!(access.leases.len(), 1);
        acl.0.borrow_mut().fail.clear();
        access.reconcile().unwrap();
        assert_eq!(users(&acl, "/a"), BTreeSet::from([1001]));
    }

    #[test]
    fn acquire_failure_rolls_back_all_paths_and_retains_retry_work() {
        let acl = FakeAcl::default();
        let mut access =
            AccessManager::new(acl.clone(), nodes(&[("seat1", &["/a", "/b"])])).unwrap();
        access
            .acquire(client("existing"), seat("seat1"), 1000)
            .unwrap();
        acl.0.borrow_mut().fail.insert("/b".into());
        assert!(
            access
                .acquire(client("failed"), seat("seat1"), 1001)
                .is_err()
        );
        assert!(!access.leases.contains_key(&client("failed")));
        assert_eq!(users(&acl, "/a"), BTreeSet::from([1000]));
        acl.0.borrow_mut().fail.clear();
        access.reconcile().unwrap();
        assert_eq!(users(&acl, "/b"), BTreeSet::from([1000]));
        assert_eq!(access.references.len(), 1);
    }

    #[test]
    fn revoke_failure_never_recreates_the_lease_and_attempts_other_nodes() {
        let acl = FakeAcl::default();
        let mut access =
            AccessManager::new(acl.clone(), nodes(&[("seat1", &["/a", "/b"])])).unwrap();
        access
            .acquire(client("client"), seat("seat1"), 1000)
            .unwrap();
        acl.0.borrow_mut().fail.insert("/a".into());
        assert!(access.revoke(&client("client")).is_err());
        assert!(access.leases.is_empty());
        assert!(access.references.is_empty());
        assert!(users(&acl, "/b").is_empty());
        acl.0.borrow_mut().fail.clear();
        access.revoke(&client("client")).unwrap();
        assert!(users(&acl, "/a").is_empty());
    }

    #[test]
    fn moves_clear_old_users_before_granting_new_users_and_retry_failed_clears() {
        let acl = FakeAcl::default();
        let mut access =
            AccessManager::new(acl.clone(), nodes(&[("seat9", &["/moving"])])).unwrap();
        access
            .acquire(client("source"), seat("seat9"), 1000)
            .unwrap();
        access
            .acquire(client("target"), seat("seat1"), 1001)
            .unwrap();
        acl.0.borrow_mut().calls.clear();
        acl.0.borrow_mut().fail.insert("/moving".into());
        assert!(access.set_nodes(nodes(&[("seat1", &["/moving"])])).is_err());
        assert_eq!(
            acl.0.borrow().calls,
            [(PathBuf::from("/moving"), BTreeSet::new())]
        );
        acl.0.borrow_mut().fail.clear();
        acl.0.borrow_mut().calls.clear();
        access.reconcile().unwrap();
        assert_eq!(
            acl.0.borrow().calls,
            [
                (PathBuf::from("/moving"), BTreeSet::new()),
                (PathBuf::from("/moving"), BTreeSet::from([1001])),
            ]
        );
    }

    #[test]
    fn topology_removal_and_shutdown_clear_users_without_losing_failed_work() {
        let acl = FakeAcl::default();
        let mut access =
            AccessManager::new(acl.clone(), nodes(&[("seat1", &["/gone", "/kept"])])).unwrap();
        access.acquire(client("one"), seat("seat1"), 1000).unwrap();
        access.set_nodes(nodes(&[("seat1", &["/kept"])])).unwrap();
        assert!(users(&acl, "/gone").is_empty());
        assert!(!access.managed.contains(Path::new("/gone")));
        acl.0.borrow_mut().fail.insert("/kept".into());
        assert!(access.shutdown().is_err());
        assert!(access.leases.is_empty());
        acl.0.borrow_mut().fail.clear();
        access.reconcile().unwrap();
        assert!(users(&acl, "/kept").is_empty());
    }
}
