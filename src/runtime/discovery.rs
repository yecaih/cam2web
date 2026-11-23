use crate::{
    config::{DeviceProfile, short_video_name},
    v4l::{Capabilities, Device},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

pub(super) struct Inventory {
    profiles: Vec<DeviceProfile>,
}

impl Inventory {
    pub(super) fn new(profiles: Vec<DeviceProfile>) -> Self {
        Self { profiles }
    }

    pub(super) fn path_for(&self, node_id: &str) -> Result<PathBuf> {
        video_paths()?
            .into_iter()
            .find(|path| path == Path::new(node_id))
            .context("node is not in /dev/video*; refresh node discovery")
    }

    pub(super) fn device_profile(&self, identity: DeviceIdentity) -> Result<Option<DeviceProfile>> {
        let nodes = video_nodes()?;
        let node = nodes
            .iter()
            .find(|node| node.identity == identity)
            .context("video node changed during profile lookup")?;
        Ok(self.profile(node, &nodes)?.cloned())
    }

    pub(super) fn nodes(
        &self,
        selected: Option<(DeviceIdentity, &Capabilities)>,
    ) -> Result<Vec<Node>> {
        let video_nodes = video_nodes()?;
        let mut nodes = Vec::new();
        for node in &video_nodes {
            let (capabilities, mut error) = match node.capabilities(selected) {
                Ok(capabilities) => (Some(capabilities), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            let has_profile = match self.profile(node, &video_nodes) {
                Ok(profile) => profile.is_some(),
                Err(conflict) => {
                    error = Some(match error {
                        Some(device) => format!("{device}; {conflict:#}"),
                        None => format!("{conflict:#}"),
                    });
                    false
                }
            };
            nodes.push(Node {
                id: node.path.display().to_string(),
                name: node.name.clone(),
                path: node.path.clone(),
                has_profile,
                capabilities,
                error,
            });
        }
        Ok(nodes)
    }

    fn profile<'a>(
        &'a self,
        node: &VideoNode,
        nodes: &[VideoNode],
    ) -> Result<Option<&'a DeviceProfile>> {
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| Some(profile.name.as_str()) == node.short_name.as_deref())
        else {
            return Ok(None);
        };
        ensure!(
            !nodes.iter().any(|other| other.identity != node.identity
                && other.short_name == node.short_name),
            "camera name {:?} matches multiple video nodes; preview profile is ambiguous",
            profile.name
        );
        Ok(Some(profile))
    }
}

#[derive(Serialize)]
pub(super) struct Node {
    id: String,
    name: Option<String>,
    path: PathBuf,
    capabilities: Option<Capabilities>,
    error: Option<String>,
    has_profile: bool,
}

struct VideoNode {
    path: PathBuf,
    identity: DeviceIdentity,
    name: Option<String>,
    short_name: Option<String>,
}

impl VideoNode {
    fn new(path: PathBuf, identity: DeviceIdentity, name: Option<String>) -> Self {
        let short_name = name.as_deref().map(short_video_name);
        Self {
            path,
            identity,
            name,
            short_name,
        }
    }

    fn capabilities(
        &self,
        selected: Option<(DeviceIdentity, &Capabilities)>,
    ) -> Result<Capabilities> {
        if let Some((_, capabilities)) = selected.filter(|(identity, _)| *identity == self.identity)
        {
            return Ok(capabilities.clone());
        }
        let device = Device::open(&self.path)?;
        ensure!(
            identity_from_metadata(&device.metadata()?)? == self.identity,
            "video node changed during inventory lookup"
        );
        device.capabilities()
    }
}

fn video_nodes() -> Result<Vec<VideoNode>> {
    Ok(video_paths()?
        .into_iter()
        .filter_map(|path| {
            let identity = device_identity(&path).ok()?;
            let name = video_name(identity);
            (device_identity(&path).ok()? == identity).then(|| VideoNode::new(path, identity, name))
        })
        .collect())
}

fn video_name(identity: DeviceIdentity) -> Option<String> {
    let device_number = identity.device_number;
    let device = format!(
        "{}:{}",
        libc::major(device_number),
        libc::minor(device_number)
    );
    let name = fs::read_to_string(Path::new("/sys/dev/char").join(device).join("name")).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn video_paths() -> Result<Vec<PathBuf>> {
    let mut nodes: Vec<_> = fs::read_dir("/dev")
        .context("list /dev")?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("video"))
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_char_device()))
        .map(|entry| entry.path())
        .collect();
    nodes.sort_unstable();
    Ok(nodes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceIdentity {
    filesystem_device: u64,
    inode: u64,
    device_number: u64,
}

pub(super) fn device_identity(path: &Path) -> io::Result<DeviceIdentity> {
    identity_from_metadata(&fs::metadata(path)?)
}

pub(super) fn identity_from_metadata(metadata: &fs::Metadata) -> io::Result<DeviceIdentity> {
    if !metadata.file_type().is_char_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "video node is no longer a character device",
        ));
    }
    Ok(DeviceIdentity {
        filesystem_device: metadata.dev(),
        inode: metadata.ino(),
        device_number: metadata.rdev(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn inventory() -> Inventory {
        Inventory::new(Config::load(Path::new("config/orin.toml")).unwrap().devices)
    }

    fn node(number: u64, name: Option<&str>) -> VideoNode {
        VideoNode::new(
            format!("/dev/video{number}").into(),
            DeviceIdentity {
                filesystem_device: 1,
                inode: number,
                device_number: number,
            },
            name.map(str::to_owned),
        )
    }

    #[test]
    fn matches_by_name_not_video_number() {
        let inventory = inventory();
        let nodes = [
            node(0, Some("vi-output, himax-tof")),
            node(7, Some("vi-output, himax-rgb")),
            node(8, None),
        ];
        assert_eq!(
            inventory.profile(&nodes[0], &nodes).unwrap().unwrap().name,
            "himax_tof"
        );
        assert_eq!(
            inventory.profile(&nodes[1], &nodes).unwrap().unwrap().name,
            "himax_rgb"
        );
        assert!(inventory.profile(&nodes[2], &nodes).unwrap().is_none());
    }

    #[test]
    fn refuses_ambiguous_names() {
        let inventory = inventory();
        let nodes = [
            node(0, Some("vi-output, himax-rgb")),
            node(4, Some("himax_rgb")),
        ];
        for node in &nodes {
            assert!(
                inventory
                    .profile(node, &nodes)
                    .unwrap_err()
                    .to_string()
                    .contains("ambiguous")
            );
        }
    }
}
