use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    },
    path::PathBuf,
};

use super::{ffi::Request, open_node};

const GET_TOPOLOGY: Request<Topology> = Request::new(linux_raw_sys::ioctl::MEDIA_IOC_G_TOPOLOGY);
const SENSOR: u32 = 0x0002_0001;
const VIDEO: u32 = 0x0200;
const SUBDEV: u32 = 0x0203;
const LINK_TYPE_MASK: u32 = 0xf000_0000;
const INTERFACE_LINK: u32 = 0x1000_0000;
const ENABLED: u32 = 1;

type NodeNumber = (u32, u32);

// linux/media.h: these fixed-width structs have the same offsets as the packed UAPI.
#[repr(C)]
#[derive(Default)]
struct Topology {
    version: u64,
    entities: TopologyArray,
    interfaces: TopologyArray,
    pads: TopologyArray,
    links: TopologyArray,
}

#[repr(C)]
#[derive(Default)]
struct TopologyArray {
    count: u32,
    reserved: u32,
    pointer: u64,
}

impl TopologyArray {
    fn allocate<T: Copy + Default>(&mut self) -> io::Result<Vec<T>> {
        let count = self.count as usize;
        let mut entries = Vec::new();
        entries.try_reserve_exact(count).map_err(io::Error::other)?;
        entries.resize(count, T::default());
        if count != 0 {
            self.pointer = entries.as_mut_ptr() as usize as u64;
        }
        Ok(entries)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Entity {
    id: u32,
    name: [u8; 64],
    function: u32,
    flags: u32,
    reserved: [u32; 5],
}

impl Default for Entity {
    fn default() -> Self {
        Self {
            id: 0,
            name: [0; 64],
            function: 0,
            flags: 0,
            reserved: [0; 5],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Interface {
    id: u32,
    kind: u32,
    flags: u32,
    reserved: [u32; 9],
    device: [u32; 16],
}

impl Interface {
    fn number(&self) -> NodeNumber {
        (self.device[0], self.device[1])
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Pad {
    id: u32,
    entity: u32,
    reserved: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Link {
    id: u32,
    source: u32,
    sink: u32,
    flags: u32,
    reserved: [u32; 6],
}

const _: () = {
    assert!(size_of::<Entity>() == 96);
    assert!(size_of::<Interface>() == 112);
    assert!(size_of::<Pad>() == 32);
    assert!(size_of::<Link>() == 40);
};

#[derive(Default)]
struct Graph {
    entities: Vec<Entity>,
    interfaces: Vec<Interface>,
    pads: Vec<Pad>,
    links: Vec<Link>,
}

impl Graph {
    fn read(media: &File) -> io::Result<Self> {
        for _ in 0..3 {
            let mut topology = Topology::default();
            GET_TOPOLOGY.call(media.as_raw_fd(), &mut topology)?;
            let version = topology.version;
            let graph = Self {
                entities: topology.entities.allocate()?,
                interfaces: topology.interfaces.allocate()?,
                pads: topology.pads.allocate()?,
                links: topology.links.allocate()?,
            };
            match GET_TOPOLOGY.call(media.as_raw_fd(), &mut topology) {
                Ok(()) if topology.version == version => return Ok(graph),
                Ok(()) => (),
                Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => (),
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("media topology changed during discovery"))
    }

    fn interface_entities(&self, interface_id: u32) -> impl Iterator<Item = u32> + '_ {
        self.links.iter().filter_map(move |link| {
            (link.source == interface_id && link.flags & LINK_TYPE_MASK == INTERFACE_LINK)
                .then_some(link.sink)
        })
    }

    fn upstream_sensor_nodes(&self, video: NodeNumber) -> io::Result<BTreeSet<NodeNumber>> {
        let mut pending: Vec<_> = self
            .interfaces
            .iter()
            .filter(|interface| interface.kind == VIDEO && interface.number() == video)
            .flat_map(|interface| self.interface_entities(interface.id))
            .collect();
        let mut visited = HashSet::new();
        let mut sensors = HashSet::new();
        while let Some(entity) = pending.pop() {
            if !visited.insert(entity) {
                continue;
            }
            if self
                .entities
                .iter()
                .any(|entry| entry.id == entity && entry.function == SENSOR)
            {
                sensors.insert(entity);
                continue;
            }
            let upstream = self
                .links
                .iter()
                .filter(|link| link.flags & (LINK_TYPE_MASK | ENABLED) == ENABLED)
                .filter(|link| {
                    self.pads
                        .iter()
                        .any(|pad| pad.id == link.sink && pad.entity == entity)
                })
                .filter_map(|link| self.pads.iter().find(|pad| pad.id == link.source));
            pending.extend(upstream.map(|pad| pad.entity));
        }
        if sensors.len() > 1 {
            return Err(io::Error::other(
                "multiple enabled sensors feed the video node",
            ));
        }
        Ok(self
            .interfaces
            .iter()
            .filter(|interface| {
                interface.kind == SUBDEV
                    && self
                        .interface_entities(interface.id)
                        .any(|entity| sensors.contains(&entity))
            })
            .map(Interface::number)
            .collect())
    }
}

fn node_number(file: &File) -> io::Result<NodeNumber> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_char_device() {
        return Err(io::Error::other("expected a character device"));
    }
    Ok((libc::major(metadata.rdev()), libc::minor(metadata.rdev())))
}

pub(super) fn open_sensor(video: &File) -> io::Result<Option<(PathBuf, File)>> {
    let video = node_number(video)?;
    let mut candidates = BTreeSet::new();
    for entry in fs::read_dir("/dev")?.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|name| name.strip_prefix("media"))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            continue;
        }
        if let Ok(graph) = File::open(entry.path()).and_then(|file| Graph::read(&file)) {
            candidates.extend(graph.upstream_sensor_nodes(video)?);
        }
    }
    if candidates.len() > 1 {
        return Err(io::Error::other(
            "multiple subdev nodes match the video node",
        ));
    }
    let Some(&(major, minor)) = candidates.first() else {
        return Ok(None);
    };
    let sysfs = fs::read_link(format!("/sys/dev/char/{major}:{minor}"))?;
    let name = sysfs
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.starts_with("v4l-subdev"))
        .ok_or_else(|| io::Error::other("sensor does not expose a V4L2 subdev node"))?;
    let path = PathBuf::from("/dev").join(name);
    let file = open_node(&path)?;
    if node_number(&file)? != (major, minor) {
        return Err(io::Error::other("sensor node changed during discovery"));
    }
    Ok(Some((path, file)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_enabled_sensor_through_a_bridge() {
        let interface = |id, kind, minor| Interface {
            id,
            kind,
            device: {
                let mut device = [0; 16];
                device[..2].copy_from_slice(&[81, minor]);
                device
            },
            ..Default::default()
        };
        let pad = |id, entity| Pad {
            id,
            entity,
            ..Default::default()
        };
        let link = |source, sink, flags| Link {
            source,
            sink,
            flags,
            ..Default::default()
        };
        let mut graph = Graph {
            entities: vec![
                Entity {
                    id: 2,
                    function: SENSOR,
                    ..Default::default()
                },
                Entity {
                    id: 4,
                    function: SENSOR,
                    ..Default::default()
                },
            ],
            interfaces: vec![
                interface(11, VIDEO, 3),
                interface(12, SUBDEV, 7),
                interface(13, SUBDEV, 8),
            ],
            pads: vec![pad(21, 1), pad(22, 2), pad(23, 3), pad(24, 3), pad(25, 4)],
            links: vec![
                link(11, 1, INTERFACE_LINK),
                link(12, 2, INTERFACE_LINK),
                link(13, 4, INTERFACE_LINK),
                link(22, 23, ENABLED),
                link(24, 21, ENABLED),
                link(25, 23, 0),
            ],
        };
        assert_eq!(
            graph.upstream_sensor_nodes((81, 3)).unwrap(),
            BTreeSet::from([(81, 7)])
        );
        assert!(graph.upstream_sensor_nodes((81, 0)).unwrap().is_empty());
        graph.links.last_mut().unwrap().flags = ENABLED;
        assert!(graph.upstream_sensor_nodes((81, 3)).is_err());
    }
}
