mod control_scan;
mod preview;

use super::{
    Command, ControlValue, Failure, Output, PreviewTarget, Reply, Request, Target,
    diagnostics::{Diagnostics, unix_ms},
    discovery::{DeviceIdentity, Inventory, device_identity, identity_from_metadata},
    handoff::LatestFrameSlot,
    serialize,
    session::{Session, lock},
};
use crate::{
    config::{DeviceProfile, validate_format},
    pipeline::Parameter,
    v4l::{Capabilities, Device, FormatRequest, ReadableControl, is_device_lost},
};
use anyhow::{Context, Result};
use control_scan::PendingControlScan;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

const DEVICE_IDENTITY_CHECK_INTERVAL_MS: u128 = 250;

type ParameterOverrides = HashMap<(String, String), Value>;

#[derive(Clone, Serialize)]
struct SelectionSnapshot {
    node_id: String,
    selection_id: u64,
    capabilities: Capabilities,
    has_profile: bool,
    path: PathBuf,
    default_format: Option<FormatRequest>,
}

struct SelectedDevice {
    snapshot: SelectionSnapshot,
    identity: DeviceIdentity,
    device: Device,
    profile: Option<DeviceProfile>,
    readable_controls: BTreeMap<u32, ReadableControl>,
}

pub(super) struct Camera {
    inventory: Inventory,
    selection: Option<SelectedDevice>,
    pending_control_scan: Option<PendingControlScan>,
    session: Option<Arc<Session>>,
    frame_slot: Arc<LatestFrameSlot>,
    preview_publisher: watch::Sender<Output>,
    last_issued_id: u64,
    last_error: Option<String>,
    last_diagnostics: Diagnostics,
    parameter_overrides: HashMap<String, ParameterOverrides>,
}

impl Camera {
    pub(super) fn new(
        profiles: Vec<DeviceProfile>,
        frame_slot: Arc<LatestFrameSlot>,
        preview_publisher: watch::Sender<Output>,
    ) -> Self {
        Self {
            inventory: Inventory::new(profiles),
            selection: None,
            pending_control_scan: None,
            session: None,
            frame_slot,
            preview_publisher,
            last_issued_id: unix_ms().saturating_mul(1024),
            last_error: None,
            last_diagnostics: Diagnostics::default(),
            parameter_overrides: HashMap::new(),
        }
    }

