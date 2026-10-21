use crate::config::SeatConfig;
use crate::service::{SeatInstance, SeatServiceManager, ServiceError};
use crate::topology::{Topology, TopologyError};
use seat_udev::SeatName;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Add,
    Remove,
    Replace,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Replace => "replace",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshChange {
    pub seat: SeatName,
    pub kind: ChangeKind,
    pub deployed: Option<SeatConfig>,
    pub desired: Option<SeatConfig>,
}

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Topology(#[from] TopologyError),
    #[error(transparent)]
    Service(#[from] ServiceError),
}

pub struct Orchestrator {
    topology: Box<dyn Topology>,
    services: Box<dyn SeatServiceManager>,
}

impl Orchestrator {
    pub fn new(topology: Box<dyn Topology>, services: Box<dyn SeatServiceManager>) -> Self {
        Self { topology, services }
    }

    pub fn plan(
        &self,
        selected: Option<&SeatName>,
    ) -> Result<Vec<RefreshChange>, OrchestratorError> {
        let desired = self.topology.snapshot()?;
        let deployed = self
            .services
            .list()?
            .into_iter()
            .map(|instance| (instance.config.name.clone(), instance))
            .collect::<BTreeMap<_, _>>();
        Ok(build_plan(&desired, &deployed, selected))
    }

    pub fn apply(&self, change: &RefreshChange) -> Result<(), OrchestratorError> {
        if change.deployed.is_some() {
            self.services.stop(&change.seat)?;
        }
        if let Some(config) = &change.desired {
            self.services.start(config)?;
        }
        Ok(())
    }

    pub fn ready_seats(&self) -> Result<Vec<SeatName>, OrchestratorError> {
        Ok(self
            .services
            .list()?
            .into_iter()
            .filter(|instance| instance.ready)
            .map(|instance| instance.config.name)
            .collect())
    }

    pub fn converge_startup(&self) -> Result<Vec<String>, OrchestratorError> {
        let changes = self.plan(None)?;
        let mut failures = Vec::new();
        for change in changes {
            if let Err(error) = self.apply(&change) {
                failures.push(format!("{}: {error}", change.seat));
            }
        }
        Ok(failures)
    }
}

fn build_plan(
    desired: &BTreeMap<SeatName, SeatConfig>,
    deployed: &BTreeMap<SeatName, SeatInstance>,
    selected: Option<&SeatName>,
) -> Vec<RefreshChange> {
    let mut seats = desired
        .keys()
        .chain(deployed.keys())
        .cloned()
        .collect::<Vec<_>>();
    seats.sort();
    seats.dedup();
    seats
        .into_iter()
        .filter(|seat| selected.is_none_or(|selected| selected == seat))
        .filter_map(|seat| {
            let wanted = desired.get(&seat).cloned();
            let current = deployed.get(&seat);
            match (current, wanted) {
                (None, Some(desired)) => Some(RefreshChange {
                    seat,
                    kind: ChangeKind::Add,
                    deployed: None,
                    desired: Some(desired),
                }),
                (Some(current), None) => Some(RefreshChange {
                    seat,
                    kind: ChangeKind::Remove,
                    deployed: Some(current.config.clone()),
                    desired: None,
                }),
                (Some(current), Some(desired)) if current.config != desired || !current.ready => {
                    Some(RefreshChange {
                        seat,
                        kind: ChangeKind::Replace,
                        deployed: Some(current.config.clone()),
                        desired: Some(desired),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ServiceError;
    use crate::topology::{Topology, TopologyError};
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeTopology(Arc<Mutex<BTreeMap<SeatName, SeatConfig>>>);

    impl Topology for FakeTopology {
        fn snapshot(&self) -> Result<BTreeMap<SeatName, SeatConfig>, TopologyError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[derive(Clone, Default)]
    struct FakeServices {
        instances: Arc<Mutex<BTreeMap<SeatName, SeatInstance>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl SeatServiceManager for FakeServices {
        fn list(&self) -> Result<Vec<SeatInstance>, ServiceError> {
            Ok(self.instances.lock().unwrap().values().cloned().collect())
        }

        fn start(&self, config: &SeatConfig) -> Result<(), ServiceError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start:{}", config.name));
            self.instances.lock().unwrap().insert(
                config.name.clone(),
                SeatInstance {
                    config: config.clone(),
                    service_dir: format!("/services/{}", config.name).into(),
                    ready: true,
                },
            );
            Ok(())
        }

        fn stop(&self, seat: &SeatName) -> Result<(), ServiceError> {
            self.calls.lock().unwrap().push(format!("stop:{seat}"));
            self.instances.lock().unwrap().remove(seat);
            Ok(())
        }

        fn status(&self, seat: &SeatName) -> Result<Option<SeatInstance>, ServiceError> {
            Ok(self.instances.lock().unwrap().get(seat).cloned())
        }
    }

    fn config(seat: &str, devices: &[&str]) -> SeatConfig {
        SeatConfig::new(
            SeatName::parse(seat).unwrap(),
            Path::new("/run/test"),
            seat == "seat0",
            devices.iter().map(std::path::PathBuf::from),
        )
        .unwrap()
    }

    #[test]
    fn plan_ignores_ready_identical_instances() {
        let wanted = config("seat0", &["/dev/dri/card0"]);
        let desired = BTreeMap::from([(wanted.name.clone(), wanted.clone())]);
        let deployed = BTreeMap::from([(
            wanted.name.clone(),
            SeatInstance {
                config: wanted,
                service_dir: "/run/test/services/seat0".into(),
                ready: true,
            },
        )]);
        assert!(build_plan(&desired, &deployed, None).is_empty());
    }

    #[test]
    fn fake_backends_converge_and_replace_stop_before_start() {
        let topology = FakeTopology::default();
        let services = FakeServices::default();
        let initial = config("seat1", &["/dev/dri/card1"]);
        topology
            .0
            .lock()
            .unwrap()
            .insert(initial.name.clone(), initial.clone());
        let orchestrator =
            Orchestrator::new(Box::new(topology.clone()), Box::new(services.clone()));
        assert!(orchestrator.converge_startup().unwrap().is_empty());
        assert_eq!(
            orchestrator.ready_seats().unwrap().as_slice(),
            std::slice::from_ref(&initial.name)
        );

        let replacement = config("seat1", &["/dev/dri/card1", "/dev/input/event5"]);
        topology
            .0
            .lock()
            .unwrap()
            .insert(replacement.name.clone(), replacement);
        let change = orchestrator.plan(None).unwrap().pop().unwrap();
        assert_eq!(change.kind, ChangeKind::Replace);
        orchestrator.apply(&change).unwrap();
        assert_eq!(
            services.calls.lock().unwrap().as_slice(),
            ["start:seat1", "stop:seat1", "start:seat1"]
        );
    }

    #[test]
    fn plan_adds_removes_and_replaces_in_seat_order() {
        let old0 = config("seat0", &["/dev/dri/card0"]);
        let new0 = config("seat0", &["/dev/dri/card0", "/dev/input/event0"]);
        let old1 = config("seat1", &["/dev/dri/card1"]);
        let new2 = config("seat2", &["/dev/dri/card2"]);
        let desired = BTreeMap::from([(new0.name.clone(), new0), (new2.name.clone(), new2)]);
        let deployed = BTreeMap::from([
            (
                old0.name.clone(),
                SeatInstance {
                    config: old0,
                    service_dir: "/old0".into(),
                    ready: true,
                },
            ),
            (
                old1.name.clone(),
                SeatInstance {
                    config: old1,
                    service_dir: "/old1".into(),
                    ready: true,
                },
            ),
        ]);
        let plan = build_plan(&desired, &deployed, None);
        assert_eq!(
            plan.iter().map(|change| change.kind).collect::<Vec<_>>(),
            [ChangeKind::Replace, ChangeKind::Remove, ChangeKind::Add]
        );
        assert_eq!(
            plan.iter()
                .map(|change| change.seat.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["seat0", "seat1", "seat2"])
        );
    }
}
