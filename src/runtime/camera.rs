use super::{
    Command, ControlRead, ControlValue, Failure, Output, PreviewTarget, Reply, Request, Responder,
    Target,
    diagnostics::{Diagnostics, unix_ms},
    discovery::{DeviceIdentity, Inventory, device_identity, identity_from_metadata},
    handoff::WorkSlot,
    serialize,
    session::{Session, lock},
};
use crate::{
    config::{DeviceProfile, validate_format},
    pipeline::{Parameter, Pipeline},
    v4l::{
        Capabilities, CaptureEvent, ControlGroup, ControlScan, ControlValueScan, Device,
        FormatRequest, ReadableControl,
    },
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Serialize)]
struct Selection {
    node_id: String,
    selection_id: u64,
    capabilities: Capabilities,
    has_profile: bool,
    path: PathBuf,
    default_format: Option<FormatRequest>,
}

struct Selected {
    view: Selection,
    identity: DeviceIdentity,
    device: Device,
    profile: Option<DeviceProfile>,
    readable_controls: BTreeMap<u32, ReadableControl>,
}

const SLOW_CONTROL: Duration = Duration::from_millis(10);
// At 60 raw fps the ToF pipeline needs consecutive captures to output a pair.
const CAPTURE_RECOVERY: Duration = Duration::from_millis(50);

enum PendingScan {
    List(ControlGroup, ControlScan),
    Values(ControlValueScan),
}

struct PendingControls {
    selection_id: u64,
    scan: PendingScan,
    sender: Responder,
    resume_at: Instant,
}

pub(super) struct Camera {
    inventory: Inventory,
    selection: Option<Selected>,
    pending_controls: Option<PendingControls>,
    session: Option<Arc<Session>>,
    work: Arc<WorkSlot>,
    publisher: watch::Sender<Output>,
    next_id: u64,
    last_error: Option<String>,
    last_diagnostics: Diagnostics,
    overrides: HashMap<String, HashMap<(String, String), Value>>,
}

impl Camera {
    pub(super) fn new(
        profiles: Vec<DeviceProfile>,
        work: Arc<WorkSlot>,
        publisher: watch::Sender<Output>,
    ) -> Self {
        Self {
            inventory: Inventory::new(profiles),
            selection: None,
            pending_controls: None,
            session: None,
            work,
            publisher,
            next_id: unix_ms().saturating_mul(1024),
            last_error: None,
            last_diagnostics: Diagnostics::default(),
            overrides: HashMap::new(),
        }
    }

