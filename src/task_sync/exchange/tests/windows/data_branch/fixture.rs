use super::*;
use crate::task_sync::data_worktree::{DataWorktree, StagedCommitRequest};
use std::process::{Child, Command, Output};

pub(super) const BRANCH: &str = "refs/heads/engram-data";

// The newline is a completion marker, not whitespace to trim: a URL prefix
// (even one ending in a valid port) is not a published endpoint record.
fn parse_endpoint(record: &str) -> Option<(String, u16)> {
    let url = record.strip_suffix('\n')?;
    let digits = url
        .strip_prefix("https://127.0.0.1:")?
        .strip_suffix("/data.git")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port = digits.parse::<u16>().ok().filter(|port| *port != 0)?;
    Some((url.to_owned(), port))
}

fn await_endpoint(path: &Path, mut pending: impl FnMut()) -> (String, u16) {
    loop {
        if let Ok(record) = fs::read_to_string(path)
            && let Some(endpoint) = parse_endpoint(&record)
        {
            return endpoint;
        }
        pending();
    }
}

pub(super) fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let _phase = super::timing::phase("fixture-git");
    let mut command = crate::task_sync::exchange::command::local(root, &[]);
    command
        .args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "user.name=Data workflow",
            "-c",
            "user.email=data@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .env("GIT_ALLOW_PROTOCOL", "file");
    let output = command
        .output()
        .expect("required prerequisite: Git executable");
    assert!(
        output.status.success(),
        "fixture Git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

pub(super) fn line(root: &Path, args: &[&str]) -> String {
    String::from_utf8(git(root, args))
        .unwrap()
        .trim_end()
        .to_owned()
}

pub(super) struct Repositories {
    pub left: PathBuf,
    pub right: PathBuf,
    pub url: String,
    server: Child,
}

fn openssl(root: &Path, args: &[&str]) -> Output {
    let _phase = super::timing::phase("fixture-tls-certificate");
    let exec = PathBuf::from(line(root, &["--exec-path"]));
    let program = exec
        .ancestors()
        .map(|ancestor| ancestor.join("usr/bin/openssl.exe"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("openssl"));
    Command::new(&program).current_dir(root).args(args).output()
        .unwrap_or_else(|error| panic!("required existing TLS fixture prerequisite {program:?}: {error}; no installation or skip"))
}

impl Repositories {
    pub fn new(root: &Path) -> Self {
        let _phase = super::timing::phase("repository-setup");
        // The enclosing process owns HOME/configuration and the Windows job.
        let backend = PathBuf::from(line(root, &["--exec-path"])).join("git-http-backend.exe");
        assert!(
            backend.is_file(),
            "required existing TLS fixture prerequisite {backend:?}; no installation or skip"
        );
        let certificate = openssl(
            root,
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "2",
                "-subj",
                "/CN=Engram loopback fixture",
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-keyout",
                "key.pem",
                "-out",
                "cert.pem",
            ],
        );
        assert!(
            certificate.status.success(),
            "TLS fixture certificate: {}",
            String::from_utf8_lossy(&certificate.stderr)
        );
        git(root, &["init", "--bare", "data.git"]);
        git(
            &root.join("data.git"),
            &["config", "http.receivepack", "true"],
        );
        let left = root.join("left");
        fs::create_dir(&left).unwrap();
        git(&left, &["init", "--initial-branch=code"]);
        fs::write(left.join(".engram-project"), "https-data-fixture\n").unwrap();
        fs::write(left.join(".gitignore"), "/target/\n").unwrap();
        fs::write(left.join("code.rs"), "fn main() {}\n").unwrap();
        git(&left, &["add", ".engram-project", ".gitignore", "code.rs"]);
        git(&left, &["commit", "-m", "Seed ordinary code"]);
        let right = root.join("right");
        git(
            root,
            &[
                "clone",
                "--no-local",
                left.to_str().unwrap(),
                right.to_str().unwrap(),
            ],
        );
        let fixture = Self::serve(root, left, right, None);
        for code in [&fixture.left, &fixture.right] {
            crate::task_sync::data_worktree::initialize(
                &code.join(".engram-project"),
                &crate::ProjectId("https-data-fixture".into()),
                "engram-data",
            )
            .unwrap();
            git(
                code,
                &["config", "--local", "remote.origin.url", &fixture.url],
            );
            git(code, &["config", "--local", "credential.helper", ""]);
            git(code, &["config", "--local", "http.sslBackend", "openssl"]);
            git(
                code,
                &[
                    "config",
                    "--local",
                    "http.sslCAInfo",
                    root.join("cert.pem").to_str().unwrap(),
                ],
            );
        }
        fixture
    }

    /// The repositories an earlier child of the same fixture set up, served
    /// again at the same endpoint: the binding's target fingerprint names it.
    pub fn reopen(root: &Path) -> Self {
        let _phase = super::timing::phase("repository-reopen");
        let (previous, port) = parse_endpoint(&fs::read_to_string(root.join("endpoint")).unwrap())
            .expect("the earlier endpoint is a complete HTTPS loopback record");
        fs::remove_file(root.join("endpoint")).unwrap();
        let fixture = Self::serve(root, root.join("left"), root.join("right"), Some(port));
        assert_eq!(fixture.url, previous);
        fixture
    }

    fn serve(root: &Path, left: PathBuf, right: PathBuf, port: Option<u16>) -> Self {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/task_sync/exchange/tests/windows/data_branch/server.mjs");
        let mut command = Command::new("node");
        command.arg(script).arg(root);
        if let Some(port) = port {
            command.arg(port.to_string());
        }
        let server = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("server-stderr")).unwrap())
            .spawn()
            .expect("required existing TLS fixture prerequisite: Node.js; no installation or skip");
        let mut fixture = Self {
            left,
            right,
            url: String::new(),
            server,
        };
        let started = Instant::now();
        let (url, _) = await_endpoint(&root.join("endpoint"), || {
            assert!(
                fixture.server.try_wait().unwrap().is_none(),
                "TLS server ended: {}",
                fs::read_to_string(root.join("server-stderr")).unwrap()
            );
            assert!(
                started.elapsed() < HANG_GUARD,
                "loopback TLS server startup hung"
            );
            std::thread::sleep(Duration::from_millis(10));
        });
        fixture.url = url;
        fixture
    }

    pub fn data(code: &Path) -> DataWorktree {
        DataWorktree::observe(code, &code.join(".engram-data"), "engram-data").unwrap()
    }

    pub fn code_snapshot(code: &Path) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            git(code, &["rev-parse", "HEAD"]),
            fs::read(code.join(".git/index")).unwrap(),
            git(code, &["status", "--porcelain", "--untracked-files=all"]),
            fs::read(code.join("code.rs")).unwrap(),
        )
    }

    pub fn commit(
        code: &Path,
        operation: uuid::Uuid,
        parents: Vec<String>,
        message: &str,
    ) -> crate::task_sync::ledger::RetainedCommit {
        let data = Self::data(code);
        git(&data.checkout, &["add", "--", ".engram"]);
        data.create_retained(&StagedCommitRequest {
            operation,
            tree: line(&data.checkout, &["write-tree"]),
            expected_head: data.head.clone(),
            parents,
            author_name: "Data workflow".into(),
            author_email: "data@example.invalid".into(),
            authored_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            message: format!("{message}\n"),
        })
        .unwrap()
    }
}

