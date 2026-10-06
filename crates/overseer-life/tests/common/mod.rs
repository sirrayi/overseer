//! Test scaffolding: a loopback mock HTTP server, a fake command runner,
//! and a temp HOME. Nothing here can touch the real network/home/keychain.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use overseer_life::connector::{CmdOutput, CommandRunner, Ctx};

pub struct Route {
    /// path prefix to match
    pub path: String,
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

/// A single-threaded loopback HTTP/1.0 mock. Records every request line
/// so tests can assert no call ever hit a token endpoint.
pub struct MockServer {
    pub base: String,
    requests: Arc<Mutex<Vec<String>>>,
    /// Full request (head + body) per connection, for POST-body asserts.
    raw: Arc<Mutex<Vec<String>>>,
    routes: Arc<Mutex<Vec<Route>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<Vec<Route>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let requests = Arc::clone(&requests);
            let raw = Arc::clone(&raw);
            let routes = Arc::clone(&routes);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                    let accept = listener.accept();
                    let Ok((mut stream, _)) = accept else { break };
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                head.push(byte[0]);
                                if head.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let head_text = String::from_utf8_lossy(&head).to_string();
                    let request_line = head_text.lines().next().unwrap_or("").to_string();
                    requests.lock().unwrap().push(request_line.clone());
                    // Read the declared body too so POST payloads are
                    // assertable (e.g. form fields on a revoke call).
                    let content_len = head_text
                        .lines()
                        .skip(1)
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        .unwrap_or(0)
                        .min(1024 * 1024);
                    let mut req_body = vec![0u8; content_len];
                    let _ = stream.read_exact(&mut req_body);
                    raw.lock()
                        .unwrap()
                        .push(format!("{head_text}{}", String::from_utf8_lossy(&req_body)));
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let route = {
                        let routes = routes.lock().unwrap();
                        routes
                            .iter()
                            .find(|r| path.starts_with(&r.path))
                            .map(|r| (r.status, r.body.clone(), r.headers.clone()))
                    };
                    let (status, body, extra) = route.unwrap_or((404, "{}".into(), Vec::new()));
                    let mut resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in &extra {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");
                    resp.push_str(&body);
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            })
        };
        MockServer {
            base: format!("http://127.0.0.1:{port}"),
            requests,
            raw,
            routes,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn route(&self, path_prefix: &str, status: u16, body: &str) {
        self.routes.lock().unwrap().push(Route {
            path: path_prefix.to_string(),
            status,
            body: body.to_string(),
            headers: Vec::new(),
        });
    }

    pub fn route_with_headers(
        &self,
        path_prefix: &str,
        status: u16,
        body: &str,
        headers: &[(&str, &str)],
    ) {
        self.routes.lock().unwrap().push(Route {
            path: path_prefix.to_string(),
            status,
            body: body.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
    }

    /// Recorded request lines ("GET /path HTTP/1.1").
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Recorded raw requests (head + declared body).
    pub fn raw(&self) -> Vec<String> {
        self.raw.lock().unwrap().clone()
    }

    /// True if any request hit a path containing `needle`.
    pub fn hit(&self, needle: &str) -> bool {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains(needle))
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Nudge the accept loop out of a blocking accept.
        let _ = std::net::TcpStream::connect(self.base.trim_start_matches("http://"));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Fake command runner: records argv, returns canned stdout per program
/// name. `calls` is shared so tests can inspect after the runner is
/// moved into `Ctx`. Secrets are never placed on argv by the code under
/// test.
type CallLog = Arc<Mutex<Vec<(String, Vec<String>)>>>;

#[derive(Clone)]
pub struct FakeRunner {
    calls: CallLog,
    responses: Arc<BTreeMap<String, CmdOutput2>>,
}

#[derive(Clone)]
pub struct CmdOutput2 {
    pub status: i32,
    pub stdout: Vec<u8>,
}

impl FakeRunner {
    pub fn new() -> Self {
        FakeRunner {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(BTreeMap::new()),
        }
    }

    /// Canned stdout for commands whose argv contains `needle`.
    pub fn respond(mut self, needle: &str, status: i32, stdout: &str) -> Self {
        Arc::get_mut(&mut self.responses)
            .expect("responses not shared yet")
            .insert(
                needle.to_string(),
                CmdOutput2 {
                    status,
                    stdout: stdout.as_bytes().to_vec(),
                },
            );
        self
    }

    /// Handle to the recorded calls (survives moving the runner into Ctx).
    pub fn calls_handle(&self) -> CallLog {
        Arc::clone(&self.calls)
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str], _timeout_ms: u64) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        ));
        let cmdline = format!("{program} {}", args.join(" "));
        for (needle, resp) in self.responses.iter() {
            if cmdline.contains(needle.as_str()) {
                return Ok(CmdOutput {
                    status: resp.status,
                    stdout: resp.stdout.clone(),
                    stderr: Vec::new(),
                });
            }
        }
        // Default: command "fails" like a missing keychain item.
        Ok(CmdOutput {
            status: 44,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

/// A temp HOME that cleans itself up.
pub struct TestHome(pub PathBuf);

impl TestHome {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ovs-life-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestHome(dir)
    }

    /// Write `home_rel` (relative to HOME) creating parents.
    pub fn write(&self, home_rel: &str, contents: &str) -> PathBuf {
        let path = self.0.join(home_rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Check whether a sibling `-wal`/`-shm` file was created next to `rel`.
    pub fn wal_or_shm_created(&self, rel: &str) -> bool {
        let p = self.0.join(rel);
        Path::new(&format!("{}-wal", p.display())).exists()
            || Path::new(&format!("{}-shm", p.display())).exists()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A ctx rooted at `home` with a fake runner; URL overrides go in env.
pub fn test_ctx(home: &TestHome, env: &[(&str, &str)], runner: FakeRunner) -> Ctx {
    let mut ctx = Ctx::for_test(home.0.clone(), Box::new(runner));
    for (k, v) in env {
        ctx.env.insert(k.to_string(), v.to_string());
    }
    ctx
}
