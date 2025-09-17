# SPDK Flint Node Agent - Pure C Docker Build
#
# Multi-stage build for the C implementation with Ulfius framework

# Stage 1: Base image with SPDK and dependencies
FROM ubuntu:22.04 AS base

ENV DEBIAN_FRONTEND=noninteractive
ENV SPDK_VERSION=23.09

# Install build dependencies
RUN apt-get update && apt-get install -y \
    # Build tools
    gcc \
    g++ \
    make \
    cmake \
    pkg-config \
    autoconf \
    automake \
    libtool \
    # SPDK dependencies
    libaio-dev \
    libssl-dev \
    libnuma-dev \
    libcunit1-dev \
    uuid-dev \
    libiscsi-dev \
    python3 \
    python3-pip \
    # Ulfius dependencies
    libmicrohttpd-dev \
    libjansson-dev \
    libcurl4-gnutls-dev \
    libgnutls28-dev \
    libgcrypt20-dev \
    libsystemd-dev \
    # Utilities
    git \
    wget \
    curl \
    vim \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Install Orcania (Ulfius dependency) - must be installed first
RUN git clone https://github.com/babelouest/orcania.git /tmp/orcania && \
    cd /tmp/orcania && \
    git checkout v2.3.3 && \
    mkdir build && cd build && \
    cmake .. && \
    make && \
    make install && \
    ldconfig && \
    rm -rf /tmp/orcania

# Install Yder (Ulfius dependency) - must be installed before Ulfius
RUN git clone https://github.com/babelouest/yder.git /tmp/yder && \
    cd /tmp/yder && \
    git checkout v1.4.20 && \
    mkdir build && cd build && \
    cmake .. && \
    make && \
    make install && \
    ldconfig && \
    rm -rf /tmp/yder

# Install Ulfius from source (for latest version)
RUN git clone https://github.com/babelouest/ulfius.git /tmp/ulfius && \
    cd /tmp/ulfius && \
    git checkout v2.7.15 && \
    mkdir build && cd build && \
    cmake .. && \
    make && \
    make install && \
    ldconfig && \
    rm -rf /tmp/ulfius

# Build and install SPDK
RUN git clone --depth 1 --branch v${SPDK_VERSION} https://github.com/spdk/spdk /tmp/spdk && \
    cd /tmp/spdk && \
    git submodule update --init && \
    ./scripts/pkgdep.sh --rdma && \
    ./configure --with-rdma --with-shared \
        --with-crypto --with-vhost --with-virtio \
        --disable-tests --disable-unit-tests --disable-examples && \
    make -j$(nproc) && \
    make install && \
    ldconfig && \
    rm -rf /tmp/spdk

# Stage 2: Build the node agent
FROM base AS builder

WORKDIR /app

# Copy source files
COPY src/main.c src/
COPY Makefile .

# Build the node agent
RUN make clean && make release

# Stage 3: Runtime image
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# Install runtime dependencies only
RUN apt-get update && apt-get install -y \
    # Required libraries
    libaio1 \
    libnuma1 \
    libssl3 \
    libmicrohttpd12 \
    libjansson4 \
    libcurl4 \
    libgnutls30 \
    uuid-runtime \
    # Utilities
    curl \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Copy SPDK libraries from builder
COPY --from=builder /usr/local/lib/libspdk*.so* /usr/local/lib/
COPY --from=builder /usr/local/lib/librte*.so* /usr/local/lib/
COPY --from=builder /usr/local/lib/libisal*.so* /usr/local/lib/

# Copy Ulfius and dependencies from builder
COPY --from=builder /usr/local/lib/libulfius.so* /usr/local/lib/
COPY --from=builder /usr/local/lib/liborcania.so* /usr/local/lib/
COPY --from=builder /usr/local/lib/libyder.so* /usr/local/lib/

# Copy the built binary
COPY --from=builder /app/spdk_flint /usr/local/bin/spdk_flint

# Update library cache
RUN ldconfig

# Create non-root user
RUN useradd -m -s /bin/bash spdk && \
    mkdir -p /var/tmp /var/log/spdk && \
    chown -R spdk:spdk /var/tmp /var/log/spdk

# Expose ports
EXPOSE 8090 9809

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:9809/health || exit 1

# Run as non-root user
USER spdk

# Default environment variables
ENV NODE_ID=node-1
ENV LOG_LEVEL=info
ENV HEALTH_PORT=9809
ENV NODE_AGENT_PORT=8090
ENV TARGET_NAMESPACE=flint-system
ENV SPDK_RPC_SOCKET=/var/tmp/spdk.sock
ENV DISCOVERY_INTERVAL=30

# Entry point
ENTRYPOINT ["/usr/local/bin/spdk_flint"]

# Default command (can be overridden)
CMD []