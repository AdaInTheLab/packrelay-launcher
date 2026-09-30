// Shared helpers for packrelay-core's integration tests.

#![allow(dead_code)] // each test binary uses a different subset

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Minimal HTTP/1.1 server answering GETs from a fixed route table
/// (anything else is a 404). Returns its base URL.
pub async fn serve(routes: Vec<(String, String)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut chunk = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&req);
                let target = req.split_whitespace().nth(1).unwrap_or_default();
                // Routes are paths; a query string (the manifest fetch's
                // ?game=) doesn't change which one answers.
                let path = target.split('?').next().unwrap_or_default();
                let (status, body) = match routes.iter().find(|(p, _)| p == path) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

/// Self-deleting temp dir (no tempfile dependency).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "packrelay-test-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
