use serde_json::Value;
use parking_lot::Mutex;
use std::{collections::VecDeque, io::{Read, Write}, net::TcpListener, sync::{Arc, atomic::{AtomicBool, Ordering}}, thread, time::Duration};

pub struct Server {
    pub url: String,
    pub requests: Arc<Mutex<Vec<(String, Value)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    /// None closes the socket after receiving a complete request, modelling ambiguous transport failure.
    pub fn new(replies: Vec<Option<(u16, Value)>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = requests.clone(); let done = stop.clone();
        let worker = thread::spawn(move || {
            let mut replies: VecDeque<_> = replies.into();
            while !done.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else { thread::sleep(Duration::from_millis(5)); continue; };
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut bytes = Vec::new(); let mut buffer = [0u8; 4096];
                let parsed = loop {
                    let n = match stream.read(&mut buffer) {
                        Ok(n) => n,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break None,
                    };
                    if n == 0 || bytes.len() > 1024 * 1024 { break None; }
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]);
                        let length = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(str::trim).and_then(|v| v.parse::<usize>().ok())).unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break Some((head.lines().next().unwrap().to_owned(), serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap_or(Value::Null)));
                        }
                    }
                };
                let Some(request) = parsed else { continue; };
                seen.lock().push(request);
                if let Some(Some((status, body))) = replies.pop_front() {
                    let body = body.to_string();
                    let _ = write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
            }
        });
        Self { url, requests, stop, worker: Some(worker) }
    }
    pub fn posts(&self) -> usize { self.requests.lock().iter().filter(|(line, _)| line.starts_with("POST ")).count() }
}
impl Drop for Server {
    fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); self.worker.take().unwrap().join().unwrap(); }
}

#[test]
fn incomplete_connection_does_not_consume_the_next_response() {
    use std::net::{Shutdown, TcpStream};
    let expected = serde_json::json!({"available_count": 1});
    let server = Server::new(vec![Some((200, expected.clone()))]);
    let mut incomplete = TcpStream::connect(server.url.trim_start_matches("http://")).unwrap();
    incomplete.write_all(b"GET /incomplete HTTP/1.1\r\n").unwrap();
    incomplete.shutdown(Shutdown::Both).unwrap();
    drop(incomplete);
    let actual: Value = reqwest::blocking::Client::builder()
        .no_proxy().timeout(Duration::from_secs(2)).build().unwrap()
        .get(&server.url).send().unwrap().json().unwrap();
    assert_eq!(actual, expected);
    assert_eq!(server.requests.lock()[0].0, "GET / HTTP/1.1");
}
