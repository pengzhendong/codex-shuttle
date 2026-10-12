use super::*;
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

struct Fixture {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/package", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            for response in responses {
                let mut socket = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0; 1];
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 64 * 1024);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                socket.write_all(&response).unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn progress_uses_final_complete_headers_and_full_range_size() {
    let proxy = "HTTP/1.1 200 Connection established\r\n\r\n";
    let redirect = "HTTP/1.1 302 Found\r\nContent-Length: 42\r\n\r\n";
    let partial = "HTTP/1.1 206 Partial Content\r\nContent-Length: 93\r\nContent-Range: bytes 7-99/100\r\n\r\n";
    assert_eq!(download_content_length(proxy), None);
    assert_eq!(download_content_length(redirect), None);
    assert_eq!(
        download_content_length(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 93\r\nContent-Range: bytes 7-99/*\r\n\r\n"
        ),
        None
    );
    assert_eq!(
        download_content_length(&format!("{proxy}{redirect}{partial}")),
        Some(100)
    );
    assert_eq!(
        download_content_length("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n"),
        None
    );
}

#[test]
fn truncated_download_resumes_within_the_same_invocation() {
    let payload = vec![0xa5; 128 * 1024];
    let partial = 16 * 1024;
    let fixture = Fixture::start(vec![
        response(
            "200 OK",
            &format!("Content-Length: {}\r\n", payload.len()),
            &payload[..partial],
        ),
        response(
            "206 Partial Content",
            &format!(
                "Content-Range: bytes {partial}-{}/{}\r\nContent-Length: {}\r\n",
                payload.len() - 1,
                payload.len(),
                payload.len() - partial,
            ),
            &payload[partial..],
        ),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("archive");
    let mut events = Vec::new();
    download_file_with_progress(&fixture.url, &path, |bytes, total, complete| {
        events.push((bytes, total, complete));
    })
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), payload);
    verify_sha256(&path, &sha256_bytes(&payload)).unwrap();
    assert!(verify_sha256(&path, &"0".repeat(64)).is_err());
    assert_eq!(
        events.last(),
        Some(&(payload.len() as u64, Some(payload.len() as u64), true))
    );
    assert_eq!(events.iter().filter(|event| event.2).count(), 1);
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].to_ascii_lowercase().contains("range:"));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains(&format!("range: bytes={partial}-"))
    );
}

#[test]
fn redirect_headers_and_body_never_enter_the_archive() {
    let fixture = Fixture::start(vec![
        response(
            "302 Found",
            "Location: /final\r\nContent-Length: 8\r\n",
            b"redirect",
        ),
        response("200 OK", "Content-Length: 7\r\n", b"payload"),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("archive");
    download(&fixture.url, &path).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"payload");
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

#[test]
fn repeated_truncation_stops_after_three_attempts_without_completion() {
    let fixture = Fixture::start(vec![
        response("200 OK", "Content-Length: 100\r\n", b"abc"),
        response(
            "206 Partial Content",
            "Content-Range: bytes 3-99/100\r\nContent-Length: 97\r\n",
            b"def",
        ),
        response(
            "206 Partial Content",
            "Content-Range: bytes 6-99/100\r\nContent-Length: 94\r\n",
            b"ghi",
        ),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("archive");
    let mut completed = false;
    assert!(
        download_file_with_progress(&fixture.url, &path, |_, _, done| completed |= done).is_err()
    );
    assert!(!completed);
    assert_eq!(fs::read(path).unwrap(), b"abcdefghi");
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
}

#[test]
fn forbidden_response_fails_without_retry_or_completion() {
    let fixture = Fixture::start(vec![response(
        "403 Forbidden",
        "Content-Length: 0\r\n",
        b"",
    )]);
    let temp = tempfile::tempdir().unwrap();
    let mut completed = false;
    let error =
        download_file_with_progress(&fixture.url, &temp.path().join("archive"), |_, _, done| {
            completed |= done;
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("403"));
    assert!(!completed);
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[test]
fn transient_http_failure_is_retried() {
    let fixture = Fixture::start(vec![
        response("503 Service Unavailable", "Content-Length: 0\r\n", b""),
        response("200 OK", "Content-Length: 7\r\n", b"payload"),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("archive");
    download(&fixture.url, &path).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"payload");
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

#[test]
fn chunked_response_completes_without_content_length() {
    let fixture = Fixture::start(vec![response(
        "200 OK",
        "Transfer-Encoding: chunked\r\n",
        b"7\r\npayload\r\n0\r\n\r\n",
    )]);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("archive");
    let mut events = Vec::new();
    download_file_with_progress(&fixture.url, &path, |bytes, total, done| {
        events.push((bytes, total, done));
    })
    .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"payload");
    assert_eq!(events.last(), Some(&(7, None, true)));
}

#[test]
fn unsupported_resume_fails_without_appending_a_full_response() {
    let fixture = Fixture::start(vec![response(
        "200 OK",
        "Content-Length: 7\r\n",
        b"payload",
    )]);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("archive");
    fs::write(&path, b"pay").unwrap();
    let mut completed = false;
    assert!(
        download_file_with_progress(&fixture.url, &path, |_, _, done| completed |= done).is_err()
    );
    assert!(!completed);
    assert_eq!(fs::read(path).unwrap(), b"pay");
    assert!(
        fixture.requests.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("range: bytes=3-")
    );
}
