mod config;
mod http;
mod pipeline;
mod runtime;
mod v4l;

use anyhow::{Context, Result, ensure};
use config::Config;
use runtime::Service;
use std::{env, path::PathBuf};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map_or_else(|| PathBuf::from("config"), PathBuf::from);
    ensure!(args.next().is_none(), "usage: cam2web [config]");
    let config = Config::load(&path)?;
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listen on {}", config.listen))?;
    let service = Service::spawn(config.devices)?;
    eprintln!("cam2web listening on http://{}", listener.local_addr()?);
    http::serve(listener, service).await.context("serve HTTP")
}
