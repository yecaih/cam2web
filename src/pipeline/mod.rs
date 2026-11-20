mod builtins;
mod config;
mod frame;
mod metadata;
mod module;
mod registry;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::time::{Duration, Instant};

use crate::v4l::{CaptureSpec, Captured};

pub use config::PipelineConfig;
pub use metadata::TofPairMetadata;
pub use module::Parameter;

use metadata::FrameMetadata;

use frame::{Frame, FrameIdentity, Layout, PixelFormat};
use registry::{NamedModule, assemble};

struct Stage {
    named: NamedModule,
    output: Layout,
}

#[derive(Debug)]
pub struct Processed {
    pub jpeg: Vec<u8>,
    pub encode_duration: Duration,
    pub tof_metadata: Option<TofPairMetadata>,
}

impl Stage {
    fn process(&mut self, frame: Frame) -> Result<Option<Frame>> {
        let identity = frame.identity;
        self.named
            .module
            .process(frame, self.output)
            .with_context(|| format!("{}: frame {identity:?}", self.named.label()))
    }
}

pub struct Pipeline {
    input: Layout,
    min_capture_bytes: usize,
    max_capture_bytes: usize,
    stages: Vec<Stage>,
}

impl Pipeline {
    pub fn validate(config: &PipelineConfig, requested_fourcc: &str) -> Result<()> {
        assemble(config, PixelFormat::from_capture(requested_fourcc)?)?;
        Ok(())
    }

    pub fn build(config: &PipelineConfig, spec: &CaptureSpec) -> Result<Self> {
        let input = Layout {
            format: PixelFormat::from_capture(&spec.fourcc)?,
            width: spec.width,
            height: spec.height,
            stride_bytes: spec.stride_bytes,
        };
        let min_capture_bytes = input
            .required_bytes()
            .with_context(|| format!("invalid negotiated capture layout {input:?}"))?;
        let max_capture_bytes = spec.size_bytes as usize;
        ensure!(
            max_capture_bytes >= min_capture_bytes,
            "negotiated capture layout {input:?} needs {min_capture_bytes} bytes, but sizeimage is {max_capture_bytes}"
        );
        let mut stages = Vec::new();
        let mut layout = input;
        for named in assemble(config, input.format)
            .with_context(|| format!("invalid chain for negotiated layout {input:?}"))?
        {
            let output = named.module.output_layout(layout).with_context(|| {
                format!("{}: incompatible input layout {layout:?}", named.label())
            })?;
            stages.push(Stage { named, output });
            layout = output;
        }
        Ok(Self {
            input,
            min_capture_bytes,
            max_capture_bytes,
            stages,
        })
    }

    pub fn process(&mut self, session_id: u64, captured: Captured) -> Result<Option<Processed>> {
        Ok(self
            .process_frame(session_id, captured)?
            .map(|(frame, encode_duration)| Processed {
                jpeg: frame.plane,
                encode_duration,
                tof_metadata: match frame.metadata {
                    FrameMetadata::TofPair(pair) => Some(pair),
                    _ => None,
                },
            }))
    }

    fn process_frame(
        &mut self,
        session_id: u64,
        captured: Captured,
    ) -> Result<Option<(Frame, Duration)>> {
        let identity = FrameIdentity {
            session_id,
            sequence: captured.sequence,
        };
        let frame_bytes = captured.bytes.len();
        ensure!(
            (self.min_capture_bytes..=self.max_capture_bytes).contains(&frame_bytes),
            "frame {identity:?}: got {frame_bytes} capture bytes, expected {}..={}",
            self.min_capture_bytes,
            self.max_capture_bytes
        );
        let mut frame = Frame {
            identity,
            layout: self.input,
            plane: captured.bytes,
            metadata: FrameMetadata::None,
        };
        let (output, transforms) = self
            .stages
            .split_last_mut()
            .context("missing JPEG output module")?;
        for stage in transforms {
            let Some(next) = stage.process(frame)? else {
                return Ok(None);
            };
            frame = next;
        }
        let started = Instant::now();
        let jpeg = output
            .process(frame)?
            .context("JPEG encoder returned no output")?;
        Ok(Some((jpeg, started.elapsed())))
    }

    pub fn parameters(&self) -> Vec<Parameter> {
        self.stages
            .iter()
            .flat_map(|stage| stage.named.module.parameters(&stage.named.id))
            .collect()
    }

    pub fn set_parameter(
        &mut self,
        instance_id: &str,
        parameter_id: &str,
        value: Value,
    ) -> Result<()> {
        let stage = self
            .stages
            .iter_mut()
            .find(|stage| stage.named.id == instance_id)
            .with_context(|| format!("unknown module instance {instance_id:?}"))?;
        stage
            .named
            .module
            .set_parameter(parameter_id, value)
            .with_context(|| stage.named.label())
    }
}