    pub(super) fn run(mut self, mut requests: mpsc::Receiver<Request>) {
        let mut last_identity_check = Instant::now();
        loop {
            if last_identity_check.elapsed().as_millis() >= DEVICE_IDENTITY_CHECK_INTERVAL_MS {
                self.check_identity();
                last_identity_check = Instant::now();
            }
            let request = match requests.try_recv() {
                Ok(request) => Some(request),
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty)
                    if self.session.is_none() && self.pending_control_scan.is_none() =>
                {
                    match requests.blocking_recv() {
                        Some(request) => Some(request),
                        None => break,
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => None,
            };
            if let Some(request) = request {
                self.check_identity();
                self.handle_request(request);
            } else {
                self.step_control_scan();
                if self.session.is_none() && self.pending_control_scan.is_some() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            self.poll_frame();
        }
        let _ = self.stop_preview();
        self.frame_slot.close();
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::Command(command, sender) if !sender.is_closed() => {
                let _ = sender.send(self.execute(command));
            }
            Request::Controls(query, sender) if !sender.is_closed() => {
                self.start_control_scan(query, sender);
            }
            _ => {}
        }
    }

    fn execute(&mut self, command: Command) -> Reply {
        let should_record_error = matches!(
            &command,
            Command::Select(_) | Command::Start(..) | Command::Stop(_) | Command::Parameter(..)
        );
        let result = self.dispatch(command);
        if should_record_error && let Err(Failure::Invalid(error)) = &result {
            self.last_error = Some(error.clone());
        }
        result
    }

    fn dispatch(&mut self, command: Command) -> Reply {
        match command {
            Command::Nodes => {
                let selected = self
                    .selection
                    .as_ref()
                    .map(|selected| (selected.identity, &selected.snapshot.capabilities));
                serialize(self.inventory.nodes(selected)?)
            }
            Command::Status => Ok(self.status()),
            Command::Select(node_id) => {
                self.select(&node_id)?;
                Ok(self.status())
            }
            Command::Intervals(target, format) => {
                let selected = self.require_target(&target)?;
                validate_format(&format)?;
                let result =
                    selected
                        .device
                        .frame_intervals(&format.fourcc, format.width, format.height);
                serialize(self.track_device_error(result)?)
            }
            Command::Parameters => serialize(self.parameters()),
            Command::Start(target, format) => {
                self.require_target(&target)?;
                self.start_preview(format)?;
                Ok(self.status())
            }
            Command::Stop(target) => {
                self.require_preview_target(&target)?;
                self.stop_preview()?;
                Ok(self.status())
            }
            Command::Control(group, target, id, value) => {
                self.require_target(&target)?;
                let value = match value {
                    ControlValue::Integer(value) => value,
                    ControlValue::Decimal(value) => value
                        .parse::<i64>()
                        .context("expected a signed 64-bit integer")?,
                };
                self.cancel_control_scan();
                let actual = self
                    .require_target(&target)?
                    .device
                    .set_control(group, id, value)
                    .inspect_err(|error| {
                        eprintln!(
                            "cam2web: control write {} {id:#010x}={value}: {error:#}",
                            target.node_id
                        );
                    });
                let actual = self.track_device_error(actual)?;
                Ok(json!({"id": id, "value": actual.map(|value| value.to_string())}))
            }
            Command::Parameter(target, instance, parameter, value) => {
                self.require_preview_target(&target)?;
                self.set_parameter(instance, parameter, value)?;
                serialize(self.parameters())
            }
        }
    }

    fn issue_id(&mut self) -> u64 {
        self.last_issued_id += 1;
        self.last_issued_id
    }

    fn status(&self) -> Value {
        let diagnostics = self
            .session
            .as_deref()
            .map_or_else(|| self.last_diagnostics.clone(), Session::snapshot);
        json!({"selection": self.selection.as_ref().map(|selected| &selected.snapshot),
            "preview": self.session.as_ref().map(|session| &session.preview),
            "diagnostics": diagnostics, "last_error": self.last_error})
    }

    fn parameters(&self) -> Vec<Parameter> {
        self.session
            .as_ref()
            .map_or_else(Vec::new, |session| lock(&session.pipeline).parameters())
    }

    fn require_target(&self, target: &Target) -> Result<&SelectedDevice, Failure> {
        self.selection
            .as_ref()
            .filter(|selected| {
                selected.snapshot.node_id == target.node_id
                    && selected.snapshot.selection_id == target.selection_id
            })
            .ok_or(Failure::Stale)
    }

    fn require_preview_target(&self, target: &PreviewTarget) -> Result<(), Failure> {
        self.require_target(&target.selection)?;
        self.session
            .as_ref()
            .filter(|session| session.preview.session_id == target.session_id)
            .map(|_| ())
            .ok_or(Failure::Stale)
    }

    fn set_parameter(&mut self, instance: String, parameter: String, value: Value) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .context("start preview before editing processor parameters")?;
        let profile = self
            .selection
            .as_ref()
            .and_then(|selection| selection.profile.as_ref())
            .context("selection has no profile")?;
        let mut pipeline = lock(&session.pipeline);
        pipeline.set_parameter(&instance, &parameter, value)?;
        let actual = pipeline
            .parameters()
            .into_iter()
            .find(|entry| entry.instance_id == instance && entry.parameter_id == parameter)
            .context("processor did not report the updated value")?;
        self.parameter_overrides
            .entry(profile.name.clone())
            .or_default()
            .insert((instance, parameter), actual.value);
        Ok(())
    }

    fn select(&mut self, node_id: &str) -> Result<()> {
        let path = self.inventory.path_for(node_id)?;
        let stopped = self.stop_preview();
        self.selection = None;
        stopped?;
        let identity = device_identity(&path)?;
        let mut device = Device::open(&path)?;
        anyhow::ensure!(
            identity == identity_from_metadata(&device.metadata()?)?,
            "video node changed while opening"
        );
        let (profile, profile_error) = match self.inventory.device_profile(identity) {
            Ok(profile) => (profile, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let capabilities = device.capabilities()?;
        device.discover_sensor_controls(&path);
        anyhow::ensure!(
            identity == device_identity(&path)?,
            "video node changed during selection"
        );
        let selection_id = self.issue_id();
        self.selection = Some(SelectedDevice {
            snapshot: SelectionSnapshot {
                node_id: node_id.to_owned(),
                selection_id,
                capabilities,
                path,
                has_profile: profile.is_some(),
                default_format: profile.as_ref().map(|profile| profile.capture.clone()),
            },
            identity,
            device,
            profile,
            readable_controls: BTreeMap::new(),
        });
        self.last_diagnostics = Diagnostics::default();
        self.last_error = profile_error;
        Ok(())
    }

    fn check_identity(&mut self) {
        let Some(selection) = &self.selection else {
            return;
        };
        if device_identity(&selection.snapshot.path).ok() != Some(selection.identity) {
            self.invalidate("selected video node disappeared or changed".into());
        }
    }

    fn invalidate(&mut self, reason: String) {
        let cleanup = self.stop_preview();
        self.selection = None;
        self.last_error = Some(match cleanup {
            Ok(()) => reason,
            Err(error) => format!("{reason}; additionally {error:#}"),
        });
    }

    fn track_device_error<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result
            && is_device_lost(error)
        {
            self.invalidate(format!("{error:#}"));
        }
        result
    }
}
