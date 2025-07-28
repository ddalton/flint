# Multi-stage Dockerfile for SPDK Flint C++ Application
# This single Dockerfile replaces all the separate Rust Dockerfiles

#--------------------------------------------------------------------
# Stage 1: Builder with SPDK development environment
#--------------------------------------------------------------------
FROM ubuntu:24.04 AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    pkg-config \
    git \
    wget \
    curl \
    # SPDK dependencies
    libaio-dev \
    libssl-dev \
    libnuma-dev \
    libpciaccess-dev \
    libcunit1-dev \
    libncurses5-dev \
    liburing-dev \
    libfuse3-dev \
    uuid-dev \
    # C++ dependencies
    libspdlog-dev \
    libprotobuf-dev \
    protobuf-compiler \
    libgrpc++-dev \
    grpc-tools \
    # Additional tools
    python3 \
    python3-pyelftools \
    nasm \
    autoconf \
    automake \
    libtool \
    meson \
    && rm -rf /var/lib/apt/lists/*

# Set up working directory
WORKDIR /workspace

# Clone and build SPDK (if not using system packages)
# We'll build SPDK with the same configuration as the original Dockerfile.spdk
RUN git clone https://github.com/spdk/spdk.git && \
    cd spdk && \
    git checkout v25.05.x && \
    git submodule update --init --recursive && \
    ./configure --with-ublk --disable-tests --disable-unit-tests && \
    make -j$(nproc) && \
    make install

# Install Crow web framework (header-only)
RUN git clone https://github.com/CrowCpp/Crow.git && \
    cd Crow && \
    git checkout v1.0+5 && \
    mkdir build && cd build && \
    cmake .. -DCROW_BUILD_EXAMPLES=OFF && \
    make -j$(nproc) && \
    make install

# Copy source code
COPY . /workspace/spdk_flint/

# Build the C++ application
WORKDIR /workspace/spdk_flint
RUN mkdir build && cd build && \
    cmake .. \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local && \
    make -j$(nproc) && \
    make install

# Run tests to ensure everything works
RUN cd build && ctest --output-on-failure

#--------------------------------------------------------------------
# Stage 2: Runtime image with minimal dependencies
#--------------------------------------------------------------------
FROM ubuntu:24.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    # SPDK runtime dependencies
    libaio1t64 \
    libnuma1 \
    libssl3 \
    libpciaccess0 \
    libcunit1 \
    liburing2 \
    libfuse3-3 \
    libuuid1 \
    # C++ runtime dependencies
    libspdlog1.12 \
    libprotobuf32t64 \
    libgrpc++1.51 \
    # System utilities for different modes
    nvme-cli \
    util-linux \
    e2fsprogs \
    xfsprogs \
    mount \
    pciutils \
    curl \
    ca-certificates \
    kmod \
    python3 \
    # Networking
    iproute2 \
    iptables \
    && rm -rf /var/lib/apt/lists/*

# Copy SPDK binaries and libraries from builder
COPY --from=builder /usr/local/bin/spdk_* /usr/local/bin/
COPY --from=builder /usr/local/lib/libspdk* /usr/local/lib/
COPY --from=builder /usr/local/lib/pkgconfig/spdk* /usr/local/lib/pkgconfig/
COPY --from=builder /usr/local/include/spdk/ /usr/local/include/spdk/
COPY --from=builder /usr/local/scripts/ /usr/local/scripts/

# Copy our C++ application
COPY --from=builder /usr/local/bin/spdk_flint /usr/local/bin/

# Create necessary directories
RUN mkdir -p /var/tmp /etc/spdk /var/lib/spdk-csi/backups /csi

# Create runtime user
RUN useradd -m -u 1001 spdk_flint

# Set up library paths
RUN ldconfig

# Create health check script
RUN echo '#!/bin/bash\n\
# Health check script that works for all modes\n\
case "${CSI_MODE:-all}" in\n\
    "dashboard-backend")\n\
        curl -f http://localhost:${DASHBOARD_PORT:-8080}/health || exit 1\n\
        ;;\n\
    "node-agent")\n\
        curl -f http://localhost:${NODE_AGENT_PORT:-8090}/health || exit 1\n\
        ;;\n\
    *)\n\
        curl -f http://localhost:${HEALTH_PORT:-9809}/healthz || exit 1\n\
        ;;\n\
esac\n\
' > /usr/local/bin/health-check.sh && chmod +x /usr/local/bin/health-check.sh

# Set default environment variables
ENV CSI_MODE=all
ENV LOG_LEVEL=info
ENV SPDK_RPC_URL=unix:///var/tmp/spdk.sock
ENV CSI_ENDPOINT=unix:///csi/csi.sock
ENV NVMEOF_TRANSPORT=tcp
ENV NVMEOF_TARGET_PORT=4420
ENV DASHBOARD_PORT=8080
ENV HEALTH_PORT=9809
ENV NODE_AGENT_PORT=8090

# Expose ports for different modes
EXPOSE 4420 8080 8090 9809

# Health check (adapts based on mode)
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD /usr/local/bin/health-check.sh

# Use non-root user for security
USER spdk_flint

# Single entrypoint that handles all modes
ENTRYPOINT ["/usr/local/bin/spdk_flint"]

# Default to help mode for debugging
CMD ["--help"] 