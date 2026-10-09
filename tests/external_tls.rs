use sqlx::postgres::PgSslMode;
use ygg::db::external::options;

#[test]
fn remote_policy_fails_closed_and_redacts_invalid_parameters() {
    assert!(matches!(
        options("postgres://user:secret@db.example.test/database")
            .unwrap()
            .get_ssl_mode(),
        PgSslMode::VerifyFull
    ));
    assert!(matches!(
        options("postgres://user@127.0.0.1/db?host=db.example.test")
            .unwrap()
            .get_ssl_mode(),
        PgSslMode::VerifyFull
    ));
    for mode in ["disable", "allow", "prefer", "require", "verify-ca"] {
        assert!(
            options(&format!(
                "postgres://user@db.example.test/db?sslmode={mode}"
            ))
            .is_err()
        );
    }
    for url in [
        "postgres://user:secret@db.example.test/db?sslmode=secret",
        "postgres://user:secret@db.example.test/db?typo=secret",
        "https://user:secret@db.example.test/db",
    ] {
        let error = options(url).unwrap_err().to_string();
        assert!(!error.contains("secret"));
        assert!(!error.contains("user"));
    }
    for url in [
        "postgres://user@127.0.0.1/db?sslmode=disable",
        "postgres://user@localhost/db?sslmode=disable",
        "postgres://user@[::1]/db?sslmode=disable",
        "postgres://user@localhost/db?host=/tmp/fixture&sslmode=disable",
    ] {
        assert!(matches!(
            options(url).unwrap().get_ssl_mode(),
            PgSslMode::Disable
        ));
    }
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires YGG_TEST_PG_BIN and openssl; creates isolated native TLS server"]
async fn native_tls_checks_ca_hostname_and_refuses_plaintext() {
    use sqlx::{Connection, PgConnection};
    use std::{
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::Duration,
    };
    fn run(bin: &Path, args: &[&str], cwd: &Path) {
        let output = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            bin.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    struct Server {
        bin: PathBuf,
        data: PathBuf,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = Command::new(self.bin.join("pg_ctl"))
                .arg("-D")
                .arg(&self.data)
                .args(["stop", "-m", "fast", "-w", "-t", "5"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let bin = PathBuf::from(std::env::var("YGG_TEST_PG_BIN").unwrap());
    let temp = tempfile::Builder::new()
        .prefix("ytls-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let data = base.join("data");
    for name in ["ca", "wrong"] {
        run(
            Path::new("openssl"),
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                &format!("/CN={name}"),
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.pem"),
            ],
            &base,
        );
    }
    run(
        Path::new("openssl"),
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
        ],
        &base,
    );
    std::fs::write(base.join("server.ext"), "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n").unwrap();
    run(
        Path::new("openssl"),
        &[
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            "server.ext",
            "-out",
            "server.pem",
        ],
        &base,
    );
    std::fs::set_permissions(
        base.join("server.key"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    run(
        &bin.join("initdb"),
        &[
            "-D",
            data.to_str().unwrap(),
            "--username=tls_test",
            "--auth=trust",
            "--encoding=UTF8",
            "--locale=C",
        ],
        &base,
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    use std::io::Write;
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(data.join("postgresql.conf"))
        .unwrap();
    writeln!(config, "listen_addresses='127.0.0.1'\nport={port}\nunix_socket_directories=''\nssl=on\nssl_cert_file='{}'\nssl_key_file='{}'", base.join("server.pem").display(), base.join("server.key").display()).unwrap();
    config.sync_all().unwrap();
    drop(listener);
    let _server = Server {
        bin: bin.clone(),
        data: data.clone(),
    };
    run(
        &bin.join("pg_ctl"),
        &[
            "-D",
            data.to_str().unwrap(),
            "-l",
            base.join("server.log").to_str().unwrap(),
            "-w",
            "-t",
            "15",
            "start",
        ],
        &base,
    );
    let url = |host: &str, ca: &str| {
        format!(
            "postgres://tls_test@{host}:{port}/postgres?sslmode=verify-full&sslrootcert={}",
            base.join(ca).display()
        )
    };
    let mut connection = PgConnection::connect_with(&options(&url("localhost", "ca.pem")).unwrap())
        .await
        .unwrap();
    let encrypted: bool =
        sqlx::query_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert!(encrypted);
    connection.close().await.unwrap();
    for (bad, reason) in [
        (url("localhost", "wrong.pem"), "UnknownIssuer"),
        (url("127.0.0.1", "ca.pem"), "NotValidForName"),
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            PgConnection::connect_with(&options(&bad).unwrap()),
        )
        .await
        .unwrap();
        let error = result.expect_err("wrong CA or hostname must fail");
        assert!(
            format!("{error:?}").contains(reason),
            "wrong TLS rejection: {error:?}"
        );
    }
    // Disable TLS on this fixture and prove verify-full cannot downgrade.
    writeln!(config, "ssl=off").unwrap();
    config.sync_all().unwrap();
    run(
        &bin.join("pg_ctl"),
        &[
            "-D",
            data.to_str().unwrap(),
            "-m",
            "fast",
            "-w",
            "-t",
            "15",
            "restart",
            "-l",
            base.join("server.log").to_str().unwrap(),
        ],
        &base,
    );
    let error = PgConnection::connect_with(&options(&url("localhost", "ca.pem")).unwrap())
        .await
        .expect_err("TLS must not downgrade");
    assert!(matches!(&error, sqlx::Error::Tls(_)), "{error:?}");
}

#[test]
fn migration_owner_cannot_redirect_database_or_endpoint() {
    use ygg::db::external::validate_owner_target;
    let runtime = "postgres://runtime@localhost/application";
    validate_owner_target(runtime, "postgres://owner:private@localhost/application").unwrap();
    for owner in [
        "postgres://owner@localhost/other",
        "postgres://owner@localhost:5544/application",
        "postgres://owner@127.0.0.1/application",
        "postgres://owner@localhost/application?host=/tmp/other",
        "postgres://owner@localhost",
    ] {
        assert!(validate_owner_target(runtime, owner).is_err());
    }
    assert!(
        validate_owner_target("postgres://runtime@localhost", "postgres://owner@localhost")
            .is_err(),
        "default DB follows username"
    );
}
