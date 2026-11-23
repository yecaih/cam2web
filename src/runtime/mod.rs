mod camera;
mod diagnostics;
mod discovery;
mod handoff;
mod session;

use crate::{
    config::DeviceProfile,
    pipeline::TofPairMetadata,
    v4l::{ControlGroup, FormatRequest},
};
use anyhow::{Result, anyhow};
use axum::body::Bytes;
use camera::Camera;
use handoff::{LatestFrameSlot, run_processor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, thread};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Deserialize)]
pub struct Target {
    node_id: String,
    selection_id: u64,
}

#[derive(Deserialize)]
pub struct PreviewTarget {
    #[serde(flatten)]
    selection: Target,
    session_id: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum ControlValue {
    Integer(i64),
    Decimal(String),
}

pub enum Command {
    Nodes,
    Status,
    Select(String),
    Intervals(Target, FormatRequest),
    Parameters,
    Start(Target, Option<FormatRequest>),
    Stop(PreviewTarget),
    Control(ControlGroup, Target, u32, ControlValue),
    Parameter(PreviewTarget, String, String, Value),
}

#[derive(Debug)]
pub enum Failure {
    Invalid(String),
    Stale,
    Unavailable,
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Invalid(format!("{error:#}"))
    }
}

type Reply = Result<Value, Failure>;
type Responder = oneshot::Sender<Reply>;

enum ControlRead {
    List(ControlGroup),
    Values(Target),
}

enum Request {
    Command(Command, Responder),
    Controls(ControlRead, Responder),
}

#[derive(Clone, Default)]
pub struct Output {
    pub session_id: Option<u64>,
    pub jpeg: Option<Bytes>,
    pub tof_metadata: Option<TofPairMetadata>,
}

#[derive(Clone)]
pub struct Service {
    commands: mpsc::Sender<Request>,
    output: watch::Receiver<Output>,
}

impl Service {
    pub fn spawn(profiles: Vec<DeviceProfile>) -> Result<Self> {
        let (commands, requests) = mpsc::channel(16);
        let (publisher, output) = watch::channel(Output::default());
        let frame_slot = Arc::new(LatestFrameSlot::default());
        let processing_slot = Arc::clone(&frame_slot);
        let processed_frames = publisher.clone();
        thread::Builder::new()
            .name("cam2web-process".into())
            .spawn(move || run_processor(&processing_slot, &processed_frames))?;
        let shutdown_slot = Arc::clone(&frame_slot);
        if let Err(error) = thread::Builder::new()
            .name("cam2web-capture".into())
            .spawn(move || Camera::new(profiles, frame_slot, publisher).run(requests))
        {
            shutdown_slot.close();
            return Err(error.into());
        }
        Ok(Self { commands, output })
    }

    pub async fn request(&self, command: Command) -> Reply {
        self.send_request(|sender| Request::Command(command, sender))
            .await
    }

    pub async fn request_controls(&self, group: ControlGroup) -> Reply {
        self.send_request(|sender| Request::Controls(ControlRead::List(group), sender))
            .await
    }

    pub async fn request_control_values(&self, target: Target) -> Reply {
        self.send_request(|sender| Request::Controls(ControlRead::Values(target), sender))
            .await
    }

    async fn send_request(&self, request: impl FnOnce(Responder) -> Request) -> Reply {
        let (sender, reply) = oneshot::channel();
        self.commands
            .try_send(request(sender))
            .map_err(|_| Failure::Unavailable)?;
        reply.await.map_err(|_| Failure::Unavailable)?
    }

    pub fn subscribe(&self) -> watch::Receiver<Output> {
        self.output.clone()
    }
}

fn serialize(value: impl Serialize) -> Reply {
    serde_json::to_value(value).map_err(|error| Failure::from(anyhow!(error)))
}
