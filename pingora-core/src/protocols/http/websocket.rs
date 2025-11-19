// Copyright 2025 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! WebSocket detection utilities for proper connection handling during hot restart

use pingora_http::{RequestHeader, ResponseHeader};

use crate::server::connection_counter::ConnectionType;

/// Check if an HTTP request is a WebSocket upgrade request
///
/// Based on [RFC 6455](https://tools.ietf.org/html/rfc6455), a valid WebSocket upgrade request must include:
/// - GET method
/// - Upgrade: websocket header
/// - Connection: Upgrade header
/// - Sec-WebSocket-Key header
/// - Sec-WebSocket-Version: 13 header
pub fn is_websocket_upgrade(req: &RequestHeader) -> bool {
    if req.method != "GET" {
        return false;
    }

    // Check for required WebSocket upgrade headers
    let has_upgrade = req
        .headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    let has_connection = req
        .headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    let has_ws_key = req.headers.contains_key("sec-websocket-key");
    let has_ws_version = req.headers.contains_key("sec-websocket-version");

    has_upgrade && has_connection && has_ws_key && has_ws_version
}

/// Check if an HTTP response indicates a successful WebSocket upgrade
///
/// A successful WebSocket upgrade response must have:
/// - 101 Switching Protocols status
/// - Upgrade: websocket header
/// - Connection: Upgrade header
pub fn is_websocket_upgrade_response(resp: &ResponseHeader) -> bool {
    if resp.status != 101 {
        return false;
    }

    let has_upgrade = resp
        .headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    let has_connection = resp
        .headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    has_upgrade && has_connection
}

/// Detect the connection type based on request and response headers
///
/// This helps classify connections for appropriate handling during graceful shutdown:
/// - WebSocket: Long-lived bidirectional connections
/// - ServerSentEvents: Long-lived server-to-client streaming
/// - GrpcStream: gRPC streaming connections
/// - Http: Regular HTTP request/response
pub fn detect_connection_type(
    req: &RequestHeader,
    resp: Option<&ResponseHeader>,
) -> ConnectionType {
    // Check for WebSocket upgrade
    if is_websocket_upgrade(req) {
        if let Some(response) = resp {
            if is_websocket_upgrade_response(response) {
                return ConnectionType::WebSocket;
            }
        } else {
            // Request looks like WebSocket, pending response
            return ConnectionType::WebSocket;
        }
    }

    // Check for Server-Sent Events
    if let Some(response) = resp {
        if let Some(content_type) = response
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            if content_type.contains("text/event-stream") {
                return ConnectionType::ServerSentEvents;
            }
        }
    }

    // Check for gRPC streaming (Content-Type: application/grpc)
    if let Some(content_type) = req
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
    {
        if content_type.starts_with("application/grpc") {
            // Further check for streaming indicators
            if req
                .headers
                .get("te")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("trailers"))
                .unwrap_or(false)
            {
                return ConnectionType::GrpcStream;
            }
        }
    }

    ConnectionType::Http
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_http::RequestHeader;

    fn create_websocket_request() -> RequestHeader {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        req.insert_header("upgrade", "websocket").unwrap();
        req.insert_header("connection", "Upgrade").unwrap();
        req.insert_header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .unwrap();
        req.insert_header("sec-websocket-version", "13").unwrap();
        req
    }

    fn create_websocket_response() -> ResponseHeader {
        let mut resp = ResponseHeader::build(101, None).unwrap();
        resp.insert_header("upgrade", "websocket").unwrap();
        resp.insert_header("connection", "Upgrade").unwrap();
        resp.insert_header(
            "sec-websocket-accept",
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        )
        .unwrap();
        resp
    }

    #[test]
    fn test_websocket_detection() {
        let req = create_websocket_request();
        assert!(is_websocket_upgrade(&req));

        let resp = create_websocket_response();
        assert!(is_websocket_upgrade_response(&resp));
    }

    #[test]
    fn test_regular_http() {
        let req = RequestHeader::build("GET", b"/", None).unwrap();
        assert!(!is_websocket_upgrade(&req));
    }

    #[test]
    fn test_connection_type_detection() {
        let ws_req = create_websocket_request();
        let ws_resp = create_websocket_response();

        assert_eq!(
            detect_connection_type(&ws_req, Some(&ws_resp)),
            ConnectionType::WebSocket
        );

        let http_req = RequestHeader::build("GET", b"/", None).unwrap();
        assert_eq!(
            detect_connection_type(&http_req, None),
            ConnectionType::Http
        );
    }

    #[test]
    fn test_sse_detection() {
        let req = RequestHeader::build("GET", b"/events", None).unwrap();
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.insert_header("content-type", "text/event-stream")
            .unwrap();

        assert_eq!(
            detect_connection_type(&req, Some(&resp)),
            ConnectionType::ServerSentEvents
        );
    }

    #[test]
    fn test_grpc_detection() {
        let mut req = RequestHeader::build("POST", b"/service.Method", None).unwrap();
        req.insert_header("content-type", "application/grpc").unwrap();
        req.insert_header("te", "trailers").unwrap();

        assert_eq!(detect_connection_type(&req, None), ConnectionType::GrpcStream);
    }

    #[test]
    fn test_websocket_post_not_upgrade() {
        // POST request shouldn't be WebSocket
        let mut req = RequestHeader::build("POST", b"/", None).unwrap();
        req.insert_header("upgrade", "websocket").unwrap();
        req.insert_header("connection", "Upgrade").unwrap();

        assert!(!is_websocket_upgrade(&req));
    }
}
