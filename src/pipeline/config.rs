use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    #[serde(default)]
    pub(super) modules: Vec<ModuleConfig>,
    pub(super) output: ModuleConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ModuleConfig {
    pub(super) id: String,
    pub(super) kind: String,
    #[serde(flatten)]
    pub(super) options: Map<String, Value>,
}
