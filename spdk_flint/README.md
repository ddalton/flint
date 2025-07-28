# SPDK Flint - C++ CSI Driver

A unified C++20 implementation of the SPDK-based CSI driver, replacing the previous Rust implementation with direct SPDK integration and improved performance.

## 🚀 Features

- **Single Binary, Multiple Modes**: One application that can run as CSI driver, controller, dashboard backend, or node agent
- **Direct SPDK Integration**: Uses SPDK C APIs directly instead of HTTP RPC calls for better performance
- **Modern C++20**: Leverages modern C++ features for maintainable and efficient code
- **Structured Logging**: Uses spdlog with configurable log levels and context-aware logging
- **Web Dashboard**: Built with Crow web framework for lightweight HTTP services
- **Comprehensive Testing**: Unit and integration tests with Catch2
- **Container Ready**: Single Dockerfile for all deployment modes

## 📁 Project Structure

```
spdk_flint/
├── CMakeLists.txt              # Main build configuration
├── Dockerfile                  # Single Dockerfile for all modes
├── README.md                   # This file
├── include/                    # Header files
│   ├── app.hpp                 # Main application interface
│   ├── logging.hpp             # Logging system
│   └── spdk/
│       └── spdk_wrapper.hpp    # SPDK C++ wrapper
├── src/                        # Implementation files
│   ├── main.cpp                # Application entry point
│   ├── app.cpp                 # Main application logic
│   ├── logging.cpp             # Logging implementation
│   ├── csi/                    # CSI gRPC services
│   ├── spdk/                   # SPDK wrapper implementations
│   ├── dashboard/              # Dashboard backend
│   ├── node_agent/             # Node agent implementation
│   ├── controller_operator/    # Controller operator
│   └── utils/                  # Utility functions
└── test/                       # Test files
    ├── CMakeLists.txt          # Test configuration
    ├── test_main.cpp           # Basic tests
    └── *.cpp                   # Additional test files
```

## 🛠 Building

### Prerequisites

- Ubuntu 24.04 (or compatible)
- CMake 3.20+
- C++20 compatible compiler (GCC 10+, Clang 12+)
- SPDK v25.05.x
- Required libraries:
  - spdlog
  - protobuf
  - gRPC
  - Crow web framework

### Quick Start with Docker

```bash
# Build the container (includes all dependencies)
docker build -t spdk-flint:latest .

# Run in different modes
docker run spdk-flint:latest --mode csi-driver
docker run spdk-flint:latest --mode dashboard-backend
docker run spdk-flint:latest --mode node-agent
```

### Manual Build

```bash
# Install dependencies (Ubuntu 24.04)
sudo apt-get update && sudo apt-get install -y \
    build-essential cmake pkg-config git \
    libspdlog-dev libprotobuf-dev protobuf-compiler \
    libgrpc++-dev grpc-tools \
    libaio-dev libssl-dev libnuma-dev uuid-dev

# Clone and build SPDK
git clone https://github.com/spdk/spdk.git
cd spdk
git checkout v25.05.x
git submodule update --init --recursive
./configure --with-ublk --disable-tests --disable-unit-tests
make -j$(nproc)
sudo make install

# Build SPDK Flint
mkdir build && cd build
cmake .. -DCMAKE_BUILD_TYPE=Release
make -j$(nproc)

# Run tests
ctest --output-on-failure

# Install
sudo make install
```

## 🎯 Usage

### Command Line Options

```bash
spdk_flint [options]

Options:
  --help, -h              Show help message
  --version, -v           Show version information
  --mode <mode>           Operating mode (csi-driver|controller|dashboard-backend|node-agent|all)
  --log-level <level>     Log level (trace|debug|info|warn|error|critical)
  --config <file>         Configuration file path
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `CSI_MODE` | Operating mode | `all` |
| `CSI_ENDPOINT` | CSI socket endpoint | `unix:///csi/csi.sock` |
| `NODE_ID` | Node identifier | `$HOSTNAME` |
| `SPDK_RPC_URL` | SPDK RPC endpoint | `unix:///var/tmp/spdk.sock` |
| `TARGET_NAMESPACE` | Kubernetes namespace | Auto-detected |
| `NVMEOF_TRANSPORT` | NVMe-oF transport | `tcp` |
| `NVMEOF_TARGET_PORT` | NVMe-oF target port | `4420` |
| `DASHBOARD_PORT` | Dashboard HTTP port | `8080` |
| `HEALTH_PORT` | Health check port | `9809` |
| `NODE_AGENT_PORT` | Node agent HTTP port | `8090` |
| `LOG_LEVEL` | Log level | `info` |

### Operating Modes

#### 1. CSI Driver Mode (`csi-driver`)
Runs both CSI controller and node services for volume management.

```bash
spdk_flint --mode csi-driver --log-level debug
```

#### 2. Controller Mode (`controller`)
Runs only the CSI controller service for centralized volume operations.

```bash
CSI_MODE=controller spdk_flint
```

#### 3. Dashboard Backend (`dashboard-backend`)
Provides REST API for the monitoring dashboard.

```bash
spdk_flint --mode dashboard-backend
# Access at http://localhost:8080
```

#### 4. Node Agent (`node-agent`)
Manages local storage devices and SPDK setup.

```bash
spdk_flint --mode node-agent
```

#### 5. All Services (`all`)
Runs all services in a single process (default).

```bash
spdk_flint  # Defaults to all mode
```

## 🔧 Key Improvements Over Rust Version

### 1. Direct SPDK Integration
- **Before**: HTTP RPC calls to SPDK target
- **After**: Direct C function calls for better performance

