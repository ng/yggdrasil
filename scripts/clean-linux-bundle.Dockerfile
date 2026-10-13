# Runtime-only qualification environment: no PostgreSQL, Rust, or Docker tools.
FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates curl git python3 \
    libossp-uuid16 libxml2 liblz4-1 libzstd1 libssl3t64 libgssapi-krb5-2 zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && ! command -v postgres \
    && ! command -v initdb \
    && ! command -v cargo \
    && ! command -v docker \
    && test ! -e /usr/lib/postgresql \
    && useradd --create-home --uid 10001 smoke
USER smoke
ENV HOME=/home/smoke LANG=C.UTF-8
WORKDIR /tmp
ENTRYPOINT ["python3", "/source/scripts/smoke-release-bundle.py"]
