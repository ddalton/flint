// wrapper.h - SPDK header wrapper for Flint CSI driver bindings
// This file includes only the SPDK headers needed for Flint's core functionality

#include <spdk/stdinc.h>
#include <spdk/bdev.h>
#include <spdk/bdev_zone.h>
#include <spdk/blob.h>
#include <spdk/blob_bdev.h>
#include <spdk/env.h>
#include <spdk/event.h>
#include <spdk/log.h>
#include <spdk/string.h>
#include <spdk/thread.h>
#include <spdk/util.h>
#include <spdk/uuid.h>

// LVol (Logical Volume) functionality - core for Flint
#include <spdk/lvol.h>

// NVMe functionality for direct device access
#include <spdk/nvme.h>
#include <spdk/nvmf.h>

// JSON RPC (for compatibility with existing RPC fallback)
#include <spdk/jsonrpc.h>
#include <spdk/rpc.h>

// Application framework
#include <spdk/app.h>
#include <spdk/init.h>

// Version info
#include <spdk/version.h> 