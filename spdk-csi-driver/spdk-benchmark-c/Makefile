SPDK_ROOT_DIR ?= /spdk

CC = gcc
CFLAGS = -Wall -Werror -g -O2
CFLAGS += -I$(SPDK_ROOT_DIR)/include
LDFLAGS = -L$(SPDK_ROOT_DIR)/build/lib
LDFLAGS += -L$(SPDK_ROOT_DIR)/dpdk/build/lib
LDFLAGS += -Wl,--no-as-needed
LDFLAGS += -lspdk_nvme -lspdk_env_dpdk -lspdk_log -lspdk_util -lspdk_vmd -lspdk_keyring -lspdk_json
LDFLAGS += -lspdk_sock -lspdk_trace -lspdk_dma -lspdk_jsonrpc -lspdk_rpc -lspdk_accel
LDFLAGS += -lrte_eal -lrte_mempool -lrte_ring -lrte_mbuf -lrte_pci -lrte_bus_pci -lrte_kvargs -lrte_telemetry -lrte_log
LDFLAGS += -lssl -lcrypto -luuid
LDFLAGS += -lnuma -ldl -lpthread -lm
LDFLAGS += -Wl,--as-needed

APP = spdk-benchmark

all: $(APP)

$(APP): benchmark.o
	$(CC) -o $@ $^ $(LDFLAGS)

benchmark.o: benchmark.c
	$(CC) $(CFLAGS) -c $<

clean:
	rm -f $(APP) *.o

.PHONY: all clean
