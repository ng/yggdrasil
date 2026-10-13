#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use ygg::knowledge::{runtime::Binding, shared::Config};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn git(root: &Path, args: &[&str]) {
    okf::success(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap(),
    );
}
fn control(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
struct Server {
    child: Child,
    root: tempfile::TempDir,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "{}",
                std::fs::read_to_string(self.root.path().join("sshd.log")).unwrap_or_default()
            );
        }
    }
}
fn distribution(samples: &[f64]) -> serde_json::Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    serde_json::json!({"samples_ms":samples,"p50_ms":sorted[(sorted.len()/2).saturating_sub(1)],
        "p95_ms":sorted[(sorted.len()*95).div_ceil(100)-1],"max_ms":sorted.last()})
}

#[test]
#[ignore = "manual release benchmark: YGG_TEST_YGG_BIN and YGG_SHARED_PERF_REPORT; loopback sshd"]
fn authenticated_shared_publication_and_fetch_latency() {
    let binary =
        PathBuf::from(std::env::var_os("YGG_TEST_YGG_BIN").expect("release binary required"))
            .canonicalize()
            .unwrap();
    let report = PathBuf::from(
        std::env::var_os("YGG_SHARED_PERF_REPORT").expect("report destination required"),
    );
    assert!(
        !report.exists(),
        "retain prior measurements; choose a fresh report path"
    );
    let root = tempfile::Builder::new()
        .prefix(".ygg-shared-latency-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    let directory = root.path().canonicalize().unwrap();
    assert!(
        !directory.to_string_lossy().chars().any(char::is_whitespace),
        "sshd fixture requires a whitespace-free home path"
    );
    for name in ["host", "client"] {
        okf::success(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(directory.join(name))
                .output()
                .unwrap(),
        );
    }
    let remote = directory.join("remote.git");
    git(&directory, &["init", "--bare", remote.to_str().unwrap()]);
    let seed = directory.join("seed");
    git(
        &directory,
        &["init", "-b", "knowledge", seed.to_str().unwrap()],
    );
    git(&seed, &["commit", "--allow-empty", "-m", "initial"]);
    git(
        &seed,
        &[
            "push",
            remote.to_str().unwrap(),
            "HEAD:refs/heads/knowledge",
        ],
    );
    // The authenticated key can execute only these two commands against this repository.
    let mut wrapper = String::from("#!/bin/sh\nset -eu\ncase \"$SSH_ORIGINAL_COMMAND\" in\n");
    for verb in ["upload-pack", "receive-pack"] {
        wrapper += &format!(
            "{}) exec /usr/bin/git {} {} ;;\n",
            quote(&format!("git-{verb} '{}'", remote.display())),
            verb,
            quote(remote.to_str().unwrap())
        );
    }
    wrapper += "*) exit 64 ;;\nesac\n";
    std::fs::write(directory.join("git-server.sh"), wrapper).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let account = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let config = directory.join("sshd_config");
    std::fs::write(&config, format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {0}/host\nPidFile {0}/sshd.pid\nAuthorizedKeysFile {0}/client.pub\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes yes\nPermitUserRC no\nForceCommand /bin/sh {0}/git-server.sh\n",directory.display())).unwrap();
    let key = std::fs::read_to_string(directory.join("host.pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(
        directory.join("known_hosts"),
        format!("[127.0.0.1]:{port} {key}\n"),
    )
    .unwrap();
    let child = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(config)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(directory.join("sshd.log")).unwrap())
        .spawn()
        .unwrap();
    let mut server = Server { child, root };
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(server.child.try_wait().unwrap().is_none() && Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let ssh = format!(
        "/usr/bin/ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o GlobalKnownHostsFile=/dev/null -o UserKnownHostsFile={} -i {} -p {port}",
        quote(directory.join("known_hosts").to_str().unwrap()),
        quote(directory.join("client").to_str().unwrap())
    );
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (binding_a, _, _) = okf::fixture(a.path());
    okf::fixture(b.path());
    let mut binding_b: Binding =
        serde_json::from_value(serde_json::to_value(&binding_a).unwrap()).unwrap();
    binding_b.bundle = b.path().join("bundle").canonicalize().unwrap();
    control(
        &b.path().join("policy/identity.json"),
        &std::fs::read(a.path().join("policy/identity.json")).unwrap(),
    );
    let shared = Config {
        version: 1,
        remote: format!("ssh://{account}@127.0.0.1:{port}{}", remote.display()),
        branch: "knowledge".into(),
    };
    for (profile, binding) in [(a.path(), &binding_a), (b.path(), &binding_b)] {
        okf::select(profile, binding);
        control(
            &profile.join("policy/shared.json"),
            &serde_json::to_vec(&shared).unwrap(),
        );
    }
    let app = |profile: &Path| {
        let mut command = Command::new(&binary);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", profile)
            .env("YGG_CONFIG_DIR", profile.join("config"))
            .env("YGG_DATA_DIR", profile.join("data"))
            .env("YGG_KNOWLEDGE_DIR", profile.join("bundle"))
            .env("YGG_KNOWLEDGE_POLICY_DIR", profile.join("policy"))
            .env("YGG_USER", "legacy-user")
            .env("YGG_AGENT_NAME", "fixture-agent")
            .env("YGG_DB_MODE", "external")
            .env("GIT_SSH_COMMAND", &ssh)
            .current_dir(profile.join("repo"));
        command
    };
    let mut writes = vec![];
    let mut reads = vec![];
    for sample in 0..22 {
        let started = Instant::now();
        let note = okf::json(
            app(a.path())
                .args([
                    "remember",
                    &format!("SSH sample {sample}"),
                    "--global",
                    "--json",
                ])
                .output()
                .unwrap(),
        );
        let write_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        let listed = okf::json(
            app(b.path())
                .args(["remember", "--list", "--all", "--json"])
                .output()
                .unwrap(),
        );
        let read_ms = started.elapsed().as_secs_f64() * 1000.;
        assert!(
            listed["results"].as_array().unwrap().contains(&note),
            "reader must see the exact acknowledged remote note"
        );
        if sample >= 2 {
            writes.push(write_ms);
            reads.push(read_ms);
        }
    }
    let result = serde_json::json!({"schema":1,"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "binary_sha256":ygg::knowledge::document::digest(&std::fs::read(&binary).unwrap()),
        "transport":"authenticated pinned-host-key SSH over 127.0.0.1; dedicated bare Git branch",
        "warmup_pairs":2,"measured_pairs":20,"concurrent_clients":1,"profiles":2,"acknowledged_notes":22,
        "publication":distribution(&writes),"fetch_and_list":distribution(&reads),
        "correctness":"Each independent reader invocation observed the exact note acknowledged by the writer; database unavailable.",
        "limits":"Small growing corpus, sequential clients, local loopback only. Includes process startup and Git/SSH authentication. No WAN/provider latency claim and no 50ms local-hook threshold applied."});
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(report)
        .unwrap();
    writeln!(output, "{}", serde_json::to_string_pretty(&result).unwrap()).unwrap();
    output.sync_all().unwrap();
}
