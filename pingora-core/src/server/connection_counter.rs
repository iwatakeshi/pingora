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

//! Connection tracking for graceful shutdown and hot restart
//!
//! This module provides atomic connection counting to track active connections
//! during graceful shutdown and hot restart scenarios.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Connection type classification for specialized handling during shutdown
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    /// Regular HTTP/HTTPS connection
    Http,
    /// WebSocket or other long-lived connection
    WebSocket,
    /// gRPC streaming connection
    GrpcStream,
    /// Server-Sent Events (SSE) connection
    ServerSentEvents,
}

/// Tracks active connections across the server
///
/// This type is `Send + Sync` and can be safely shared across threads.
/// All operations use atomic operations for lock-free concurrent access.
#[derive(Clone)]
pub struct ConnectionCounter {
    /// Total active connections
    active_connections: Arc<AtomicUsize>,
    /// Active WebSocket connections (subset of active_connections)
    active_websockets: Arc<AtomicUsize>,
    /// Active streaming connections (gRPC, SSE, etc.)
    active_streams: Arc<AtomicUsize>,
}

impl Default for ConnectionCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionCounter {
    /// Create a new connection counter
    pub fn new() -> Self {
        Self {
            active_connections: Arc::new(AtomicUsize::new(0)),
            active_websockets: Arc::new(AtomicUsize::new(0)),
            active_streams: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Increment connection count for a new connection
    pub fn inc(&self, conn_type: ConnectionType) {
        // Relaxed ordering is sufficient: we only need eventual consistency
        // for metrics, not strict happens-before relationships
        let prev = self.active_connections.fetch_add(1, Ordering::Relaxed);
        if prev == usize::MAX {
            log::error!("Connection counter overflow detected!");
        }
        match conn_type {
            ConnectionType::WebSocket => {
                self.active_websockets.fetch_add(1, Ordering::Relaxed);
            }
            ConnectionType::GrpcStream | ConnectionType::ServerSentEvents => {
                self.active_streams.fetch_add(1, Ordering::Relaxed);
            }
            ConnectionType::Http => {}
        }
    }

    /// Decrement connection count when a connection closes
    pub fn dec(&self, conn_type: ConnectionType) {
        // Relaxed ordering is sufficient for decrement operations
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
        match conn_type {
            ConnectionType::WebSocket => {
                self.active_websockets.fetch_sub(1, Ordering::Relaxed);
            }
            ConnectionType::GrpcStream | ConnectionType::ServerSentEvents => {
                self.active_streams.fetch_sub(1, Ordering::Relaxed);
            }
            ConnectionType::Http => {}
        }
    }

    /// Get total number of active connections
    ///
    /// # Examples
    ///
    /// ```
    /// use pingora_core::server::connection_counter::{ConnectionCounter, ConnectionType};
    ///
    /// let counter = ConnectionCounter::new();
    /// assert_eq!(counter.active_count(), 0);
    ///
    /// counter.inc(ConnectionType::Http);
    /// assert_eq!(counter.active_count(), 1);
    /// ```
    pub fn active_count(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Get number of active WebSocket connections
    pub fn websocket_count(&self) -> usize {
        self.active_websockets.load(Ordering::Relaxed)
    }

    /// Get number of active streaming connections (non-WebSocket)
    pub fn streaming_count(&self) -> usize {
        self.active_streams.load(Ordering::Relaxed)
    }

    /// Get number of regular HTTP connections (short-lived)
    ///
    /// Note: This is a best-effort calculation. Due to concurrent modifications
    /// by other threads, the value may be slightly inconsistent in high-concurrency
    /// scenarios. The returned value represents a snapshot that may not reflect
    /// simultaneous changes across the individual counters.
    pub fn http_count(&self) -> usize {
        let total = self.active_count();
        let ws = self.websocket_count();
        let stream = self.streaming_count();
        total.saturating_sub(ws + stream)
    }

    /// Check if there are any long-lived connections active
    pub fn has_long_lived_connections(&self) -> bool {
        self.websocket_count() > 0 || self.streaming_count() > 0
    }
}

/// RAII guard for automatic connection count decrement
///
/// When this guard is dropped, it automatically decrements the connection count.
/// This ensures connections are properly tracked even if the handler panics.
pub struct ConnectionGuard {
    counter: ConnectionCounter,
    conn_type: ConnectionType,
    active: bool,
}

impl ConnectionGuard {
    /// Create a new connection guard and increment the counter
    pub fn new(counter: ConnectionCounter, conn_type: ConnectionType) -> Self {
        counter.inc(conn_type);
        Self {
            counter,
            conn_type,
            active: true,
        }
    }

    /// Manually release the guard without decrementing (useful for connection handoff)
    pub fn release(&mut self) {
        self.active = false;
    }

    /// Get the connection type
    pub fn connection_type(&self) -> ConnectionType {
        self.conn_type
    }

    /// Check if this guard is still active
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Get a reference to the underlying counter
    pub fn counter(&self) -> &ConnectionCounter {
        &self.counter
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.active {
            self.counter.dec(self.conn_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_counter() {
        let counter = ConnectionCounter::new();
        assert_eq!(counter.active_count(), 0);

        counter.inc(ConnectionType::Http);
        assert_eq!(counter.active_count(), 1);
        assert_eq!(counter.http_count(), 1);

        counter.inc(ConnectionType::WebSocket);
        assert_eq!(counter.active_count(), 2);
        assert_eq!(counter.websocket_count(), 1);

        counter.dec(ConnectionType::Http);
        assert_eq!(counter.active_count(), 1);
        assert_eq!(counter.http_count(), 0);

        counter.dec(ConnectionType::WebSocket);
        assert_eq!(counter.active_count(), 0);
    }

    #[test]
    fn test_connection_guard() {
        let counter = ConnectionCounter::new();
        {
            let _guard = ConnectionGuard::new(counter.clone(), ConnectionType::Http);
            assert_eq!(counter.active_count(), 1);
        }
        assert_eq!(counter.active_count(), 0);
    }

    #[test]
    fn test_guard_release() {
        let counter = ConnectionCounter::new();
        {
            let mut guard = ConnectionGuard::new(counter.clone(), ConnectionType::Http);
            assert_eq!(counter.active_count(), 1);
            guard.release();
            assert_eq!(counter.active_count(), 1); // Not decremented
            assert!(!guard.is_active()); // Can still check status after release
        }
        assert_eq!(counter.active_count(), 1); // Still 1 after drop
    }

    #[test]
    fn test_has_long_lived() {
        let counter = ConnectionCounter::new();
        assert!(!counter.has_long_lived_connections());

        counter.inc(ConnectionType::Http);
        assert!(!counter.has_long_lived_connections());

        counter.inc(ConnectionType::WebSocket);
        assert!(counter.has_long_lived_connections());

        counter.dec(ConnectionType::WebSocket);
        assert!(!counter.has_long_lived_connections());
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;
        let counter = Arc::new(ConnectionCounter::new());
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let c = counter.clone();
                thread::spawn(move || {
                    for _ in 0..1000 {
                        c.inc(ConnectionType::Http);
                        c.dec(ConnectionType::Http);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.active_count(), 0, "All connections should be decremented");
    }

    #[test]
    fn test_mixed_connection_types_concurrent() {
        use std::thread;
        let counter = Arc::new(ConnectionCounter::new());
        let mut handles = vec![];

        // HTTP connections
        for _ in 0..5 {
            let c = counter.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    c.inc(ConnectionType::Http);
                    c.dec(ConnectionType::Http);
                }
            }));
        }

        // WebSocket connections
        for _ in 0..3 {
            let c = counter.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..50 {
                    c.inc(ConnectionType::WebSocket);
                    c.dec(ConnectionType::WebSocket);
                }
            }));
        }

        // Streaming connections
        for _ in 0..2 {
            let c = counter.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..30 {
                    c.inc(ConnectionType::GrpcStream);
                    c.dec(ConnectionType::GrpcStream);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.active_count(), 0);
        assert_eq!(counter.websocket_count(), 0);
        assert_eq!(counter.streaming_count(), 0);
    }
}
