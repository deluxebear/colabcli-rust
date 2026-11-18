# Build context contains only this file and the locked Cargo manifests.
FROM rust:1.85.1-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 openssh-client pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install stable --profile minimal --component rustfmt --component clippy \
    && rustup default stable
ENV CARGO_HOME=/opt/validation-cargo \
    CARGO_TARGET_DIR=/validation/target \
    CARGO_BUILD_JOBS=2 \
    CARGO_TERM_COLOR=never \
    CARGO_NET_OFFLINE=true \
    RUST_TEST_THREADS=4 \
    HOME=/validation/home \
    TMPDIR=/validation/tmp \
    PYTHONDONTWRITEBYTECODE=1
WORKDIR /dependency-manifest
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs \
    && CARGO_NET_OFFLINE=false cargo fetch --locked
RUN useradd --uid 1000 --create-home validator \
    && mkdir -p /validation/home /validation/tmp /validation/reports \
    && chown -R 1000:1000 /validation /opt/validation-cargo
USER 1000:1000
WORKDIR /workspace
CMD ["python3", "-B", "/workspace/tools/linux-validation/validate.py"]
