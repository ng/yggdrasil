//! External connection policy: local endpoints remain compatible; remote TCP
//! requires certificate-chain and hostname verification, with no TLS downgrade.
use sqlx::postgres::{PgConnectOptions, PgSslMode};

fn invalid(message: &'static str) -> sqlx::Error {
    sqlx::Error::Configuration(message.into())
}

pub fn options(database_url: &str) -> Result<PgConnectOptions, sqlx::Error> {
    let url =
        url::Url::parse(database_url).map_err(|_| invalid("invalid external PostgreSQL URL"))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(invalid("external database requires a PostgreSQL URL"));
    }
    let mut explicit_mode = std::env::var_os("PGSSLMODE").is_some();
    // SQLx logs unknown parameter values. Reject these before handing it the
    // URL so misspelled credentials/settings cannot appear in diagnostics.
    for (key, _) in url.query_pairs() {
        match key.as_ref() {
            "sslmode" | "ssl-mode" => explicit_mode = true,
            "sslrootcert"
            | "ssl-root-cert"
            | "ssl-ca"
            | "sslcert"
            | "ssl-cert"
            | "sslkey"
            | "ssl-key"
            | "statement-cache-capacity"
            | "host"
            | "hostaddr"
            | "port"
            | "dbname"
            | "user"
            | "password"
            | "application_name"
            | "options" => {}
            key if key.starts_with("options[") && key.ends_with(']') => {}
            _ => return Err(invalid("unsupported external PostgreSQL URL parameter")),
        }
    }
    let options: PgConnectOptions = database_url
        .parse()
        .map_err(|_| invalid("invalid external PostgreSQL connection settings"))?;
    let host = options.get_host();
    let local = options.get_socket().is_some()
        || host.starts_with('/')
        || host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if local {
        return Ok(options);
    }
    if explicit_mode && !matches!(options.get_ssl_mode(), PgSslMode::VerifyFull) {
        return Err(invalid(
            "remote PostgreSQL requires sslmode=verify-full; configure sslrootcert for a private CA",
        ));
    }
    Ok(options.ssl_mode(PgSslMode::VerifyFull))
}

/// Role credentials may differ, but an owner URL must not silently redirect an
/// operator migration. Compare effective SQLx endpoints, including query/env
/// defaults, before opening either connection. Different transport endpoints
/// require an explicit deployment move rather than implicit owner selection.
pub fn validate_owner_target(runtime: &str, owner: &str) -> Result<(), sqlx::Error> {
    let runtime = options(runtime)?;
    let owner = options(owner)?;
    let database = |options: &PgConnectOptions| {
        options
            .get_database()
            .unwrap_or(options.get_username())
            .to_owned()
    };
    if runtime.get_host() != owner.get_host()
        || runtime.get_socket() != owner.get_socket()
        || runtime.get_port() != owner.get_port()
        || database(&runtime) != database(&owner)
    {
        return Err(invalid(
            "migration owner URL must select the same host, socket, port and database as the runtime URL",
        ));
    }
    Ok(())
}
