//! Complete, bounded raw reads against scripted loopback HTTP replies.

use openchamber_native_transport::client::ServerClient;
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::error::TransportError;
use openchamber_native_transport::files::FilesError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn server(reply: Vec<u8>) -> (ServerClient, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("raw request");
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).await.expect("request header");
            request.push(byte[0]);
            assert!(request.len() <= 65536, "request headers remain bounded");
        }
        // A policy refusal may close before the entire scripted body is sent.
        let _ = socket.write_all(&reply).await;
        String::from_utf8(request).expect("ASCII HTTP request")
    });
    let client = ServerClient::new(
        Endpoint::parse(&format!("http://{address}/prefix/")).expect("endpoint"),
        Credential::ClientToken("raw-fixture-token".to_owned()),
    )
    .expect("client");
    (client, task)
}

fn reply(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut reply = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    reply.extend_from_slice(body);
    reply
}

#[tokio::test]
async fn raw_bytes_preserve_binary_and_capture_auth_path_directory_without_outside_permission() {
    let original = [0xff, 0, 0x80, b'P', b'K'];
    for mime in [
        "application/octet-stream",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "image/heic",
    ] {
        let (client, request) = server(reply(
            "200 OK",
            &format!("Content-Type: {mime}\r\nContent-Length: 5\r\n"),
            &original,
        ))
        .await;
        let directory = "/work/my project/ü?x=%&y=1";
        let path = "/work/my project/ü?x=%&y=1/report #1.heic";
        let bytes = client
            .read_raw_file(directory, path, original.len())
            .await
            .expect("complete bounded binary");
        assert_eq!(bytes.bytes(), original);
        assert_eq!(bytes.content_type(), Some(mime));
        assert_eq!(bytes.into_bytes(), original);
        let request = request.await.expect("scripted server");
        let target = request
            .lines()
            .next()
            .expect("request line")
            .split_whitespace()
            .nth(1)
            .expect("target");
        let url = reqwest::Url::parse(&format!("http://h{target}")).expect("request URL");
        assert_eq!(url.path(), "/prefix/api/fs/raw");
        assert_eq!(
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>(),
            [
                ("path".to_owned(), path.to_owned()),
                ("directory".to_owned(), directory.to_owned())
            ]
        );
        let headers = request.to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer raw-fixture-token\r\n"));
        assert!(headers.contains(
            "x-opencode-directory: %2fwork%2fmy%20project%2f%c3%bc%3fx%3d%25%26y%3d1\r\n"
        ));
        assert!(headers.contains("x-opencode-directory-encoding: uri\r\n"));
        assert!(!headers.contains("\r\nrange:"));
        assert!(!headers.contains("allowoutsideworkspace"));
    }
}

#[tokio::test]
async fn raw_reads_refuse_partial_truncated_advertised_and_streamed_oversize_answers() {
    for (response, too_large) in [
        (
            reply("206 Partial Content", "Content-Length: 2\r\n", b"ok"),
            false,
        ),
        (
            reply(
                "200 OK",
                "Content-Length: 2\r\nContent-Range: bytes 0-1/9\r\n",
                b"ok",
            ),
            false,
        ),
        (reply("200 OK", "Content-Length: 9\r\n", b"ok"), false),
        (reply("200 OK", "Content-Length: 11\r\n", b""), true),
        (
            reply(
                "200 OK",
                "Transfer-Encoding: chunked\r\n",
                b"b\r\n12345678901\r\n0\r\n\r\n",
            ),
            true,
        ),
        (reply("204 No Content", "", b""), false),
    ] {
        let (client, task) = server(response).await;
        let error = client
            .read_raw_file("/work", "/work/private-office.docx", 10)
            .await
            .expect_err("incomplete bytes grant no preparation input");
        assert_eq!(matches!(error, FilesError::RawFileTooLarge), too_large);
        assert!(
            !format!("{error:?}").contains("private-office"),
            "errors omit captured paths"
        );
        task.await.expect("scripted server");
    }
}

#[tokio::test]
async fn raw_empty_and_chunked_complete_answers_are_valid_and_401_signals_the_existing_watch() {
    for response in [
        reply("200 OK", "Content-Length: 0\r\n", b""),
        reply("200 OK", "Transfer-Encoding: chunked\r\n", b"0\r\n\r\n"),
    ] {
        let (client, task) = server(response).await;
        assert!(
            client
                .read_raw_file("/work", "/work/empty", 0)
                .await
                .expect("complete empty file")
                .bytes()
                .is_empty()
        );
        task.await.expect("scripted server");
    }
    let (client, task) = server(reply(
        "401 Unauthorized",
        "Content-Length: 100000000\r\n",
        b"private error content",
    ))
    .await;
    let watch = client.authentication_failures();
    assert!(matches!(
        client.read_raw_file("/work", "/work/file", 10).await,
        Err(FilesError::Transport(
            TransportError::AuthenticationRequired
        ))
    ));
    assert!(
        watch
            .has_changed()
            .expect("authentication watch remains open")
    );
    task.await.expect("scripted server");
}

#[tokio::test]
async fn raw_refusals_are_content_free_and_redirects_are_not_followed() {
    for status in ["302 Found", "404 Not Found", "500 Server Error"] {
        let (client, task) = server(reply(
            status,
            "Location: http://example.invalid/private\r\nContent-Length: 100000000\r\n",
            b"private server refusal",
        ))
        .await;
        let error = client
            .read_raw_file("/work", "/work/file", 10)
            .await
            .expect_err("refused raw read");
        assert!(!format!("{error:?}").contains("private"));
        if status.starts_with("302") {
            assert!(matches!(
                error,
                FilesError::Transport(TransportError::Redirected)
            ));
        }
        task.await.expect("one request without retry or redirect");
    }
}