impl Drop for Repositories {
    fn drop(&mut self) {
        // Only this fixture's child; the enclosing Windows job owns descendants.
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

#[test]
fn endpoint_readiness_waits_for_the_complete_record() {
    let temporary = std::env::current_dir().unwrap().join("target/tmp");
    fs::create_dir_all(&temporary).unwrap();
    let root = tempfile::Builder::new()
        .prefix("endpoint-")
        .tempdir_in(temporary)
        .unwrap();
    let endpoint = root.path().join("endpoint");
    let records = [
        "",
        "https://127.0.0.1:12",
        "https://127.0.0.1:12345/data.g",
        "https://127.0.0.1:12345/data.git",
        "https://127.0.0.1:12345/data.git\n",
    ];
    let mut writes = 0;
    let result = await_endpoint(&endpoint, || {
        fs::write(&endpoint, records[writes]).unwrap();
        writes += 1;
    });
    assert_eq!(writes, records.len());
    assert_eq!(result, ("https://127.0.0.1:12345/data.git".into(), 12345));
}

#[test]
fn endpoint_record_requires_the_fixtures_https_address() {
    for invalid in [
        "http://127.0.0.1:12345/data.git\n",
        "https://localhost:12345/data.git\n",
        "https://user@127.0.0.1:12345/data.git\n",
        "https://127.0.0.1:0/data.git\n",
        "https://127.0.0.1:65536/data.git\n",
        "https://127.0.0.1:+12345/data.git\n",
        "https://127.0.0.1:12345/data.git?query\n",
        "https://127.0.0.1:12345/data.git#fragment\n",
        "https://127.0.0.1:12345/data.git\n\n",
    ] {
        assert_eq!(parse_endpoint(invalid), None, "accepted {invalid:?}");
    }
    assert_eq!(
        parse_endpoint("https://127.0.0.1:65535/data.git\n"),
        Some(("https://127.0.0.1:65535/data.git".into(), 65535))
    );
}
