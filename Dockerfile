FROM --platform=$BUILDPLATFORM ghcr.io/nushell/nushell@sha256:4a635f5d1e7b7f22293daf4a3dd67de7eda50f6c3fd350ce9e622ece65463214 AS nushell

FROM --platform=$BUILDPLATFORM docker.io/library/rust@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS frontend
ARG BUILDARCH
RUN test "$BUILDARCH" = amd64
RUN printf '%s\n' \
      'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20260801T000000Z bookworm main' \
      > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/* \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends \
      ca-certificates=20230311+deb12u1 \
      clang=1:14.0-55.7~deb12u1 \
      curl=7.88.1-10+deb12u15 \
      gzip=1.12-1 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown
COPY --from=nushell /usr/bin/nu /usr/bin/nu
RUN archive=/tmp/cargo-leptos.tar.gz \
    && curl --fail --location --silent --show-error --output "$archive" \
      https://github.com/leptos-rs/cargo-leptos/releases/download/v0.3.7/cargo-leptos-x86_64-unknown-linux-gnu.tar.gz \
    && printf '%s  %s\n' fda80f4845e92d0e8f5ec13cf1a46982ba7a518ae01182e7e4201312944bc05d "$archive" \
      | sha256sum --check --strict \
    && tar --extract --gzip --file "$archive" --directory /usr/local/bin --strip-components=1 \
      cargo-leptos-x86_64-unknown-linux-gnu/cargo-leptos \
    && rm "$archive" \
    && test "$(cargo leptos --version)" = 'cargo-leptos 0.3.7'
WORKDIR /app/frontend
COPY frontend ./
RUN nu --no-config-file scripts/build-release.nu \
    && test -f dist/app.html

FROM --platform=$BUILDPLATFORM docker.io/library/rust@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS backend
ARG TARGETARCH
RUN test "$TARGETARCH" = amd64 -o "$TARGETARCH" = arm64
RUN if test "$TARGETARCH" = arm64; then \
      printf '%s\n' \
        'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20260801T000000Z bookworm main' \
        > /etc/apt/sources.list \
      && rm -f /etc/apt/sources.list.d/* \
      && apt-get -o Acquire::Check-Valid-Until=false update \
      && apt-get install -y --no-install-recommends \
        gcc-aarch64-linux-gnu=4:12.2.0-3 \
        libc6-dev-arm64-cross=2.36-8cross1 \
      && rm -rf /var/lib/apt/lists/*; \
    fi
RUN target="$(if test "$TARGETARCH" = amd64; then printf x86_64-unknown-linux-gnu; else printf aarch64-unknown-linux-gnu; fi)" \
    && rustup target add "$target"
WORKDIR /app
COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src ./src
RUN target="$(if test "$TARGETARCH" = amd64; then printf x86_64-unknown-linux-gnu; else printf aarch64-unknown-linux-gnu; fi)" \
    && if test "$TARGETARCH" = arm64; then \
      CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        cargo build --locked --release --target "$target"; \
    else \
      cargo build --locked --release --target "$target"; \
    fi \
    && install -D -m 0755 "target/$target/release/pdf-tools-server" /out/pdf-tools-server

FROM --platform=$BUILDPLATFORM docker.io/library/debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS pdfium
ARG TARGETARCH=amd64
RUN printf '%s\n' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20260801T000000Z bookworm main' \
      > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/* \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends \
      ca-certificates=20230311+deb12u1 \
      curl=7.88.1-10+deb12u15 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=nushell /usr/bin/nu /usr/bin/nu
COPY --chmod=0755 scripts/install-pdfium.nu /usr/local/bin/install-pdfium
RUN install-pdfium /pdfium "$TARGETARCH"

FROM docker.io/library/debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
RUN printf '%s\n' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20260801T000000Z bookworm main' \
      > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/* \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends \
      ca-certificates=20230311+deb12u1 \
      curl=7.88.1-10+deb12u15 \
      gosu=1.14-1+b10 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
RUN groupadd --system pdf-tools \
    && useradd --system --gid pdf-tools --home-dir /app --no-create-home pdf-tools
COPY --from=backend /out/pdf-tools-server /usr/local/bin/pdf-tools-server
COPY --from=pdfium /pdfium/lib/libpdfium.so /usr/local/lib/libpdfium.so
RUN ldconfig
COPY --from=frontend /app/frontend/dist ./frontend/dist
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN mkdir -p /app/data && chown pdf-tools:pdf-tools /app/data
RUN chmod 0755 /usr/local/bin/docker-entrypoint.sh
ENV PORT=3000
ENV PDF_TOOLS_BIND_ADDRESS=0.0.0.0
ENV PDF_TOOLS_PDFIUM_PATH=/usr/local/lib/libpdfium.so
ENV PDF_TOOLS_DATA_DIR=/app/data
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PORT:-3000}/health" || exit 1
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["pdf-tools-server"]
