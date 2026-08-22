# ============================================================
#  Aero Programming Language — Docker official image
#  Build:   docker build -t aero-lang/aero:1.2.0 .
#  Run:     docker run --rm -v $(pwd):/workspace aero-lang/aero run hello.aero
#  Shell:   docker run --rm -it aero-lang/aero
# ============================================================

FROM ubuntu:22.04 AS builder

LABEL maintainer="Aero Team"
LABEL description="Aero Programming Language — Linux Docker Image"
LABEL version="1.2.0"

# Non-interactive apt
ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Download and install Aero
# Note: only the linux-x86_64 asset is currently published by CI.
ARG AERO_VERSION=1.2.0

RUN BINARY="aero-v${AERO_VERSION}-linux-x86_64.tar.gz"; \
    curl -fsSL "https://github.com/SereinCin/aero-lang/releases/download/v${AERO_VERSION}/${BINARY}" \
         -o /tmp/aero.tar.gz && \
    tar -xzf /tmp/aero.tar.gz -C /tmp && \
    cp /tmp/bin/aero /usr/local/bin/aero && \
    rm -rf /tmp/aero.tar.gz /tmp/bin

# ── Runtime stage ───────────────────────────────────────────
# CI builds on ubuntu-latest (24.04 / glibc 2.39), so run on 24.04 to avoid
# GLIBC version mismatches with the compiled binary.
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/aero /usr/local/bin/aero

# Verify the install
RUN aero --help >/dev/null 2>&1

WORKDIR /workspace

# Default command: show help
CMD ["aero", "--help"]

# Usage examples:
#   docker run --rm -v $(pwd):/workspace aero-lang/aero run hello.aero
#   docker run --rm -it aero-lang/aero
#   docker run --rm -v $(pwd):/workspace aero-lang/aero build hello.aero
