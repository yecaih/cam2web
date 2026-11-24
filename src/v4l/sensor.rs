use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, RawFd},
    sync::mpsc::{SyncSender, sync_channel},
    thread,
};

use anyhow::{Context, Result};
use tokio::sync::oneshot;

pub(super) struct SensorControls {
    file: File,
    jobs: SyncSender<Job>,
}

type Job = Box<dyn FnOnce(RawFd) + Send>;

impl SensorControls {
    pub(super) fn start(file: File) -> io::Result<Self> {
        let worker_file = file.try_clone()?;
        let (jobs, inbox) = sync_channel::<Job>(1);
        thread::Builder::new()
            .name("cam2web-sensor".into())
            .spawn(move || {
                for job in inbox {
                    job(worker_file.as_raw_fd());
                }
            })?;
        Ok(Self { file, jobs })
    }

    pub(super) fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub(super) fn submit<T: Send + 'static>(
        &self,
        operation: impl FnOnce(RawFd) -> Result<T> + Send + 'static,
    ) -> Result<oneshot::Receiver<Result<T>>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .try_send(Box::new(move |fd| {
                if !reply.is_closed() {
                    let _ = reply.send(operation(fd));
                }
            }))
            .ok()
            .context("sensor control worker busy or stopped")?;
        Ok(result)
    }
}
