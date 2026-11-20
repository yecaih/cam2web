mod api;
mod mjpeg;
mod transport;

use crate::runtime::Service;
use axum::{
    Json, Router,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use std::io;
use tokio::net::TcpListener;

pub async fn serve(listener: TcpListener, service: Service) -> io::Result<()> {
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../web/index.html")) }),
        )
        .route(
            "/assets/cam2web.css",
            get(|| async {
                asset(
                    "text/css; charset=utf-8",
                    include_str!("../../web/assets/cam2web.css"),
                )
            }),
        )
        .route(
            "/assets/cam2web.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../../web/assets/cam2web.js"),
                )
            }),
        )
        .merge(api::routes())
        .route("/stream/{session_id}", get(mjpeg::stream_preview))
        .with_state(service);
    axum::serve(transport::TimedListener(listener), app).await
}

fn asset(content_type: &'static str, source: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        source,
    )
        .into_response()
}

fn error_reply(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error": message.into()})),
    )
        .into_response()
}
