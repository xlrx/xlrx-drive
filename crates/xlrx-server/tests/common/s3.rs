//! An S3 server on a temporary directory (s3s-fs), checking signatures itself.

use std::sync::{Arc, Mutex};

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use xlrx_server::s3::S3Config;

pub const BUCKET: &str = "xlrx-cache";

pub struct FakeS3 {
    pub cfg: S3Config,
    pub dir: tempfile::TempDir,
    /// Every request: method and path (what was uploaded, and when).
    pub log: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeS3 {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(BUCKET)).unwrap();
        let fs = s3s_fs::FileSystem::new(dir.path()).unwrap();
        let mut b = S3ServiceBuilder::new(fs);
        b.set_auth(SimpleAuth::from_single("testkey", "testsecret"));
        let service = b.build();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let (service, l) = (service.clone(), l.clone());
                let logged = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        l.lock()
                            .unwrap()
                            .push(format!("{} {}", req.method(), req.uri().path()));
                        let service = service.clone();
                        async move { hyper::service::Service::call(&service, req).await }
                    },
                );
                tokio::spawn(async move {
                    let _ = Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), logged)
                        .await;
                });
            }
        });
        Self {
            cfg: S3Config {
                endpoint: format!("http://{addr}").parse().unwrap(),
                bucket: BUCKET.into(),
                region: "us-east-1".into(),
                access_key: "testkey".into(),
                secret_key: "testsecret".into(),
                virtual_host: false,
            },
            dir,
            log,
            task,
        }
    }

    /// Uploads so far (single requests, parts and their start and end).
    pub fn uploads(&self) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("PUT ") || l.starts_with("POST "))
            .count()
    }

    /// Keys stored in the bucket (from the directory).
    pub fn keys(&self) -> Vec<String> {
        let base = self.dir.path().join(BUCKET);
        let mut out = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p
                        .strip_prefix(&base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    // s3s-fs keeps metadata and unfinished uploads in hidden files.
                    if !rel.split('/').any(|c| c.starts_with('.')) {
                        out.push(rel);
                    }
                }
            }
        }
        out.sort();
        out
    }
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.task.abort();
    }
}
