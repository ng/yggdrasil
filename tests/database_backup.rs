#![cfg(any(target_os = "macos", target_os = "linux"))]
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use ygg::db::backup::NativeConnection;

#[test]
fn native_settings_preserve_effective_identity_tls_and_redact_debug() {
    let options = PgConnectOptions::new()
        .host("::1")
        .port(5439)
        .username("owner % @ + λ")
        .password("secret %+&=/'λ")
        .database("db % = + λ")
        .ssl_mode(PgSslMode::VerifyFull)
        .ssl_root_cert("/tmp/ca with spaces.pem")
        .ssl_client_cert("/tmp/client.pem")
        .ssl_client_key("/tmp/key.pem");
    let native = NativeConnection::from_options(&options).unwrap();
    assert_eq!(format!("{native:?}"), "NativeConnection([redacted])");
    let mut command = tokio::process::Command::new("/explicit/pg_dump");
    native.apply(&mut command);
    assert_eq!(command.as_std().get_args().count(), 0);
    let env: std::collections::BTreeMap<_, _> = command
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    assert_eq!(env["PGHOST"], "::1");
    assert_eq!(env["PGPORT"], "5439");
    assert_eq!(env["PGUSER"], "owner % @ + λ");
    assert_eq!(env["PGDATABASE"], "db % = + λ");
    assert_eq!(env["PGPASSWORD"], "secret %+&=/'λ");
    assert_eq!(env["PGSSLMODE"], "verify-full");
    assert_eq!(env["PGSSLROOTCERT"], "/tmp/ca with spaces.pem");
    assert_eq!(env["PGSSLCERT"], "/tmp/client.pem");
    assert_eq!(env["PGSSLKEY"], "/tmp/key.pem");
    assert_eq!(env["PGPASSFILE"], "/dev/null/ygg-no-pgpass");
    assert_eq!(env["PGGSSENCMODE"], "disable");
    assert!(!env.contains_key("PGSERVICE"));
}

#[test]
fn native_settings_preserve_socket_and_reject_inline_certificates() {
    let options = PgConnectOptions::new()
        .socket("/tmp/private socket")
        .username("owner")
        .database("actual");
    assert!(
        NativeConnection::from_options(&options.clone().ssl_mode(PgSslMode::VerifyFull))
            .unwrap_err()
            .to_string()
            .contains("explicit sslrootcert")
    );
    let native = NativeConnection::from_options(&options).unwrap();
    let mut command = tokio::process::Command::new("/explicit/pg_dump");
    native.apply(&mut command);
    assert!(
        command
            .as_std()
            .get_envs()
            .any(|(k, v)| k == "PGHOST" && v.unwrap() == "/tmp/private socket")
    );
    assert!(
        NativeConnection::from_options(
            &options.ssl_root_cert_from_pem(b"-----BEGIN CERTIFICATE-----".to_vec())
        )
        .is_err()
    );
}

#[tokio::test]
async fn tool_warnings_fail_without_echoing_diagnostics_or_certifying_output() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let tool = temp.path().join("pg_dump");
    std::fs::write(&tool, "#!/bin/sh\nprintf 'pg_dump (PostgreSQL) 18.3\\n'\nprintf 'fixture-secret-password\\n' >&2\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut artifact = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
    let output = artifact.as_file_mut();
    let error = ygg::db::backup::dump(temp.path(), &PgConnectOptions::new(), output)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("warnings"));
    assert!(!format!("{error:?}").contains("fixture-secret"));
    assert_eq!(output.metadata().unwrap().len(), 0);
}

#[tokio::test]
async fn dump_refuses_nonempty_or_public_output_before_launching_tools() {
    use std::{io::Write, os::unix::fs::PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let mut artifact = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
    let output = artifact.as_file_mut();
    output.write_all(b"preserve me").unwrap();
    let error = ygg::db::backup::dump(temp.path(), &PgConnectOptions::new(), output)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("empty private regular file"));
    assert_eq!(output.metadata().unwrap().len(), 11);
    output.set_len(0).unwrap();
    output
        .set_permissions(std::fs::Permissions::from_mode(0o644))
        .unwrap();
    assert!(
        ygg::db::backup::dump(temp.path(), &PgConnectOptions::new(), output)
            .await
            .unwrap_err()
            .to_string()
            .contains("private")
    );
}
