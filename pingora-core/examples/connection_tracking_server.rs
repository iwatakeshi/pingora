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

//! Example: HTTP server with connection tracking for graceful hot restart
//!
//! This example demonstrates:
//! 1. Integration of ConnectionCounter for tracking active connections
//! 2. WebSocket upgrade detection and classification
//! 3. Graceful shutdown with connection draining
//! 4. Hot restart with zero-downtime for short-lived connections
//!
//! Run this example:
//! ```bash
//! cargo run --example connection_tracking_server
//! ```
//!
//! Test graceful shutdown:
//! ```bash
//! # In another terminal, send SIGTERM
//! kill -TERM <pid>
//!
//! # For hot restart, start new process first
//! cargo run --example connection_tracking_server -- --upgrade &
//! # Then signal old process
//! kill -QUIT <old_pid>
//! ```

use async_trait::async_trait;
use log::info;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service as ListeningService;
use pingora_core::apps::http_app::ServeHttp;
use pingora_core::protocols::http::ServerSession;
use pingora_core::server::connection_counter::{ConnectionCounter, ConnectionGuard, ConnectionType};
use pingora_core::protocols::http::websocket::{detect_connection_type, is_websocket_upgrade};


/// Example HTTP application with connection tracking
pub struct TrackedHttpApp {
    counter: Arc<ConnectionCounter>,
}

impl TrackedHttpApp {
    pub fn new(counter: Arc<ConnectionCounter>) -> Self {
        Self { counter }
    }

    async fn handle_http_request(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        // Track this HTTP connection
        let _guard = ConnectionGuard::new((*self.counter).clone(), ConnectionType::Http);
        
        info!(
            "Handling HTTP request: {} - Active connections: {} (HTTP: {}, WS: {})",
            session.request_summary(),
            self.counter.active_count(),
            self.counter.http_count(),
            self.counter.websocket_count()
        );

        // Simulate some processing time
        sleep(Duration::from_millis(100)).await;

        http::Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(b"Hello from Pingora with connection tracking!\n".to_vec())
            .unwrap()
    }

    async fn handle_websocket_upgrade(&self, _session: &mut ServerSession) -> http::Response<Vec<u8>> {
        // Track WebSocket connection (will be upgraded to bidirectional)
        let _guard = ConnectionGuard::new((*self.counter).clone(), ConnectionType::WebSocket);
        
        info!(
            "WebSocket upgrade request - Active connections: {} (HTTP: {}, WS: {})",
            self.counter.active_count(),
            self.counter.http_count(),
            self.counter.websocket_count()
        );

        // In a real implementation, you would upgrade the connection here
        // For this example, we'll just acknowledge the upgrade request
        http::Response::builder()
            .status(101)
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Accept", "dummy-accept-key")
            .body(Vec::new())
            .unwrap()
    }

    async fn handle_sse_request(&self, _session: &mut ServerSession) -> http::Response<Vec<u8>> {
        // Track Server-Sent Events connection
        let _guard = ConnectionGuard::new((*self.counter).clone(), ConnectionType::ServerSentEvents);
        
        info!(
            "SSE request - Active connections: {} (SSE: {})",
            self.counter.active_count(),
            self.counter.streaming_count()
        );

        // Simulate SSE stream
        let mut events = Vec::new();
        for i in 0..3 {
            events.extend_from_slice(format!("data: Event {}\n\n", i).as_bytes());
        }

        http::Response::builder()
            .status(200)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(events)
            .unwrap()
    }
}

#[async_trait]
impl ServeHttp for TrackedHttpApp {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let req = session.req_header();
        
        // Detect connection type based on headers
        let conn_type = detect_connection_type(req, None);
        
        match conn_type {
            ConnectionType::WebSocket if is_websocket_upgrade(req) => {
                self.handle_websocket_upgrade(session).await
            }
            ConnectionType::ServerSentEvents => {
                self.handle_sse_request(session).await
            }
            _ => {
                // Handle as regular HTTP
                self.handle_http_request(session).await
            }
        }
    }
}

/// Monitor and log connection statistics
async fn connection_monitor(counter: Arc<ConnectionCounter>) {
    loop {
        sleep(Duration::from_secs(10)).await;
        
        let active = counter.active_count();
        if active > 0 {
            info!(
                "Connection stats - Total: {}, HTTP: {}, WebSocket: {}, Streaming: {}, Has long-lived: {}",
                active,
                counter.http_count(),
                counter.websocket_count(),
                counter.streaming_count(),
                counter.has_long_lived_connections()
            );
        }
    }
}

fn main() {
    env_logger::init();

    // Parse command line options
    let opt = Some(Opt::parse_args());
    let mut server = Server::new(opt).unwrap();

    // Get the server's connection counter - this is the proper way to integrate
    let counter = server.connection_counter().clone();
    
    info!(
        "Server initialized with connection tracking. Active connections: {}",
        server.active_connection_count()
    );

    // Create our application with connection tracking
    let app = TrackedHttpApp::new(counter.clone());
    
    // Create the listening service
    let mut http_service = ListeningService::new(
        "HTTP Service with Connection Tracking".to_string(),
        app
    );
    
    // Listen on port 6190
    http_service.add_tcp("0.0.0.0:6190");
    
    server.add_service(http_service);

    // Spawn a background task to monitor connections
    let monitor_counter = counter.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(connection_monitor(monitor_counter));
    });

    info!("Server starting on 0.0.0.0:6190");
    info!("Try:");
    info!("  curl http://localhost:6190/");
    info!("  curl -H 'Connection: Upgrade' -H 'Upgrade: websocket' http://localhost:6190/");
    info!("");
    info!("Graceful shutdown: kill -TERM <pid>");
    info!("Hot restart: ./example --upgrade & && kill -QUIT <old_pid>");

    server.run_forever();
}
