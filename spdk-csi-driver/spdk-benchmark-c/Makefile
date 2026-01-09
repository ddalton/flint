SPDK_ROOT_DIR ?= /spdk

CC = gcc
CFLAGS = -Wall -Werror -g -O2
CFLAGS += -I$(SPDK_ROOT_DIR)/include
LDFLAGS = -L$(SPDK_ROOT_DIR)/build/lib
LDFLAGS += -lspdk_nvme -lspdk_env_dpdk -lspdk_log -lspdk_util -lspdk_vmd
LDFLAGS += -lrte_eal -lrte_mempool -lrte_ring -lrte_mbuf -lrte_pci -lrte_bus_pci -lrte_kvargs -lrte_telemetry
LDFLAGS += -lnuma -ldl -lpthread -lm

APP = spdk-benchmark

all: $(APP)

$(APP): benchmark.o
	$(CC) -o $@ $^ $(LDFLAGS)

benchmark.o: benchmark.c
	$(CC) $(CFLAGS) -c $<

clean:
	rm -f $(APP) *.o

.PHONY: all clean
