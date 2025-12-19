# pNFS RFC Implementation Guide

This document provides a focused guide to the key RFCs needed for implementing pNFS support in Flint.

## Essential RFCs

### 1. [RFC 8881 - NFSv4.1](https://datatracker.ietf.org/doc/html/rfc8881) ⭐ PRIMARY

**Status**: Updated NFSv4.1 specification (obsoletes RFC 5661)  
**Published**: August 2020  
**Relevance**: Contains the complete pNFS specification

#### Key Sections for pNFS Implementation

**Chapter 12: Parallel NFS (pNFS)**
- Section 12.1: Introduction to pNFS
- Section 12.2: pNFS Definitions
- Section 12.3: pNFS Operations
  - `LAYOUTGET` (Section 18.43) - Get layout information
  - `LAYOUTRETURN` (Section 18.44) - Return layout
  - `LAYOUTCOMMIT` (Section 18.42) - Commit written data
  - `GETDEVICEINFO` (Section 18.40) - Get device addressing
  - `GETDEVICELIST` (Section 18.41) - List all devices

**Chapter 13: NFSv4.1 File Layout Type**
- Section 13.1: File Layout Type Definition
- Section 13.2: File Layout Data Types
- Section 13.3: File Layout Operations
- Section 13.4: Stripe Indices and Sparse Files
- Section 13.5: Data Server Multipathing

**Chapter 18: NFSv4.1 Operations**
- Complete operation definitions with XDR
- Error codes and status values
- Operation ordering requirements

**Critical Concepts:**
- Layout types and device IDs (Section 12.2.1-12.2.4)
- Layout stateid semantics (Section 12.5.2)
- Layout recall and return (Section 12.5.5)
- Client ID and session management (Chapter 9)

### 2. [RFC 7862 - NFSv4.2](https://datatracker.ietf.org/doc/html/rfc7862) ⭐ PERFORMANCE

**Status**: Current NFSv4.2 specification  
**Published**: November 2016  
**Relevance**: Performance operations that work with pNFS

#### Key Sections

**Chapter 15: NFSv4.2 Operations**
- Section 15.1: `ALLOCATE` - Pre-allocate space
- Section 15.2: `CLONE` - Clone a file
- Section 15.3: `COPY` - Server-side copy
- Section 15.5: `DEALLOCATE` - Deallocate space
- Section 15.11: `READ_PLUS` - Read with holes
- Section 15.12: `SEEK` - Seek to data/holes
- Section 15.13: `WRITE_SAME` - Write pattern

**Important Notes:**
- NFSv4.2 builds on NFSv4.1 pNFS foundation
- All operations work with pNFS layouts
- Performance operations can be parallelized across data servers

### 3. Supporting RFCs

#### [RFC 5661 - NFSv4.1](https://datatracker.ietf.org/doc/html/rfc5661) (Historical)
**Status**: Obsoleted by RFC 8881  
**Note**: Original pNFS specification, replaced by RFC 8881

#### [RFC 8434 - pNFS Layout Types](https://datatracker.ietf.org/doc/html/rfc8434)
**Published**: August 2018  
**Relevance**: Requirements for new layout types

#### [RFC 5663 - pNFS Block/Volume Layout](https://datatracker.ietf.org/doc/html/rfc5663)
**Relevance**: BLOCK layout type (future implementation)

#### [RFC 5664 - pNFS Object-Based Layout](https://datatracker.ietf.org/doc/html/rfc5664)
**Relevance**: OBJECT layout type (future implementation)

## Implementation Roadmap by RFC Section

### Phase 1: Basic MDS (RFC 8881 Focus)

#### Must Implement:

1. **LAYOUTGET (RFC 8881 Section 18.43)**
   ```c
   struct LAYOUTGET4args {
       bool                    loga_signal_layout_avail;
       layouttype4             loga_layout_type;
       layoutiomode4           loga_iomode;
       offset4                 loga_offset;
       length4                 loga_length;
       length4                 loga_minlength;
       stateid4                loga_stateid;
       count4                  loga_maxcount;
   };
   ```
   - Parse layout request
   - Generate layout based on policy
   - Return layout with device IDs

