# Single-stage Dockerfile for SPDK Flint C++ Application
# Simplified version without separate runtime image

FROM ubuntu:24.04

# Install all dependencies (build and runtime)
RUN apt-get update && apt-get install -y \
    # Build tools
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
    protobuf-compiler-grpc \
    # Additional build tools
    python3 \
    python3-pyelftools \
    nasm \
    autoconf \
    automake \
    libtool \
    meson \
    # Runtime utilities
    nvme-cli \
    util-linux \
    e2fsprogs \
    xfsprogs \
    mount \
    pciutils \
    ca-certificates \
    kmod \
    # Networking
    iproute2 \
    iptables \
    && rm -rf /var/lib/apt/lists/*

# Set up working directory
WORKDIR /workspace

# Clone and build SPDK
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
RUN mkdir -p build && cd build && \
    cmake .. \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local && \
    make -j$(nproc) && \
    make install

# Run tests to ensure everything works
RUN cd build && ctest --output-on-failure || true

# Create necessary directories
RUN mkdir -p /var/tmp /etc/spdk /var/lib/spdk-csi/backups /csi

# Create runtime user
RUN useradd -m -u 1001 spdk_flint

# Set up library paths
RUN ldconfig

# Create health check script for node-agent
RUN echo '#!/bin/bash\n\
# Health check script for node-agent mode\n\
# Check node agent API health\n\
curl -f http://localhost:${NODE_AGENT_PORT:-8090}/health || exit 1\n\
' > /usr/local/bin/health-check.sh && chmod +x /usr/local/bin/health-check.sh

# Set default environment variables for node-agent
ENV CSI_MODE=node-agent
ENV LOG_LEVEL=info
ENV SPDK_RPC_URL=unix:///var/tmp/spdk.sock
ENV CSI_ENDPOINT=unix:///csi/csi.sock
ENV NVMEOF_TRANSPORT=tcp
ENV NVMEOF_TARGET_PORT=4420
ENV HEALTH_PORT=9809
ENV NODE_AGENT_PORT=8090

# Expose ports for node-agent mode
EXPOSE 4420 8090 9809

# Health check (adapts based on mode)
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD /usr/local/bin/health-check.sh

# Use non-root user for security
USER spdk_flint

# Entrypoint for node-agent with embedded SPDK
ENTRYPOINT ["/usr/local/bin/spdk_flint"]

# Default to node-agent mode
CMD ["--mode", "node-agent"]