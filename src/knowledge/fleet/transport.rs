//! Authenticated byte transport only. Successful exchange does not validate a
//! participant receipt or authorize a migration transition.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::DirBuilder,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use uuid::Uuid;

const MAX_REQUEST: usize = super::protocol::MAX_REQUEST;
const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub account: String,
    /// Operator-enrolled public key, not a key discovered from the connection.
    pub host_key: String,
}
impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.is_empty()
                && self.host.len() <= 253
                && !self.host.starts_with('-')
                && self
                    .host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:_".contains(&b))
                && self.port != 0,
            "invalid SSH endpoint"
        );
        ensure!(
            !self.account.is_empty()
                && self.account.len() <= 128
                && self
                    .account
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid SSH account"
        );
        let fields: Vec<_> = self.host_key.split(' ').collect();
        ensure!(
            fields.len() == 2
                && matches!(
                    fields[0],
                    "ssh-ed25519"
                        | "ssh-rsa"
                        | "ecdsa-sha2-nistp256"
                        | "ecdsa-sha2-nistp384"
                        | "ecdsa-sha2-nistp521"
                )
                && !fields[1].is_empty()
                && fields[1].len() <= 4096
                && fields[1]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)),
            "explicit OpenSSH public host key required"
        );
        Ok(())
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        // Fixed system temporary root avoids SSH option expansion of arbitrary
        // TMPDIR contents. create() is exclusive, with private permissions.
        let path = Path::new("/tmp").join(format!("ygg-fleet-ssh-{}", Uuid::new_v4()));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("known_hosts"));
        let _ = std::fs::remove_dir(&self.0);
    }
}
async fn bounded(mut reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "SSH response exceeds limit");
    Ok(bytes)
}
/// Execute the fixed participant protocol command. The caller must validate the
/// response envelope (operation, participant, plan digest, action and nonce).
/// Any error after dispatch has an uncertain remote mutation outcome.
/// `identity` selects a local operator key; its contents never enter the plan.
pub async fn exchange(
    endpoint: &Endpoint,
    participant: Uuid,
    request: &[u8],
    identity: Option<&Path>,
) -> Result<Vec<u8>> {
    exchange_with_timeout(
        endpoint,
        participant,
        request,
        identity,
        Duration::from_secs(60),
    )
    .await
}
async fn exchange_with_timeout(
    endpoint: &Endpoint,
    participant: Uuid,
    request: &[u8],
    identity: Option<&Path>,
    deadline: Duration,
) -> Result<Vec<u8>> {
    endpoint.validate()?;
    ensure!(
        !participant.is_nil() && request.len() <= MAX_REQUEST,
        "invalid participant or oversized request"
    );
    let scratch = Scratch::new()?;
    let alias = format!("ygg-fleet-{participant}");
    let known_hosts = scratch.0.join("known_hosts");
    std::fs::write(&known_hosts, format!("{alias} {}\n", endpoint.host_key))?;
    let mut command = Command::new("ssh");
    command.args(["-F", "/dev/null", "-T", "-a", "-x"]);
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "GlobalKnownHostsFile=/dev/null",
        "VerifyHostKeyDNS=no",
        "UpdateHostKeys=no",
        "ControlMaster=no",
        "ControlPath=none",
        "ClearAllForwardings=yes",
        "ConnectTimeout=10",
        "ConnectionAttempts=1",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "PermitLocalCommand=no",
    ] {
        command.args(["-o", option]);
    }
    command
        .arg("-o")
        .arg(format!("HostKeyAlias={alias}"))
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    if let Some(identity) = identity {
        ensure!(
            identity.is_absolute(),
            "absolute operator SSH identity path required"
        );
        command.arg("-i").arg(identity).args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
        ]);
    }
    command
        .arg("-p")
        .arg(endpoint.port.to_string())
        .arg("-l")
        .arg(&endpoint.account)
        .arg(&endpoint.host)
        .arg("ygg knowledge fleet-participant")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("cannot start participant SSH transport")?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = tokio::time::timeout(deadline, async {
        let send = async {
            stdin.write_all(request).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<_, anyhow::Error>(())
        };
        let (_, response, _diagnostics, status) = tokio::try_join!(
            send,
            bounded(stdout, MAX_RESPONSE),
            bounded(stderr, 64 * 1024),
            async { Ok::<_, anyhow::Error>(child.wait().await?) }
        )?;
        ensure!(
            status.success(),
            "SSH participant failed; remote operation outcome may be uncertain"
        );
        Ok::<_, anyhow::Error>(response)
    })
    .await;
    match result {
        Ok(Ok(response)) => Ok(response),
        failure => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match failure {
                Ok(Err(error)) => Err(error.context(
                    "participant exchange failed; reconcile retained operation before retry",
                )),
                Err(_) => anyhow::bail!(
                    "participant exchange timed out; remote operation outcome uncertain"
                ),
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command as SyncCommand};
    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[tokio::test]
    #[ignore = "requires local sshd/ssh-keygen; starts disposable loopback SSH server"]
    async fn authenticated_exchange_rejects_wrong_keys_and_bounds_output() {
        // OpenSSH StrictModes rejects authorized keys beneath world-writable
        // /tmp, even when the leaf directory is private. Keep strict checking.
        let temp = tempfile::Builder::new()
            .prefix(".ygg-fleet-transport-")
            .tempdir_in(std::env::var_os("HOME").expect("SSH test requires user home"))
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        for name in ["host", "client"] {
            assert!(
                SyncCommand::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(root.join(name))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let account = String::from_utf8(SyncCommand::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let config = root.join("sshd_config");
        std::fs::write(&config, format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {0}/host\nPidFile {0}/sshd.pid\nAuthorizedKeysFile {0}/client.pub\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes yes\nForceCommand /bin/cat\n", root.display())).unwrap();
        let _server = Server(
            SyncCommand::new("/usr/sbin/sshd")
                .args(["-D", "-e", "-f"])
                .arg(&config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < until, "sshd did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let key = |name: &str| {
            std::fs::read_to_string(root.join(name))
                .unwrap()
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut endpoint = Endpoint {
            host: "127.0.0.1".into(),
            port,
            account,
            host_key: key("host.pub"),
        };
        let participant = Uuid::new_v4();
        let identity = root.join("client");
        assert_eq!(
            exchange(
                &endpoint,
                participant,
                b"request with 'quotes'\n",
                Some(&identity)
            )
            .await
            .unwrap(),
            b"request with 'quotes'\n"
        );
        endpoint.host_key = key("client.pub");
        assert!(
            exchange(&endpoint, participant, b"request", Some(&identity))
                .await
                .is_err()
        );
        endpoint.host_key = key("host.pub");
        assert!(
            exchange(&endpoint, participant, b"request", Some(&root.join("host")))
                .await
                .is_err()
        );
        assert!(
            exchange(
                &endpoint,
                participant,
                &vec![b'x'; MAX_RESPONSE + 1],
                Some(&identity)
            )
            .await
            .is_err()
        );
        assert!(
            exchange_with_timeout(
                &endpoint,
                participant,
                b"request",
                Some(&identity),
                Duration::ZERO
            )
            .await
            .is_err()
        );
    }
}