2. **GETDEVICEINFO (RFC 8881 Section 18.40)**
   ```c
   struct GETDEVICEINFO4args {
       deviceid4               gdia_device_id;
       layouttype4             gdia_layout_type;
       count4                  gdia_maxcount;
       bitmap4                 gdia_notify_types;
   };
   ```
   - Look up device in registry
   - Return network addresses (multipath)

3. **LAYOUTRETURN (RFC 8881 Section 18.44)**
   ```c
   struct LAYOUTRETURN4args {
       bool                    lora_reclaim;
       layouttype4             lora_layout_type;
       layoutiomode4           lora_iomode;
       layoutreturn4           lora_layoutreturn;
   };
   ```
   - Update layout state
   - Clean up recalled layouts

#### Recommended:

4. **LAYOUTCOMMIT (RFC 8881 Section 18.42)**
   ```c
   struct LAYOUTCOMMIT4args {
       offset4                 loca_offset;
       length4                 loca_length;
       bool                    loca_reclaim;
       stateid4                loca_stateid;
       newoffset4              loca_last_write_offset;
       newtime4                loca_time_modify;
       layoutupdate4           loca_layoutupdate;
   };
   ```
   - Update file metadata after writes
   - Commit layout changes

5. **GETDEVICELIST (RFC 8881 Section 18.41)**
   ```c
   struct GETDEVICELIST4args {
       layouttype4             gdla_layout_type;
       count4                  gdla_maxdevices;
       nfs_cookie4             gdla_cookie;
       verifier4               gdla_cookieverf;
   };
   ```
   - List all available devices
   - Support pagination

### Phase 2: FILE Layout Type (RFC 8881 Chapter 13)

#### Data Structures:

**Device Address (Section 13.2.1):**
```c
struct nfs_fh4 {
    opaque data<NFS4_FHSIZE>;
};

struct nfsv4_1_file_layout_ds_addr4 {
    uint32_t                nflda_stripe_indices<>;
    multipath_list4         nflda_multipath_ds_list<>;
};
```

**Layout Segment (Section 13.3):**
```c
struct nfsv4_1_file_layout4 {
    deviceid4               nfl_deviceid;
    uint32_t                nfl_util;
    uint32_t                nfl_first_stripe_index;
    offset4                 nfl_pattern_offset;
    nfs_fh4                 nfl_fh_list<>;
};
```

#### Implementation Details:

1. **Stripe Unit** (Section 13.1.1)
   - Fixed-size data unit
   - Default: 4KB to 1MB
   - Flint recommendation: 8MB

2. **Stripe Indices** (Section 13.4)
   - Map file offsets to data servers
   - Support dense and sparse striping
   - Handle sparse files efficiently

3. **Multipathing** (Section 13.5)
   - Multiple network paths to same DS
   - RDMA and TCP support
   - Client selects optimal path

### Phase 3: NFSv4.2 Performance Operations (RFC 7862)

#### Server-Side Copy (Section 15.3):
```c
struct COPY4args {
    stateid4        ca_src_stateid;
    stateid4        ca_dst_stateid;
    offset4         ca_src_offset;
    offset4         ca_dst_offset;
    length4         ca_count;
    bool            ca_consecutive;
    bool            ca_synchronous;
    netloc4         ca_source_server<>;
};
```
- Works across pNFS layouts
- Can copy between data servers
- Reduces client bandwidth

#### Clone Operation (Section 15.2):
- Copy-on-write file copy
- Instant cloning
- Works with pNFS layouts

## XDR Definitions Reference

### Core pNFS Types (RFC 8881 Section 3.3)

```c
typedef uint32_t        layouttype4;
typedef uint64_t        offset4;
typedef uint64_t        length4;
typedef opaque          deviceid4[16];

const LAYOUT4_NFSV4_1_FILES = 1;
const LAYOUT4_BLOCK_VOLUME  = 2;
const LAYOUT4_OSD2_OBJECTS  = 3;

enum layoutiomode4 {
    LAYOUTIOMODE4_READ = 1,
    LAYOUTIOMODE4_RW   = 2,
    LAYOUTIOMODE4_ANY  = 3
};

enum layoutreturn_type4 {
    LAYOUTRETURN4_FILE = 1,
    LAYOUTRETURN4_FSID = 2,
    LAYOUTRETURN4_ALL  = 3
};

struct layout4 {
    offset4             lo_offset;
    length4             lo_length;
    layoutiomode4       lo_iomode;
    opaque              lo_content<>;
};

struct layoutget_res4 {
    bool                logr_return_on_close;
    stateid4            logr_stateid;
    layout4             logr_layout<>;
};

struct device_addr4 {
    layouttype4         da_layout_type;
    opaque              da_addr_body<>;
};
```