```cpp
// Old (Rust): HTTP RPC call
http_client.post(spdk_rpc_url)
    .json(json!({"method": "bdev_lvol_create", ...}))

// New (C++): Direct function call
spdk_wrapper->createLvol(lvs_name, lvol_name, size_mib);
```

### 2. Unified Binary
- **Before**: 4 separate Rust binaries with 5 Dockerfiles
- **After**: 1 C++ binary with 1 Dockerfile, multiple modes

### 3. Better Logging
- **Before**: `println!` macros
- **After**: Structured logging with levels and context

```cpp
// Context-aware logging
LOG_CSI_INFO("CreateVolume", "Creating volume {} of size {}", vol_id, size);
LOG_SPDK_ERROR("Failed to create logical volume: {}", error);
```

### 4. Modern Testing
- **Before**: Limited Rust testing
- **After**: Comprehensive testing with Catch2

```cpp
TEST_CASE("Volume Management", "[spdk][volume]") {
    SECTION("Create and delete volume") {
        auto vol_id = spdk_wrapper->createLvol("lvs0", "test-vol", 1024);
        REQUIRE(!vol_id.empty());
        REQUIRE(spdk_wrapper->deleteLvol(vol_id));
    }
}
```

## 🧪 Testing

### Unit Tests
```bash
cd build
make spdk_flint_tests
./spdk_flint_tests
```

### Integration Tests (requires SPDK)
```bash
# Start SPDK target first
sudo spdk_tgt -r /var/tmp/spdk.sock &

# Run integration tests
cd build
make spdk_flint_integration_tests
./spdk_flint_integration_tests
```

### Docker Testing
```bash
# Test all modes
docker run spdk-flint:latest --mode csi-driver --log-level debug
docker run spdk-flint:latest --mode dashboard-backend
docker run spdk-flint:latest --mode node-agent
```

## 🚀 Deployment

### Specialized Docker Images

Instead of one monolithic image, we now provide **optimized images** for each deployment pattern:

```bash
# Build all specialized images
cd spdk_flint
./docker/build-images.sh

# Images created:
# - spdk-flint:base              (Common base)
# - spdk-flint:csi-controller    (Deployment - lightweight)
# - spdk-flint:csi-node         (DaemonSet - privileged)
# - spdk-flint:dashboard-backend (Service - web API)
# - spdk-flint:node-agent       (DaemonSet - disk management)
```

### Kubernetes Deployment

Each image is optimized for its specific deployment pattern:

```yaml
# CSI Controller (Lightweight Deployment)
apiVersion: apps/v1
kind: Deployment
metadata:
  name: spdk-csi-controller
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: csi-controller
        image: spdk-flint:csi-controller  # Specialized image
        resources:
          requests:
            memory: "256Mi"
            cpu: "100m"
          limits:
            memory: "512Mi"
            cpu: "500m"

---
# CSI Node (Privileged DaemonSet)
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: spdk-csi-node
spec:
  template:
    spec:
      hostNetwork: true
      containers:
      - name: csi-node
        image: spdk-flint:csi-node       # Optimized for host access
        securityContext:
          privileged: true
        volumeMounts:
        - name: kubelet-dir
          mountPath: /var/lib/kubelet
          mountPropagation: Bidirectional

---
# Dashboard Backend (Web Service)
apiVersion: apps/v1
kind: Deployment
metadata:
  name: spdk-dashboard-backend
spec:
  template:
    spec:
      containers:
      - name: dashboard-backend
        image: spdk-flint:dashboard-backend  # Web-optimized
        ports:
        - containerPort: 8080
        resources:
          requests:
            memory: "128Mi"
            cpu: "50m"

---
# Node Agent (Storage DaemonSet)
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: spdk-node-agent
spec:
  template:
    spec:
      hostNetwork: true
      containers:
      - name: node-agent
        image: spdk-flint:node-agent     # Disk management tools
        securityContext:
          privileged: true
        volumeMounts:
        - name: device-dir
          mountPath: /dev
      nodeSelector:
        node-type: storage               # Only on storage nodes
```

## 📊 Migration Status

- ✅ **Project Structure**: Complete
- ✅ **Build System**: CMake with SPDK linking
- ✅ **Logging System**: spdlog with structured logging
- ✅ **Testing Framework**: Catch2 setup
- ✅ **Configuration**: Environment variable loading
- ✅ **Docker Integration**: Specialized images for each deployment pattern
- ✅ **SPDK Wrapper**: Core operations implemented with direct C calls
- ✅ **Kubernetes Client**: Full custom resource support with libcurl
- ✅ **Core Application**: Complete with mode switching and service management
- ✅ **Dashboard Backend**: Crow web framework with REST API
- ✅ **Node Agent**: Disk discovery and management logic
- ✅ **Controller Operator**: Volume reconciliation loop
- 🚧 **CSI gRPC Services**: Stubs ready (easy to implement)

## 🤝 Contributing

1. Follow C++20 best practices
2. Use structured logging instead of `std::cout`
3. Add tests for new functionality
4. Update documentation

## 📝 License

Copyright (c) 2024 - SPDK Flint CSI Driver

## 🔗 Related Projects

- [SPDK](https://github.com/spdk/spdk) - Storage Performance Development Kit
- [CSI Specification](https://github.com/container-storage-interface/spec)
- [Crow Web Framework](https://github.com/CrowCpp/Crow)
- [spdlog](https://github.com/gabime/spdlog)
- [Catch2](https://github.com/catchorg/Catch2) 