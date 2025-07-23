// wrapper.h - SPDK header wrapper for Flint CSI driver bindings
// This file includes only the SPDK headers needed for Flint's core functionality

#include <spdk/stdinc.h>
#include <spdk/bdev.h>
#include <spdk/blob.h>
#include <spdk/blob_bdev.h>
#include <spdk/env.h>
#include <spdk/log.h>
#include <spdk/util.h>
#include <spdk/uuid.h>

// LVol (Logical Volume) functionality - core for Flint
#include <spdk/lvol.h>

// Note: Removed spdk/nvme.h due to alignment conflicts in generated bindings
// Flint primarily uses bdev/lvol functionality, not direct NVMe access

// Note: Removed headers that might not exist in SPDK v24.01:
// - spdk/app.h (application framework - not needed for embedded use)
// - spdk/init.h (initialization - not needed for embedded use)
// - spdk/jsonrpc.h (might not exist)
// - spdk/rpc.h (might not exist)
// - spdk/version.h (might not exist)
// - spdk/nvmf.h (might not exist)
// - spdk/event.h (might not exist)
// - spdk/thread.h (might not exist)
// - spdk/string.h (might not exist)
// - spdk/bdev_zone.h (might not exist) 