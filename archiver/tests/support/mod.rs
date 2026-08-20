//! Shared scripted HTTP server support for integration tests.
#![allow(
    dead_code,
    reason = "each integration-test crate uses a different subset"
)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

/// Build a simple HTTP/1.1 response with a text body.
pub fn plain(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Serve a fixed number of connections, letting a script choose each response and retained note.
pub fn serve_with<N: Send + 'static>(
    connections: usize,
    script: impl Fn(&str) -> (Vec<u8>, N) + Send + 'static,
) -> std::io::Result<(u16, thread::JoinHandle<Vec<N>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let handle = thread::spawn(move || {
        let mut notes = Vec::with_capacity(connections);
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return notes;
            };
            let request = read_request_head(&mut stream);
            let (response, note) = script(&request);
            let _ = stream.write_all(&response);
            notes.push(note);
        }
        notes
    });

    Ok((port, handle))
}

/// Serve connections concurrently so one deliberately stalled response cannot block later ones.
pub fn serve_concurrently_with<N: Send + 'static>(
    connections: usize,
    script: impl Fn(usize, &str) -> (Vec<u8>, N) + Send + Sync + 'static,
) -> std::io::Result<(u16, thread::JoinHandle<Vec<N>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let script = Arc::new(script);

    let handle = thread::spawn(move || {
        let mut handlers = Vec::with_capacity(connections);
        for attempt in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let script = Arc::clone(&script);
            handlers.push(thread::spawn(move || {
                let request = read_request_head(&mut stream);
                let (response, note) = script(attempt, &request);
                let _ = stream.write_all(&response);
                note
            }));
        }
        handlers
            .into_iter()
            .filter_map(|handler| handler.join().ok())
            .collect()
    });

    Ok((port, handle))
}

/// Read one request's header section from a stream.
pub fn read_request_head(stream: &mut (impl Read + Write)) -> String {
    let mut head = Vec::new();
    let mut buffer = [0; 4096];

    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
        }
    }

    String::from_utf8_lossy(&head).into_owned()
}

/// Return the target path from a request head.
pub fn request_path(head: &str) -> &str {
    head.split(' ').nth(1).unwrap_or("/")
}

/// Return a lowercased request-header value.
pub fn request_header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_ascii_lowercase())
    })
}
