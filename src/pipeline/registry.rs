use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    builtins::{HimaxTofMetadata, HimaxTofPair, JpegEncode, ResizeNearest, ToRgb},
    config::{ModuleConfig, PipelineConfig},
    frame::PixelFormat,
    module::Module,
};

type ModuleFactory = fn(&ModuleConfig) -> Result<Box<dyn Module>>;

const IMAGE_REGISTRY: &[(&str, ModuleFactory)] = &[
    ("to_rgb", build_module::<ToRgb>),
    ("himax_tof_metadata", build_module::<HimaxTofMetadata>),
    ("himax_tof_pair", build_module::<HimaxTofPair>),
    ("resize_nearest", build_module::<ResizeNearest>),
];
const OUTPUT_REGISTRY: &[(&str, ModuleFactory)] = &[("jpeg_encode", build_module::<JpegEncode>)];

fn build_module<Implementation: Module + DeserializeOwned + 'static>(
    config: &ModuleConfig,
) -> Result<Box<dyn Module>> {
    Ok(Box::new(serde_json::from_value::<Implementation>(
        Value::Object(config.options.clone()),
    )?))
}

pub(super) struct ModuleInstance {
    pub(super) instance_id: String,
    kind: String,
    pub(super) module: Box<dyn Module>,
}

impl ModuleInstance {
    pub(super) fn label(&self) -> String {
        format!("module {:?} ({})", self.instance_id, self.kind)
    }
}

pub(super) fn assemble(
    config: &PipelineConfig,
    mut format: PixelFormat,
) -> Result<Vec<ModuleInstance>> {
    let mut instance_ids = HashSet::new();
    let mut modules = Vec::new();
    let entries = config
        .modules
        .iter()
        .map(|config| (config, IMAGE_REGISTRY))
        .chain(std::iter::once((&config.output, OUTPUT_REGISTRY)));
    for (config, registry) in entries {
        ensure!(!config.id.trim().is_empty(), "module ID must not be blank");
        ensure!(
            instance_ids.insert(&config.id),
            "duplicate module ID {:?}",
            config.id
        );
        let factory = registry
            .iter()
            .find(|(kind, _)| *kind == config.kind)
            .map(|(_, factory)| factory)
            .with_context(|| {
                format!(
                    "module {:?}: unknown kind {:?} for this chain position",
                    config.id, config.kind
                )
            })?;
        let module = factory(config).with_context(|| {
            format!("module {:?} ({}): invalid options", config.id, config.kind)
        })?;
        let module_instance = ModuleInstance {
            instance_id: config.id.clone(),
            kind: config.kind.clone(),
            module,
        };
        format = module_instance
            .module
            .output_format(format)
            .with_context(|| {
                format!(
                    "{}: incompatible input FourCC {}",
                    module_instance.label(),
                    format.fourcc()
                )
            })?;
        modules.push(module_instance);
    }
    ensure!(format == PixelFormat::Jpeg, "chain must produce JPEG");
    Ok(modules)
}
