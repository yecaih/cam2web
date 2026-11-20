use super::error_reply;
use crate::runtime::{Output, Service};
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::stream;
use std::convert::Infallible;
use tokio::sync::watch;

const BOUNDARY: &str = "cam2web";

struct Multipart {
    output: watch::Receiver<Output>,
    session_id: u64,
    part: Part,
}

enum Part {
    AwaitFrame,
    Jpeg(Bytes),
    EndFrame,
}

impl Multipart {
    async fn next(mut self) -> Option<(Result<Bytes, Infallible>, Self)> {
        match std::mem::replace(&mut self.part, Part::AwaitFrame) {
            Part::Jpeg(jpeg) => {
                self.part = Part::EndFrame;
                return Some((Ok(jpeg), self));
            }
            Part::EndFrame => return Some((Ok(Bytes::from_static(b"\r\n")), self)),
            Part::AwaitFrame => {}
        }
        loop {
            self.output.changed().await.ok()?;
            let output = self.output.borrow_and_update().clone();
            if output.session_id != Some(self.session_id) {
                return None;
            }
            let Some(jpeg) = output.jpeg else {
                continue;
            };
            let headers = format!(
                "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            );
            self.part = Part::Jpeg(jpeg);
            return Some((Ok(Bytes::from(headers)), self));
        }
    }
}

pub(super) async fn stream_preview(
    State(service): State<Service>,
    Path(session_id): Path<u64>,
) -> Response {
    let mut output = service.subscribe();
    if output.borrow().session_id != Some(session_id) {
        return error_reply(StatusCode::GONE, "preview session ended; reload status");
    }
    output.mark_changed();
    let body = stream::unfold(
        Multipart {
            output,
            session_id,
            part: Part::AwaitFrame,
        },
        Multipart::next,
    );
    (
        [
            (
                header::CONTENT_TYPE,
                "multipart/x-mixed-replace; boundary=cam2web",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn multipart_sends_latest_frame_and_ends_on_session_change() {
        let (sender, receiver) = watch::channel(Output {
            session_id: Some(42),
            ..Output::default()
        });
        let multipart = Multipart {
            output: receiver,
            session_id: 42,
            part: Part::AwaitFrame,
        };
        for jpeg in [b"old".as_slice(), b"latest".as_slice()] {
            sender.send_replace(Output {
                session_id: Some(42),
                jpeg: Some(Bytes::copy_from_slice(jpeg)),
                ..Output::default()
            });
        }
        let (headers, multipart) = multipart.next().await.expect("headers");
        assert_eq!(
            headers.unwrap(),
            "--cam2web\r\nContent-Type: image/jpeg\r\nContent-Length: 6\r\n\r\n"
        );
        let (jpeg, multipart) = multipart.next().await.expect("JPEG");
        assert_eq!(jpeg.unwrap(), "latest");
        let (trailer, multipart) = multipart.next().await.expect("trailer");
        assert_eq!(trailer.unwrap(), "\r\n");
        sender.send_replace(Output {
            session_id: Some(43),
            jpeg: Some(Bytes::from_static(b"new session")),
            ..Output::default()
        });
        assert!(multipart.next().await.is_none());
    }
}
