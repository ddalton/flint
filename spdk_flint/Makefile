# SPDK Flint Node Agent - Pure C Makefile
#
# This Makefile builds the node agent using pure C with Ulfius framework
# to directly integrate with SPDK libraries

CC = gcc
CFLAGS = -Wall -Wextra -O2 -g -std=c11 -D_GNU_SOURCE
LDFLAGS = -pthread

# SPDK configuration (adjust paths as needed)
SPDK_DIR ?= /usr/local
SPDK_INCLUDE = -I$(SPDK_DIR)/include
SPDK_LIB_DIR = $(SPDK_DIR)/lib

# Dependencies
PKG_CONFIG = pkg-config
ULFIUS_CFLAGS = $(shell $(PKG_CONFIG) --cflags libulfius)
ULFIUS_LIBS = $(shell $(PKG_CONFIG) --libs libulfius)
JANSSON_CFLAGS = $(shell $(PKG_CONFIG) --cflags jansson)
JANSSON_LIBS = $(shell $(PKG_CONFIG) --libs jansson)

# Include directories
INCLUDES = $(SPDK_INCLUDE) $(ULFIUS_CFLAGS) $(JANSSON_CFLAGS)

# SPDK libraries (core RPC client functionality)
SPDK_LIBS = \
	-L$(SPDK_LIB_DIR) \
	-lspdk_jsonrpc \
	-lspdk_json \
	-lspdk_rpc \
	-lspdk_log \
	-lspdk_util \
	-lspdk_env_dpdk \
	-lspdk_thread

# System libraries
SYS_LIBS = -lrt -ldl -lm -lnuma -luuid

# All libraries
LIBS = $(SPDK_LIBS) $(ULFIUS_LIBS) $(JANSSON_LIBS) $(SYS_LIBS)

# Source files
SRCS = src/main.c

# Object files
OBJS = $(SRCS:.c=.o)

# Target executable
TARGET = spdk_flint

# Default target
all: $(TARGET)

# Build the executable
$(TARGET): $(OBJS)
	$(CC) $(LDFLAGS) -o $@ $^ $(LIBS)
	@echo "Build complete: $(TARGET)"

# Compile C files
%.o: %.c
	$(CC) $(CFLAGS) $(INCLUDES) -c -o $@ $<

# Clean build artifacts
clean:
	rm -f $(OBJS) $(TARGET)
	@echo "Clean complete"

# Install target
install: $(TARGET)
	install -D -m 755 $(TARGET) /usr/local/bin/$(TARGET)
	@echo "Installed to /usr/local/bin/$(TARGET)"

# Uninstall target
uninstall:
	rm -f /usr/local/bin/$(TARGET)
	@echo "Uninstalled from /usr/local/bin/$(TARGET)"

# Development build with debug symbols and sanitizers
debug: CFLAGS += -DDEBUG -fsanitize=address,leak,undefined -fno-omit-frame-pointer
debug: LDFLAGS += -fsanitize=address,leak,undefined
debug: $(TARGET)

# Release build with optimizations
release: CFLAGS = -Wall -Wextra -O3 -DNDEBUG -std=c11 -D_GNU_SOURCE
release: $(TARGET)

# Static analysis
analyze:
	clang-tidy src/*.c -- $(CFLAGS) $(INCLUDES)

# Format code
format:
	clang-format -i src/*.c

# Check dependencies
check-deps:
	@echo "Checking dependencies..."
	@which gcc >/dev/null 2>&1 || (echo "ERROR: gcc not found"; exit 1)
	@$(PKG_CONFIG) --exists libulfius || (echo "ERROR: libulfius not found"; exit 1)
	@$(PKG_CONFIG) --exists jansson || (echo "ERROR: jansson not found"; exit 1)
	@test -f $(SPDK_LIB_DIR)/libspdk_jsonrpc.so || (echo "WARNING: SPDK libraries not found in $(SPDK_LIB_DIR)"; exit 1)
	@echo "All dependencies found"

# Run the node agent
run: $(TARGET)
	./$(TARGET)

# Run with custom SPDK socket
run-custom: $(TARGET)
	SPDK_RPC_SOCKET=/tmp/custom_spdk.sock ./$(TARGET)

# Test endpoints with curl
test-endpoints:
	@echo "Testing health endpoint..."
	@curl -s http://localhost:9809/health || echo "Health check failed"
	@echo ""
	@echo "Testing version endpoint..."
	@curl -s http://localhost:9809/version | jq . || echo "Version check failed"
	@echo ""
	@echo "Testing status endpoint..."
	@curl -s http://localhost:8090/api/status | jq . || echo "Status check failed"

# Help target
help:
	@echo "SPDK Flint Node Agent - Makefile targets"
	@echo ""
	@echo "  make              - Build the node agent"
	@echo "  make debug        - Build with debug symbols and sanitizers"
	@echo "  make release      - Build optimized release version"
	@echo "  make clean        - Remove build artifacts"
	@echo "  make install      - Install to /usr/local/bin"
	@echo "  make uninstall    - Remove from /usr/local/bin"
	@echo "  make check-deps   - Check for required dependencies"
	@echo "  make run          - Build and run the node agent"
	@echo "  make test-endpoints - Test HTTP endpoints with curl"
	@echo "  make analyze      - Run static analysis"
	@echo "  make format       - Format source code"
	@echo "  make help         - Show this help message"
	@echo ""
	@echo "Environment variables:"
	@echo "  SPDK_DIR          - SPDK installation directory (default: /usr/local)"

.PHONY: all clean install uninstall debug release analyze format check-deps run run-custom test-endpoints help