### FILE Layout Specific (RFC 8881 Section 13.2)

```c
struct multipath_list4 {
    netaddr4            ml_entries<>;
};

struct nfsv4_1_file_layouttype4 {
    deviceid4               nfl_deviceid;
    nfs_fh4                 nfl_fh_list<>;
    offset4                 nfl_first_stripe_index;
    offset4                 nfl_pattern_offset;
    uint32_t                nfl_util;
};
```

## Protocol Flow Examples

### Example 1: Client Opens File and Gets Layout (RFC 8881 Section 12.5.1)

```
1. Client → MDS: COMPOUND
   - SEQUENCE
   - PUTFH (root)
   - LOOKUP ("myfile")
   - GETFH
   - OPEN
   - LAYOUTGET (offset=0, length=∞, iomode=READ)

2. MDS → Client: COMPOUND response
   - SEQUENCE OK
   - PUTFH OK
   - LOOKUP OK (filehandle)
   - GETFH OK
   - OPEN OK (stateid)
   - LAYOUTGET OK:
     * Layout type: LAYOUT4_NFSV4_1_FILES
     * Device ID: 0x1234...
     * Stripe size: 8MB
     * File handles for each DS

3. Client → MDS: GETDEVICEINFO (device_id=0x1234...)

4. MDS → Client: GETDEVICEINFO response
   - DS addresses: [10.0.1.1:2049, 10.0.1.2:2049]

5. Client → DS-1: COMPOUND (READ offset=0, count=8MB)
   Client → DS-2: COMPOUND (READ offset=8MB, count=8MB)
   [Parallel I/O]

6. Client → MDS: LAYOUTRETURN

7. Client → MDS: CLOSE
```

### Example 2: Layout Recall (RFC 8881 Section 12.5.5)

```
1. DS-1 fails

2. MDS detects failure (heartbeat timeout)

3. MDS → Client: CB_LAYOUTRECALL
   - Layout type: LAYOUT4_NFSV4_1_FILES
   - IO mode: LAYOUTIOMODE4_ANY
   - Reason: Device unavailable

4. Client → MDS: LAYOUTRETURN
   - Return affected layouts

5. Client → MDS: LAYOUTGET (new layout)

6. MDS → Client: New layout
   - Device IDs: DS-2, DS-3 (excludes DS-1)

7. Client resumes I/O with new layout
```

## Error Codes (RFC 8881 Section 15.1)

### pNFS-Specific Errors:

| Error | Value | Description |
|-------|-------|-------------|
| `NFS4ERR_LAYOUTUNAVAILABLE` | 10049 | Layout not available for file |
| `NFS4ERR_NOMATCHING_LAYOUT` | 10050 | Layout doesn't match |
| `NFS4ERR_RECALLCONFLICT` | 10051 | Layout recall in progress |
| `NFS4ERR_UNKNOWN_LAYOUTTYPE` | 10052 | Unsupported layout type |
| `NFS4ERR_LAYOUTTRYLATER` | 10058 | Layout temporarily unavailable |
| `NFS4ERR_BADIOMODE` | 10033 | Invalid I/O mode |
| `NFS4ERR_BADLAYOUT` | 10051 | Invalid layout |

## Implementation Checklist

### Phase 1: Basic MDS

- [ ] Parse LAYOUTGET XDR (RFC 8881 Section 18.43.1)
- [ ] Implement device registry
- [ ] Generate FILE layout (RFC 8881 Chapter 13)
- [ ] Encode LAYOUTGET response
- [ ] Parse GETDEVICEINFO (RFC 8881 Section 18.40.1)
- [ ] Return device addresses
- [ ] Parse LAYOUTRETURN (RFC 8881 Section 18.44.1)
- [ ] Update layout state

### Phase 2: Layout Policies

