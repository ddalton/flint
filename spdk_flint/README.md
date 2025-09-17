# SPDK Flint Node Agent - Pure C Implementation

## Overview

This is a pure C implementation of the SPDK Flint Node Agent using the Ulfius web framework. This version replaces the C++ implementation to provide better integration with SPDK libraries and eliminate C++ linking complications.

## Key Features

- **Pure C Implementation**: No C++ dependencies, links directly with SPDK
- **Ulfius Framework**: Lightweight, efficient HTTP server framework for C
- **Direct SPDK Integration**: Uses SPDK's JSON-RPC client library directly
- **RESTful API**: Compatible with the original C++ API endpoints
- **Minimal Dependencies**: Only requires Ulfius, Jansson (JSON), and SPDK libraries

## Architecture Changes

### From C++ to C

| Component | C++ Version | C Version |
|-----------|------------|-----------|
| HTTP Server | Crow (C++) | Ulfius (C) |
| JSON Handling | nlohmann::json | Jansson |
| Logging | spdlog | Simple stdio macros |
| SPDK Interface | Custom RPC wrapper | Direct spdk_jsonrpc_client |
| Build System | CMake | Make |

## API Endpoints

The C implementation maintains the same API endpoints:

- `GET /api/disks/uninitialized` - Discover uninitialized disks
- `POST /api/disks/setup` - Setup disks for SPDK
- `GET /api/lvs` - List logical volume stores
- `POST /api/lvs` - Create logical volume store
- `GET /api/bdevs` - List block devices
- `GET /api/status` - Service status
- `GET /health` - Health check
- `GET /ready` - Readiness check
- `GET /version` - Version information

## Building

### Prerequisites

```bash
# Install Ulfius and dependencies
sudo apt-get install libulfius-dev libjansson-dev

# Or build from source for latest versions
git clone https://github.com/babelouest/ulfius.git
cd ulfius && mkdir build && cd build
cmake .. && make && sudo make install
```

### Build with Make

```bash
# Build the node agent
make

# Build with debug symbols
make debug

# Build optimized release version
make release

# Check dependencies
make check-deps
```

### Build with Docker

```bash
# Build the Docker image
docker build -f Dockerfile.c -t spdk-flint-node-agent:c .

# Run the container
docker run -d \
  --name spdk-flint \
  --privileged \
  -p 8090:8090 \
  -p 9809:9809 \
  -e NODE_ID=node-1 \
  -e SPDK_RPC_SOCKET=/var/tmp/spdk.sock \
  spdk-flint-node-agent:c
```

## Configuration

The node agent can be configured via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `NODE_ID` | node-1 | Node identifier |
| `NODE_AGENT_PORT` | 8090 | API server port |
| `HEALTH_PORT` | 9809 | Health check port |
| `TARGET_NAMESPACE` | flint-system | Kubernetes namespace |
| `SPDK_RPC_SOCKET` | /var/tmp/spdk.sock | SPDK RPC socket path |
| `DISCOVERY_INTERVAL` | 30 | Disk discovery interval (seconds) |
| `LOG_LEVEL` | info | Log level (debug, info, warn, error) |

## Running

```bash
# Run with defaults
./spdk_flint

# Run with custom configuration
NODE_AGENT_PORT=8080 LOG_LEVEL=debug ./spdk_flint

# Run with custom SPDK socket
./spdk_flint --rpc-socket /tmp/custom_spdk.sock
```

## Testing

```bash
# Test health endpoint
curl http://localhost:9809/health

# Get version
curl http://localhost:9809/version | jq .

# Get service status
curl http://localhost:8090/api/status | jq .

# List block devices
curl http://localhost:8090/api/bdevs | jq .

# Discover disks
curl http://localhost:8090/api/disks/uninitialized | jq .
```

## Benefits of C Implementation

1. **Direct SPDK Integration**: Links directly with SPDK libraries without C++ name mangling issues
2. **Lower Memory Footprint**: No C++ STL overhead
3. **Simpler Build Process**: Make instead of CMake reduces complexity
4. **Better Performance**: Direct function calls to SPDK without wrapper overhead
5. **Easier Debugging**: Simpler call stacks without C++ abstractions
6. **Portable**: Pure C code is more portable across different environments

## Migration from C++ Version

The C version maintains API compatibility with the C++ version. To migrate:

1. Stop the C++ version: `systemctl stop spdk-flint`
2. Build and install the C version: `make && sudo make install`
3. Update any systemd service files to use the new binary
4. Start the C version: `systemctl start spdk-flint`

The configuration and API remain the same, so no changes are needed in clients.

## Code Structure

```
src/
├── node_agent.c      # Main implementation file
include/
├── node_agent.h      # Public header with API definitions
Makefile             # Build configuration
Dockerfile.c         # Docker build for C version
```

## Development

```bash
# Format code
make format

# Run static analysis
make analyze

# Run with valgrind for memory debugging
valgrind --leak-check=full ./spdk_flint
```

## Troubleshooting

### SPDK RPC Connection Issues

If the node agent cannot connect to SPDK:

1. Verify SPDK is running: `spdk_tgt`
2. Check socket path: `ls -la /var/tmp/spdk.sock`
3. Verify permissions: Socket should be accessible by the user
4. Test RPC manually: `spdk-rpc.py bdev_get_bdevs`

### Missing Libraries

If you get library errors:

```bash
# Check for missing libraries
ldd spdk_flint

# Install missing dependencies
sudo apt-get install libulfius-dev libjansson-dev

# Update library cache
sudo ldconfig
```

## License

Same as the original SPDK Flint project.