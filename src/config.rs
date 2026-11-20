use crate::pipeline::{Pipeline, PipelineConfig};
use crate::v4l::FormatRequest;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::HashSet, fs, net::SocketAddr, path::Path};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    #[serde(default)]
    pub devices: Vec<DeviceProfile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfile {
    pub name: String,
    pub capture: FormatRequest,
    pub pipeline: PipelineConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&contents).with_context(|| format!("parse {}", path.display()))
    }

    fn parse(contents: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(contents)?;
        let mut names = HashSet::new();
        for profile in &mut config.devices {
            profile.name = short_video_name(&profile.name);
            ensure!(!profile.name.is_empty(), "profile name must not be blank");
            ensure!(
                names.insert(profile.name.as_str()),
                "duplicate profile name {:?}",
                profile.name
            );
            validate_format(&profile.capture)
                .with_context(|| format!("capture for profile {:?}", profile.name))?;
            Pipeline::validate(&profile.pipeline, &profile.capture.fourcc)
                .with_context(|| format!("pipeline for profile {:?}", profile.name))?;
        }
        Ok(config)
    }
}

pub fn short_video_name(name: &str) -> String {
    let name = name.trim_start();
    name.strip_prefix("vi-output, ")
        .unwrap_or(name)
        .trim()
        .replace('-', "_")
}

pub fn validate_format(format: &FormatRequest) -> Result<()> {
    ensure!(
        format.fourcc.len() == 4
            && format
                .fourcc
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' '),
        "FourCC must be four printable ASCII bytes"
    );
    ensure!(
        format.width > 0 && format.height > 0,
        "capture dimensions must be positive"
    );
    if let Some(interval) = &format.interval {
        ensure!(
            interval.numerator > 0 && interval.denominator > 0,
            "frame interval must be positive"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles(device_fields: &[&str]) -> Result<Config> {
        let mut contents = String::from("listen = '127.0.0.1:8766'\n");
        for fields in device_fields {
            contents.push_str(&format!(
                r#"
[[devices]]
{fields}
[devices.capture]
fourcc = 'YUYV'
width = 1600
height = 1200
[[devices.pipeline.modules]]
id = 'color'
kind = 'to_rgb'
[devices.pipeline.output]
id = 'jpeg'
kind = 'jpeg_encode'
quality = 80
"#
            ));
        }
        Config::parse(&contents)
    }

    #[test]
    fn loads_bundled_orin_profile() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/orin.toml");
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config
                .devices
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["himax_rgb", "himax_tof"]
        );
    }

    #[test]
    fn normalizes_video_names_without_folding_case() {
        assert_eq!(short_video_name("  vi-output,  Himax-RGB  "), "Himax_RGB");
        assert_eq!(
            short_video_name(" vi-output,Himax-RGB "),
            "vi_output,Himax_RGB"
        );
        let config = profiles(&["name = '  vi-output,  Himax-RGB  '"]).unwrap();
        assert_eq!(config.devices[0].name, "Himax_RGB");
    }

    #[test]
    fn rejects_blank_and_missing_names() {
        for fields in ["name = ''", "name = '  '", "name = 'vi-output,  '"] {
            let error = profiles(&[fields]).unwrap_err();
            assert!(error.to_string().contains("profile name must not be blank"));
        }
        let error = profiles(&[""]).unwrap_err();
        assert!(error.to_string().contains("missing field `name`"));
    }

    #[test]
    fn rejects_alias_and_board_fields() {
        for field in ["alias", "board"] {
            for fields in [
                format!("{field} = 'legacy'"),
                format!("name = 'himax_rgb'\n{field} = 'legacy'"),
            ] {
                let error = profiles(&[&fields]).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains(&format!("unknown field `{field}`"))
                );
            }
        }
    }

    #[test]
    fn rejects_duplicate_normalized_names() {
        let error = profiles(&["name = 'himax-rgb'", "name = 'himax_rgb'"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate profile name \"himax_rgb\"")
        );
    }
}