- [ ] Round-robin stripe allocation
- [ ] Dense striping (RFC 8881 Section 13.4.1)
- [ ] Sparse file support (RFC 8881 Section 13.4.2)
- [ ] Multipath support (RFC 8881 Section 13.5)

### Phase 3: State Management

- [ ] Layout stateid generation (RFC 8881 Section 12.5.2)
- [ ] Layout recall (RFC 8881 Section 12.5.5)
- [ ] Layout segment tracking
- [ ] Lease management

### Phase 4: Data Server

- [ ] READ operation (RFC 8881 Section 18.22)
- [ ] WRITE operation (RFC 8881 Section 18.32)
- [ ] COMMIT operation (RFC 8881 Section 18.3)
- [ ] Validate stateids from MDS

### Phase 5: Advanced Features (NFSv4.2)

- [ ] COPY operation (RFC 7862 Section 15.3)
- [ ] CLONE operation (RFC 7862 Section 15.2)
- [ ] ALLOCATE operation (RFC 7862 Section 15.1)
- [ ] DEALLOCATE operation (RFC 7862 Section 15.5)

## Testing References

### RFC 8881 Section 21: Security Considerations
- Authentication requirements
- Layout security
- Data server security

### RFC 8881 Appendix A: Error Definitions
- Complete error code list
- Error handling requirements

### RFC 8881 Appendix B: Protocol Data Types
- XDR definitions
- Type sizes and limits

## Quick Reference Links

### Essential Sections:

1. **pNFS Overview**: [RFC 8881 Section 12](https://datatracker.ietf.org/doc/html/rfc8881#section-12)
2. **FILE Layout**: [RFC 8881 Section 13](https://datatracker.ietf.org/doc/html/rfc8881#section-13)
3. **LAYOUTGET**: [RFC 8881 Section 18.43](https://datatracker.ietf.org/doc/html/rfc8881#section-18.43)
4. **GETDEVICEINFO**: [RFC 8881 Section 18.40](https://datatracker.ietf.org/doc/html/rfc8881#section-18.40)
5. **LAYOUTRETURN**: [RFC 8881 Section 18.44](https://datatracker.ietf.org/doc/html/rfc8881#section-18.44)
6. **LAYOUTCOMMIT**: [RFC 8881 Section 18.42](https://datatracker.ietf.org/doc/html/rfc8881#section-18.42)
7. **NFSv4.2 Operations**: [RFC 7862 Section 15](https://datatracker.ietf.org/doc/html/rfc7862#section-15)

### XDR Definitions:

- **pNFS Types**: [RFC 8881 Section 3.3](https://datatracker.ietf.org/doc/html/rfc8881#section-3.3)
- **FILE Layout Types**: [RFC 8881 Section 13.2](https://datatracker.ietf.org/doc/html/rfc8881#section-13.2)
- **Operation Arguments**: [RFC 8881 Section 18](https://datatracker.ietf.org/doc/html/rfc8881#section-18)

### Security & Implementation:

- **Security**: [RFC 8881 Section 21](https://datatracker.ietf.org/doc/html/rfc8881#section-21)
- **Implementation Notes**: [RFC 8881 Section 22](https://datatracker.ietf.org/doc/html/rfc8881#section-22)

## Additional Resources

### Linux Kernel Implementation
- Source: `fs/nfs/pnfs.c`, `fs/nfs/pnfs_nfs.c`
- Good reference for client-side behavior

### Ganesha NFS Server
- Source: `src/FSAL/*/pnfs.c`
- Server-side pNFS implementation

### NFS Test Suite
- `nfstest_pnfs` - pNFS conformance testing
- Tests based on RFC requirements

## Summary

**Start Here:**
1. Read [RFC 8881 Chapter 12](https://datatracker.ietf.org/doc/html/rfc8881#section-12) - pNFS overview
2. Read [RFC 8881 Chapter 13](https://datatracker.ietf.org/doc/html/rfc8881#section-13) - FILE layout
3. Implement LAYOUTGET, GETDEVICEINFO, LAYOUTRETURN

**Then:**
1. Add [RFC 7862](https://datatracker.ietf.org/doc/html/rfc7862) performance operations
2. Test with Linux kernel client
3. Add advanced features

The RFCs are comprehensive but well-organized. Follow the section references above for efficient implementation.







