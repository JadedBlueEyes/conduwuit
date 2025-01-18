FROM --platform=$BUILDPLATFORM docker.io/tonistiigi/xx AS xx
FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm AS base
FROM --platform=$BUILDPLATFORM rust:1-slim-bookworm AS builder

# Don't delete the apt cache
RUN rm -f /etc/apt/apt.conf.d/docker-clean

ARG LLVM_VERSION=19
# Install repo tools
# Line one: compiler tools
# Line two: curl, for downloading binaries
# Line three: for xx-verify
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y \
    clang-${LLVM_VERSION} lld-${LLVM_VERSION} pkg-config make \
    curl \
    file

# Create symlinks for clang
RUN ln -s /usr/bin/clang-${LLVM_VERSION} /usr/bin/clang
RUN ln -s /usr/bin/clang-${LLVM_VERSION} /usr/bin/clang++
RUN ln -s /usr/bin/lld-${LLVM_VERSION} /usr/bin/lld

# Developer tool versions
# renovate: datasource=github-releases depName=cargo-bins/cargo-binstall
ENV BINSTALL_VERSION=1.10.21
# renovate: datasource=github-releases depName=psastras/sbom-rs
ENV CARGO_SBOM_VERSION=0.9.1
# renovate: datasource=crate depName=lddtree
ENV LDDTREE_VERSION=0.3.7

RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
RUN cargo binstall --no-confirm cargo-sbom --version $CARGO_SBOM_VERSION
RUN cargo binstall --no-confirm lddtree --version $LDDTREE_VERSION

# Set up xx (cross-compilation scripts)
COPY --from=xx / /
ARG TARGETPLATFORM

# install libraries linked by the binary
# xx-* are xx-specific meta-packages
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    xx-apt-get install -y \
    xx-c-essentials xx-cxx-essentials \
    liburing-dev

# Set up Rust toolchain
WORKDIR /app
COPY ./rust-toolchain.toml .
RUN rustc --version
RUN rustup target add $(xx-cargo --print-target-triple)

# Get source
COPY . .

# Build binary
# We disable incremental compilation to save disk space, as it only produces a minimal speedup for this case.
ENV CARGO_INCREMENTAL=0

# Configure pkg-config
RUN echo "PKG_CONFIG_LIBDIR=/usr/lib/$(xx-info)/pkgconfig" >> /etc/environment
RUN echo "PKG_CONFIG=/usr/bin/$(xx-info)-pkg-config"
RUN echo "PKG_CONFIG_ALLOW_CROSS=true" >> /etc/environment

# Configure cc to use clang version
RUN echo "CC=clang" >> /etc/environment
RUN echo "CXX=clang++" >> /etc/environment

# Cross-language LTO
RUN echo "CFLAGS=-flto" >> /etc/environment
RUN echo "CXXFLAGS=-flto" >> /etc/environment
RUN echo "RUSTFLAGS='-Clinker-plugin-lto -Clinker=clang -Clink-arg=-fuse-ld=lld'"

# CPU specific optimizations
ARG TARGET_CPU=
RUN set -o allexport && \
    . /etc/environment && \
if [ -n "${TARGET_CPU}" ]; then \
    echo "CFLAGS='${CFLAGS} -march=${TARGET_CPU}'" >> /etc/environment && \
    echo "CXXFLAGS='${CCCFLAGS} -march=${TARGET_CPU}'" >> /etc/environment && \
    echo "RUSTFLAGS='${RUSTFLAGS} -C target-cpu=${TARGET_CPU}'" >> /etc/environment; \
fi

# Conduwuit specific variable
ARG CONDUWUIT_VERSION_EXTRA=
ENV CONDUWUIT_VERSION_EXTRA=$CONDUWUIT_VERSION_EXTRA

RUN cat /etc/environment

RUN mkdir /out
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/app/target \
    set -o allexport && \
    . /etc/environment && \
    xx-cargo build --locked --release && \
    xx-verify ./target/$(xx-cargo --print-target-triple)/release/conduwuit && \
    cp ./target/$(xx-cargo --print-target-triple)/release/conduwuit /out/app

RUN cargo sbom > /out/sbom.spdx.json

# find dynamically linked dependencies
RUN mkdir /out/libs \
    && lddtree /out/app | awk '{print $(NF-0) " " $1}' | sort -u -k 1,1 | awk '{print "install", "-D", $1, "/out/libs" (($2 ~ /^\//) ? $2 : $1)}' | xargs -I {} sh -c {}

FROM scratch

WORKDIR /

# Copy root certs for tls into image
# You can also mount the certs from the host
# --volume /etc/ssl/certs:/etc/ssl/certs:ro
COPY --from=base /etc/ssl/certs /etc/ssl/certs

# Copy our build
COPY --from=builder /out/app ./app
# Copy SBOM
COPY --from=builder /out/sbom.spdx.json ./sbom.spdx.json

# Copy dynamic libraries to root
COPY --from=builder /out/libs /

CMD ["/app"]
