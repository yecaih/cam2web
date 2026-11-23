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

use crate::v4l::{CaptureSpec, CapturedFrame};

pub use config::PipelineConfig;
pub use metadata::TofPairMetadata;
pub use module::Parameter;

use metadata::FrameMetadata;

use frame::{Frame, FrameIdentity, Layout, PixelFormat};
use registry::{ModuleInstance, assemble};

struct Stage {
    module_instance: ModuleInstance,
    output_layout: Layout,
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
        self.module_instance
            .module
            .process(frame, self.output_layout)
            .with_context(|| format!("{}: frame {identity:?}", self.module_instance.label()))
    }
}

pub struct Pipeline {
    capture_layout: Layout,
    min_capture_bytes: usize,
    max_capture_bytes: usize,
    transforms: Vec<Stage>,
    jpeg_encoder: Stage,
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
        let mut stage_input_layout = input;
        for module_instance in assemble(config, input.format)
            .with_context(|| format!("invalid chain for negotiated layout {input:?}"))?
        {
            let output_layout = module_instance
                .module
                .output_layout(stage_input_layout)
                .with_context(|| {
                    format!(
                        "{}: incompatible input layout {stage_input_layout:?}",
                        module_instance.label()
                    )
                })?;
            stages.push(Stage {
                module_instance,
                output_layout,
            });
            stage_input_layout = output_layout;
        }
        let jpeg_encoder = stages.pop().context("missing JPEG output module")?;
        Ok(Self {
            capture_layout: input,
            min_capture_bytes,
            max_capture_bytes,
            transforms: stages,
            jpeg_encoder,
        })
    }

    pub fn process(
        &mut self,
        session_id: u64,
        captured: CapturedFrame,
    ) -> Result<Option<Processed>> {
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
        captured: CapturedFrame,
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
            layout: self.capture_layout,
            plane: captured.bytes,
            metadata: FrameMetadata::None,
        };
        for transform in &mut self.transforms {
            let Some(transformed_frame) = transform.process(frame)? else {
                return Ok(None);
            };
            frame = transformed_frame;
        }
        let encode_started_at = Instant::now();
        let jpeg = self
            .jpeg_encoder
            .process(frame)?
            .context("JPEG encoder returned no output")?;
        Ok(Some((jpeg, encode_started_at.elapsed())))
    }

    pub fn parameters(&self) -> Vec<Parameter> {
        self.transforms
            .iter()
            .chain(std::iter::once(&self.jpeg_encoder))
            .flat_map(|stage| {
                stage
                    .module_instance
                    .module
                    .parameters(&stage.module_instance.instance_id)
            })
            .collect()
    }

    pub fn set_parameter(
        &mut self,
        instance_id: &str,
        parameter_id: &str,
        value: Value,
    ) -> Result<()> {
        let module_instance = self
            .transforms
            .iter_mut()
            .chain(std::iter::once(&mut self.jpeg_encoder))
            .map(|stage| &mut stage.module_instance)
            .find(|module_instance| module_instance.instance_id == instance_id)
            .with_context(|| format!("unknown module instance {instance_id:?}"))?;
        module_instance
            .module
            .set_parameter(parameter_id, value)
            .with_context(|| module_instance.label())
    }
}
