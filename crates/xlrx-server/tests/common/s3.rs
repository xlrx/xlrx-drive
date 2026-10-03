//! An S3 server on a temporary directory (s3s-fs), checking signatures itself.

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use xlrx_server::s3::S3Config;

pub const BUCKET: &str = "xlrx-cache";

pub struct FakeS3 {
    pub cfg: S3Config,
    pub dir: tempfile::TempDir,
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
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let service = service.clone();
                tokio::spawn(async move {
                    let _ = Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), service)
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
            task,
        }
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
