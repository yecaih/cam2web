use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

use super::frame::{Frame, Layout, PixelFormat};

#[derive(Clone, Debug, Serialize)]
pub struct Parameter {
    pub instance_id: String,
    pub parameter_id: String,
    pub value: Value,
    pub value_type: &'static str,
    pub min: u8,
    pub max: u8,
    pub is_online_writable: bool,
}

pub(super) trait Module: Send {
    fn output_format(&self, input: PixelFormat) -> Result<PixelFormat>;
    fn output_layout(&self, input: Layout) -> Result<Layout>;
    fn process(&mut self, frame: Frame, output: Layout) -> Result<Option<Frame>>;

    fn parameters(&self, _instance_id: &str) -> Vec<Parameter> {
        Vec::new()
    }

    fn set_parameter(&mut self, parameter_id: &str, _value: Value) -> Result<()> {
        bail!("unknown online parameter {parameter_id:?}")
    }
}
