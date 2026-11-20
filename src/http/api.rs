use super::error_reply;
use crate::{
    runtime::{Command, ControlValue, Failure, PreviewTarget, Service, Target},
    v4l::{ControlGroup, FormatRequest},
};
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/api/nodes", get(nodes))
        .route("/api/status", get(status))
        .route("/api/select", post(select))
        .route("/api/start", post(start))
        .route("/api/stop", post(stop))
        .route("/api/intervals", post(intervals))
        .route("/api/controls", get(user_controls))
        .route("/api/control-values", post(control_values))
        .route("/api/control", post(user_control))
        .route("/api/camera-controls", get(camera_controls))
        .route("/api/camera-control", post(camera_control))
        .route("/api/parameters", get(parameters))
        .route("/api/parameter", post(parameter))
        .route("/api/metadata", get(metadata))
}

async fn nodes(State(service): State<Service>) -> Response {
    reply(&service, Command::Nodes).await
}
async fn status(State(service): State<Service>) -> Response {
    reply(&service, Command::Status).await
}
async fn metadata(State(service): State<Service>) -> Response {
    let receiver = service.subscribe();
    let output = receiver.borrow();
    let payload = match output.tof_metadata {
        Some(pair) => json!({
            "session_id": output.session_id,
            "sources": ["himax_tof"],
            "himax_tof": pair,
        }),
        None => json!({"session_id": output.session_id, "sources": []}),
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(payload)).into_response()
}
async fn user_controls(State(service): State<Service>) -> Response {
    reply_result(service.request_controls(ControlGroup::User).await)
}
async fn camera_controls(State(service): State<Service>) -> Response {
    reply_result(service.request_controls(ControlGroup::Camera).await)
}
async fn control_values(State(service): State<Service>, payload: JsonRequest<Target>) -> Response {
    match payload {
        Ok(Json(target)) => reply_result(service.request_control_values(target).await),
        Err(error) => error_reply(error.status(), error.body_text()),
    }
}
async fn parameters(State(service): State<Service>) -> Response {
    reply(&service, Command::Parameters).await
}

#[derive(Deserialize)]
struct Select {
    node_id: String,
}
#[derive(Deserialize)]
struct Start {
    #[serde(flatten)]
    target: Target,
    format: Option<FormatRequest>,
}
#[derive(Deserialize)]
struct Intervals {
    #[serde(flatten)]
    target: Target,
    format: FormatRequest,
}
#[derive(Deserialize)]
struct Control {
    #[serde(flatten)]
    target: Target,
    id: u32,
    value: ControlValue,
}
#[derive(Deserialize)]
struct Parameter {
    #[serde(flatten)]
    target: PreviewTarget,
    instance_id: String,
    parameter_id: String,
    value: Value,
}

async fn select(State(service): State<Service>, payload: JsonRequest<Select>) -> Response {
    mutate(&service, payload, |request| {
        Command::Select(request.node_id)
    })
    .await
}
async fn start(State(service): State<Service>, payload: JsonRequest<Start>) -> Response {
    mutate(&service, payload, |request| {
        Command::Start(request.target, request.format)
    })
    .await
}
async fn stop(State(service): State<Service>, payload: JsonRequest<PreviewTarget>) -> Response {
    mutate(&service, payload, Command::Stop).await
}
async fn intervals(State(service): State<Service>, payload: JsonRequest<Intervals>) -> Response {
    mutate(&service, payload, |request| {
        Command::Intervals(request.target, request.format)
    })
    .await
}
async fn user_control(State(service): State<Service>, payload: JsonRequest<Control>) -> Response {
    set_control(&service, payload, ControlGroup::User).await
}
async fn camera_control(State(service): State<Service>, payload: JsonRequest<Control>) -> Response {
    set_control(&service, payload, ControlGroup::Camera).await
}
async fn set_control(
    service: &Service,
    payload: JsonRequest<Control>,
    group: ControlGroup,
) -> Response {
    mutate(service, payload, |request| {
        Command::Control(group, request.target, request.id, request.value)
    })
    .await
}
async fn parameter(State(service): State<Service>, payload: JsonRequest<Parameter>) -> Response {
    mutate(&service, payload, |request| {
        Command::Parameter(
            request.target,
            request.instance_id,
            request.parameter_id,
            request.value,
        )
    })
    .await
}

type JsonRequest<T> = Result<Json<T>, JsonRejection>;

async fn mutate<T>(
    service: &Service,
    payload: JsonRequest<T>,
    command: impl FnOnce(T) -> Command,
) -> Response {
    match payload {
        Ok(Json(request)) => reply(service, command(request)).await,
        Err(error) => error_reply(error.status(), error.body_text()),
    }
}

async fn reply(service: &Service, command: Command) -> Response {
    reply_result(service.request(command).await)
}

fn reply_result(result: Result<Value, Failure>) -> Response {
    match result {
        Ok(payload) => ([(header::CACHE_CONTROL, "no-store")], Json(payload)).into_response(),
        Err(Failure::Invalid(error)) => error_reply(StatusCode::UNPROCESSABLE_ENTITY, error),
        Err(Failure::Stale) => error_reply(
            StatusCode::CONFLICT,
            "selection or preview changed; reload state before trying again",
        ),
        Err(Failure::Unavailable) => error_reply(
            StatusCode::SERVICE_UNAVAILABLE,
            "camera command queue is busy or offline",
        ),
    }
}