    pub(super) fn run(mut self, mut requests: mpsc::Receiver<Request>) {
        let mut identity_check = Instant::now();
        loop {
            if identity_check.elapsed().as_millis() >= 250 {
                self.check_identity();
                identity_check = Instant::now();
            }
            let request = match requests.try_recv() {
                Ok(request) => Some(request),
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty)
                    if self.session.is_none() && self.pending_controls.is_none() =>
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
            }
            self.poll_frame();
        }
        let _ = self.stop_preview();
        self.work.close();
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::Command(command, sender) if !sender.is_closed() => {
                let _ = sender.send(self.execute(command));
            }
            Request::Controls(group, sender) if !sender.is_closed() => {
                self.start_control_scan(group, sender);
            }
            _ => {}
        }
    }

    fn start_control_scan(&mut self, read: ControlRead, sender: Responder) {
        if let ControlRead::Values(target) = &read
            && let Err(error) = self.require_target(target)
        {
            let _ = sender.send(Err(error));
            return;
        }
        if self
            .pending_controls
            .as_ref()
            .is_some_and(|pending| !pending.sender.is_closed())
        {
            let _ = sender.send(Err(Failure::Unavailable));
            return;
        }
        self.pending_controls = None;
        let Some(selection) = &self.selection else {
            let _ = sender.send(Ok(json!([])));
            return;
        };
        let selection_id = selection.view.selection_id;
        let scan = match read {
            ControlRead::List(group) => selection
                .device
                .scan_controls(group)
                .map(|scan| PendingScan::List(group, scan)),
            ControlRead::Values(_) => Ok(PendingScan::Values(ControlValueScan::new(
                selection.readable_controls.values().copied().collect(),
            ))),
        };
        match self.track_device_error(scan) {
            Ok(scan) => {
                self.pending_controls = Some(PendingControls {
                    selection_id,
                    scan,
                    sender,
                    resume_at: Instant::now(),
                })
            }
            Err(error) => {
                let _ = sender.send(Err(error.into()));
            }
        }
    }

    fn step_control_scan(&mut self) {
        let Some(mut pending) = self.pending_controls.take() else {
            return;
        };
        if pending.sender.is_closed() {
            return;
        }
        let Some(selection) = self
            .selection
            .as_ref()
            .filter(|selected| selected.view.selection_id == pending.selection_id)
        else {
            let _ = pending.sender.send(Err(Failure::Stale));
            return;
        };
        let started_at = Instant::now();
        if started_at < pending.resume_at {
            self.pending_controls = Some(pending);
            return;
        }
        let step = match &mut pending.scan {
            PendingScan::List(group, scan) => {
                let result = scan.step(&selection.device);
                self.track_device_error(result).map(|completed| {
                    completed.map(|controls| {
                        if let Some(selection) = &mut self.selection {
                            selection
                                .readable_controls
                                .retain(|id, _| !group.includes(*id));
                            selection.readable_controls.extend(
                                controls
                                    .iter()
                                    .filter_map(|control| control.readable())
                                    .map(|control| (control.id, control)),
                            );
                        }
                        serialize(controls)
                    })
                })
            }
            PendingScan::Values(scan) => {
                let result = scan.step(&selection.device);
                self.track_device_error(result)
                    .map(|completed| completed.map(serialize))
            }
        };
        match step {
            Ok(None) => {
                let finished_at = Instant::now();
                if self.session.is_some() && finished_at.duration_since(started_at) >= SLOW_CONTROL
                {
                    pending.resume_at = finished_at + CAPTURE_RECOVERY;
                }
                self.pending_controls = Some(pending);
            }
            Ok(Some(reply)) => {
                let _ = pending.sender.send(reply);
            }
            Err(error) => {
                let _ = pending.sender.send(Err(error.into()));
            }
        }
    }

    fn cancel_control_scan(&mut self) {
        if let Some(pending) = self.pending_controls.take() {
            let _ = pending.sender.send(Err(Failure::Stale));
        }
    }

    fn execute(&mut self, command: Command) -> Reply {
        let is_mutation = matches!(
            &command,
            Command::Select(_)
                | Command::Start(..)
                | Command::Stop(_)
                | Command::Control(..)
                | Command::Parameter(..)
        );
        let result = self.dispatch(command);
        if is_mutation && let Err(Failure::Invalid(error)) = &result {
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
                    .map(|selected| (selected.identity, &selected.view.capabilities));
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
                    .set_control(group, id, value);
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

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn status(&self) -> Value {
        let diagnostics = self
            .session
            .as_deref()
            .map_or_else(|| self.last_diagnostics.clone(), Session::snapshot);
        json!({"selection": self.selection.as_ref().map(|selected| &selected.view),
            "preview": self.session.as_ref().map(|session| &session.preview),
            "diagnostics": diagnostics, "last_error": self.last_error})
    }

    fn parameters(&self) -> Vec<Parameter> {
        self.session
            .as_ref()
            .map_or_else(Vec::new, |session| lock(&session.pipeline).parameters())
    }

    fn require_target(&self, target: &Target) -> Result<&Selected, Failure> {
        self.selection
            .as_ref()
            .filter(|selected| {
                selected.view.node_id == target.node_id
                    && selected.view.selection_id == target.selection_id
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
        self.overrides
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
        let device = Device::open(&path)?;
        anyhow::ensure!(
            identity == identity_from_metadata(&device.metadata()?)?,
            "video node changed while opening"
        );
        let (profile, profile_error) = match self.inventory.device_profile(identity) {
            Ok(profile) => (profile, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let capabilities = device.capabilities()?;
        anyhow::ensure!(
            identity == device_identity(&path)?,
            "video node changed during selection"
        );
        let selection_id = self.next_id();
        self.selection = Some(Selected {
            view: Selection {
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
        if device_identity(&selection.view.path).ok() != Some(selection.identity) {
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
            && error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<io::Error>())
                .any(|cause| {
                    matches!(
                        cause.raw_os_error(),
                        Some(libc::ENODEV | libc::ENXIO | libc::EBADF)
                    )
                })
        {
            self.invalidate(format!("{error:#}"));
        }
        result
    }

    fn stop_preview(&mut self) -> Result<()> {
        self.cancel_control_scan();
        self.publisher.send_replace(Output::default());
        self.work.clear();
        if let Some(session) = self.session.take() {
            self.last_diagnostics = session.snapshot();
        }
        if let Some(selection) = &mut self.selection
            && let Err(error) = selection.device.stop()
        {
            self.selection = None;
            return Err(error).context("stop preview; device selection revoked");
        }
        Ok(())
    }

    fn start_preview(&mut self, request: Option<FormatRequest>) -> Result<()> {
        let selection = self.selection.as_ref().context("no device selected")?;
        anyhow::ensure!(
            selection.view.capabilities.can_stream,
            "node has no supported capture buffer type or streaming I/O"
        );
        let profile = self
            .inventory
            .device_profile(selection.identity)?
            .context("no device profile for this camera name; preview disabled")?;
        anyhow::ensure!(
            selection.profile.as_ref().map(|profile| &profile.name) == Some(&profile.name),
            "camera profile changed; select the node again"
        );
        let requested = request.unwrap_or(profile.capture);
        validate_format(&requested)?;
        Pipeline::validate(&profile.pipeline, &requested.fourcc)?;
        self.stop_preview()?;
        self.last_diagnostics = Diagnostics::default();
        let device = &mut self
            .selection
            .as_mut()
            .context("device selection lost")?
            .device;
        let result = device.start(&requested).context("start V4L2 preview");
        let negotiated = self.track_device_error(result).and_then(|spec| {
            Pipeline::build(&profile.pipeline, &spec).map(|pipeline| (spec, pipeline))
        });
        let (spec, mut pipeline) = match negotiated {
            Ok(negotiated) => negotiated,
            Err(error) => {
                return match self.stop_preview() {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!("cleanup also failed: {cleanup:#}"))),
                };
            }
        };
        self.last_error = None;
        if let Some(overrides) = self.overrides.get_mut(&profile.name) {
            let mut reset = Vec::new();
            overrides.retain(|(instance, parameter), value| {
                if let Err(error) = pipeline.set_parameter(instance, parameter, value.clone()) {
                    let actual = pipeline
                        .parameters()
                        .into_iter()
                        .find(|entry| {
                            entry.instance_id == *instance && entry.parameter_id == *parameter
                        })
                        .map(|entry| entry.value);
                    reset.push(format!(
                        "{instance}/{parameter} reset to {actual:?}: {error:#}"
                    ));
                    return false;
                }
                true
            });
            if !reset.is_empty() {
                self.last_error = Some(reset.join("; "));
            }
        }
        let session_id = self.next_id();
        self.publisher.send_replace(Output {
            session_id: Some(session_id),
            ..Output::default()
        });
        self.session = Some(Arc::new(Session::new(
            session_id, requested, spec, pipeline,
        )));
        Ok(())
    }

    fn poll_frame(&mut self) {
        let (Some(session), Some(selection)) = (&self.session, &mut self.selection) else {
            return;
        };
        match selection.device.capture(20) {
            Ok(CaptureEvent::Frame(frame)) => {
                let mut diagnostics = lock(&session.diagnostics);
                diagnostics.captured_count += 1;
                diagnostics.last_capture_unix_ms = Some(unix_ms());
                diagnostics.last_sequence = Some(frame.sequence);
                drop(diagnostics);
                self.work.push(session, frame);
            }
            Ok(CaptureEvent::Timeout) => {}
            Ok(CaptureEvent::DriverError { sequence }) => {
                Self::record_invalid_buffer(session, sequence, "V4L2_BUF_FLAG_ERROR");
            }
            Ok(CaptureEvent::Empty { sequence }) => {
                Self::record_invalid_buffer(session, sequence, "zero bytes used");
            }
            Err(error) => {
                let reason = format!("capture failed: {error:#}");
                lock(&session.diagnostics).last_capture_error = Some(reason.clone());
                if self.track_device_error(Err::<(), _>(error)).is_err() && self.session.is_some() {
                    let cleanup = self.stop_preview();
                    self.last_error = Some(match cleanup {
                        Ok(()) => reason,
                        Err(error) => format!("{reason}; additionally {error:#}"),
                    });
                }
            }
        }
    }

    fn record_invalid_buffer(session: &Session, sequence: u32, reason: &str) {
        let mut diagnostics = lock(&session.diagnostics);
        diagnostics.invalid_capture_count += 1;
        diagnostics.last_capture_error =
            Some(format!("discarded capture buffer {sequence}: {reason}"));
    }
}
