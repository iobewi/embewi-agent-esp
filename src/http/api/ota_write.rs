//! `PUT /v1alpha1/ota/write` (contrat §4): the one route that can't use
//! picoserve's usual `String`/`Form` body extractors -- those buffer the
//! *entire* body before a handler ever runs, and an OTA image (hundreds of
//! KB) doesn't fit this device's heap. Implements `RequestHandlerService`
//! directly instead, streaming the body straight into flash one `read()`
//! at a time.

use alloc::format;
use alloc::string::String;

use picoserve::io::Read;
use picoserve::request::Request;
use picoserve::response::{IntoResponse, ResponseWriter, StatusCode};
use picoserve::routing::RequestHandlerService;
use picoserve::ResponseSent;

use crate::{agent, ota};
use crate::storage::SharedStorage;

use crate::http::{JsonResponse, json_error, json_ok, unauthorized};
use ota::parse_content_range;

pub struct OtaWrite {
    pub storage: &'static SharedStorage,
}

impl RequestHandlerService for OtaWrite {
    async fn call_request_handler_service<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        _state: &(),
        _path_parameters: (),
        mut request: Request<'_, R>,
        response_writer: W,
    ) -> Result<ResponseSent, W::Error> {
        let headers = request.parts.headers();
        let token = headers
            .get("authorization")
            .and_then(|v| v.as_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !agent::is_authorized(self.storage, token).await {
            return unauthorized()
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        let deployment_id = headers.get("x-embewi-deployment-id").and_then(|v| v.as_str().ok()).unwrap_or("");
        let expected_digest = headers.get("x-embewi-digest").and_then(|v| v.as_str().ok()).unwrap_or("");
        let content_range = headers.get("content-range").and_then(|v| v.as_str().ok());

        let (has_range, start, end, total) = match content_range {
            None => (false, 0u32, 0u32, 0u32),
            Some(value) => match parse_content_range(value) {
                Some((s, e, t)) => (true, s, e, t),
                None => {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"bad_content_range\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
            },
        };

        let in_progress = ota::write_in_progress().await;
        let written_so_far = ota::write_written().await;
        match ota::write_plan(has_range, start, in_progress, written_so_far) {
            ota::Plan::Begin => {
                if !ota::write_begin(self.storage).await {
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"ota_begin_failed\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
            }
            ota::Plan::Resync => {
                let written = ota::write_written().await;
                return json_error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    &format!("{{\"error\":\"range_mismatch\",\"written\":{written}}}"),
                )
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
            }
            ota::Plan::Continue => {}
        }

        let content_length = request.body_connection.content_length();
        let mut buf = [0u8; 1024];
        let mut remaining = content_length;
        let mut chunk_error = false;
        {
            let mut reader = request.body_connection.body().reader();
            while remaining > 0 {
                let to_read = remaining.min(buf.len());
                let n = reader.read(&mut buf[..to_read]).await?;
                if n == 0 {
                    chunk_error = true;
                    break;
                }
                if !ota::write_chunk(self.storage, &buf[..n]).await {
                    chunk_error = true;
                    break;
                }
                remaining -= n;
            }
        }

        if chunk_error {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"write_failed\"}")
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        if !ota::write_is_final(has_range, end, total) {
            let written = ota::write_written().await;
            return json_ok(format!("{{\"status\":\"partial\",\"written\":{written}}}"))
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        let response: JsonResponse = match ota::write_finish(self.storage, expected_digest, deployment_id).await {
            Ok(result) => json_ok(format!(
                "{{\"written\":{},\"digest\":\"{}\",\"status\":\"written\"}}",
                result.written, result.digest
            )),
            Err(ota::WriteFinishError::DigestMismatch) => json_ok(String::from("{\"status\":\"digest_mismatch\"}")),
            Err(ota::WriteFinishError::NotWriting) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"write_failed\"}")
            }
        };
        response
            .write_to(request.body_connection.finalize().await?, response_writer)
            .await
    }
}
