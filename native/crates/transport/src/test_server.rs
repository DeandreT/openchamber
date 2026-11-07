//! A loopback stand-in for OpenChamber server routes in transport tests: it
//! answers each request through a route function and records it.

use std::sync::{Arc, Mutex};

use reqwest::Url;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::client::ServerClient;

/// A request the fake server received.
#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub directory: Option<String>,
    pub body: Value,
}

impl Recorded {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) type Route = Box<dyn Fn(&Recorded) -> (u16, Value) + Send + Sync>;

/// Serves `route` on loopback until the test's runtime ends.
pub(crate) async fn serve(route: Route) -> (ServerClient, Arc<Mutex<Vec<Recorded>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let log = recorded.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let end = loop {
                let mut buffer = [0_u8; 4096];
                let count = socket.read(&mut buffer).await.expect("request");
                assert!(count > 0, "request must have headers");
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let head = String::from_utf8(bytes[..end].to_vec()).expect("ASCII headers");
            let header = |name: &str| {
                head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_owned())
                })
            };
            assert_eq!(
                header("authorization").as_deref(),
                Some("Bearer oc_client_synthetic"),
                "every request carries the client token"
            );
            let length = header("content-length")
                .map(|value| value.parse::<usize>().expect("length"))
                .unwrap_or_default();
            while bytes.len() < end + length {
                let mut buffer = [0_u8; 4096];
                let count = socket.read(&mut buffer).await.expect("body");
                bytes.extend_from_slice(&buffer[..count]);
            }
            let line = head.lines().next().expect("request line");
            let mut parts = line.split(' ');
            let method = parts.next().unwrap_or_default().to_owned();
            let target = Url::parse(&format!("http://fake{}", parts.next().unwrap_or("/")))
                .expect("request target");
            let request = Recorded {
                method,
                path: target.path().to_owned(),
                query: target.query_pairs().into_owned().collect(),
                directory: header("x-opencode-directory"),
                body: if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + length]).expect("JSON body")
                },
            };
            let (status, body) = route(&request);
            log.lock().expect("log").push(request);
            let body = body.to_string();
            let reply = format!(
                "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    let endpoint =
        crate::endpoint::Endpoint::parse(&format!("http://{address}/")).expect("test endpoint");
    let client = ServerClient::new(
        endpoint,
        crate::credential::Credential::ClientToken("oc_client_synthetic".into()),
    )
    .expect("test client");
    (client, recorded)
}
