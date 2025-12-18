# LAYOUTGET Integration Plan - Clean Architecture

**Goal**: Add pNFS support to CompoundDispatcher without breaking standalone NFS

---

## Architecture Principles

### 1. **Backward Compatibility**
- Standalone NFS must continue to work exactly as before
- pNFS handler is OPTIONAL
- When no pNFS handler: return NFS4ERR_NOTSUPP for pNFS operations

### 2. **Modularity**
- pNFS operations cleanly separated
- No changes to existing operation handlers
- Clean dependency injection

### 3. **Zero Overhead for Non-pNFS**
- No performance impact on standalone NFS
- pNFS check is a simple Option::is_some()

---

## Implementation Steps

### Step 1: Add pNFS Handler Trait

```rust
// src/pnfs/handler_trait.rs (NEW FILE)

pub trait PnfsOperations: Send + Sync {
    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError>;
    fn getdeviceinfo(&self, args: GetDeviceInfoArgs) -> Result<GetDeviceInfoResult, GetDeviceInfoError>;
    fn layoutreturn(&self, args: LayoutReturnArgs) -> Result<(), String>;
}
```

### Step 2: Implement Trait for PnfsOperationHandler

```rust
// src/pnfs/mds/operations/mod.rs

impl PnfsOperations for PnfsOperationHandler {
    fn layoutget(&self, args: LayoutGetArgs) -> Result<LayoutGetResult, LayoutGetError> {
        // Existing implementation
    }
    // ... etc
}
```

### Step 3: Add Optional pNFS Handler to Dispatcher

```rust
// src/nfs/v4/dispatcher.rs

pub struct CompoundDispatcher {
    // ... existing fields ...
    
    /// Optional pNFS handler (only set for pNFS MDS mode)
    pnfs_handler: Option<Arc<dyn PnfsOperations>>,
}

impl CompoundDispatcher {
    pub fn new(/* existing params */) -> Self {
        Self::new_with_pnfs(/* params */, None)
    }
    
    pub fn new_with_pnfs(
        /* existing params */,
        pnfs_handler: Option<Arc<dyn PnfsOperations>>,
    ) -> Self {
        Self {
            // ... existing fields ...
            pnfs_handler,
        }
    }
}
```

### Step 4: Handle pNFS Operations in Dispatcher

```rust
// src/nfs/v4/dispatcher.rs - in dispatch_operation()

match operation {
    // ... existing operations ...
    
    Operation::Unsupported(opcode) => {
        // Check if this is a pNFS operation
        if Self::is_pnfs_opcode(opcode) {
            if let Some(ref pnfs) = self.pnfs_handler {
                // Delegate to pNFS handler
                return self.handle_pnfs_operation(opcode, context, pnfs).await;
            }
        }
        // Return NotSupp for truly unsupported or pNFS without handler
        warn!("Unsupported operation: opcode={}", opcode);
        OperationResult::Unsupported(Nfs4Status::NotSupp)
    }
}

fn is_pnfs_opcode(opcode: u32) -> bool {
    matches!(opcode, 47 | 48 | 49 | 50 | 51)  // GETDEVICEINFO through LAYOUTRETURN
}
```

### Step 5: Update MDS to Use pNFS-Aware Dispatcher

```rust
// src/pnfs/mds/server.rs

impl MetadataServer {
    pub fn new(config: MdsConfig, exports: Vec<ExportConfig>) -> Result<Self> {
        // ... existing setup ...
        
        // Create pNFS operation handler
        let operation_handler = Arc::new(PnfsOperationHandler::new(
            layout_manager,
            device_registry,
        ));
        
        // Create dispatcher WITH pNFS support
        let base_dispatcher = Arc::new(CompoundDispatcher::new_with_pnfs(
            fh_manager,
            state_mgr,
            lock_mgr,
            Some(operation_handler as Arc<dyn PnfsOperations>),  // ← Pass pNFS handler
        ));
        
        // ...
    }
}
```

### Step 6: Standalone NFS Unchanged

```rust
// src/nfs_main.rs (standalone NFS server)

// Create dispatcher WITHOUT pNFS (existing code unchanged)
let dispatcher = Arc::new(CompoundDispatcher::new(
    fh_manager,
    state_mgr,
    lock_mgr,
    // No pNFS handler passed - uses default None
));
```

---

## Benefits of This Approach

### ✅ Clean Separation
- pNFS code is in pnfs/ module
- NFSv4 dispatcher doesn't know pNFS details
- Trait-based abstraction

### ✅ Backward Compatible
- Standalone NFS: Zero changes, works exactly as before
- Tests: No modifications needed
- Performance: No overhead when pNFS not used

### ✅ Testable
- Can test with and without pNFS handler
- Mock pNFS handler for testing
- Unit tests for each component

### ✅ Maintainable
- Clear ownership of code
- Easy to add new pNFS operations
- No spaghetti dependencies

---

## File Changes Required

### New Files
1. `src/pnfs/handler_trait.rs` - PnfsOperations trait
2. Update `src/pnfs/mod.rs` - Export trait

### Modified Files
1. `src/nfs/v4/dispatcher.rs`
   - Add optional pnfs_handler field
   - Add new_with_pnfs() constructor
   - Handle pNFS opcodes in Unsupported case

2. `src/pnfs/mds/operations/mod.rs`
   - Implement PnfsOperations trait

3. `src/pnfs/mds/server.rs`
   - Use new_with_pnfs() when creating dispatcher

### Unchanged Files
- ✅ `src/nfs_main.rs` - Standalone NFS
- ✅ `src/nfs/v4/operations/*` - All operation handlers
- ✅ All tests

---

## Estimated Time

- Step 1-2: Create trait and implement - 15 minutes
- Step 3: Modify dispatcher - 20 minutes
- Step 4: Add pNFS handling logic - 30 minutes
- Step 5: Update MDS - 10 minutes
- Step 6: Test - 15 minutes

**Total**: ~90 minutes

---

## Testing Strategy

### 1. Verify Standalone NFS Still Works
```bash
cargo test --lib  # All 126 tests should pass
```

### 2. Verify pNFS MDS Compiles
```bash
cargo build --bin flint-pnfs-mds
```

### 3. Deploy and Test
```bash
# Deploy with new image
# Mount client
# Check for LAYOUTGET in logs (should see it handled, not "Unsupported")
```

---

## Success Criteria

- ✅ cargo test --lib passes (126/126)
- ✅ Standalone NFS unchanged
- ✅ pNFS MDS handles LAYOUTGET
- ✅ MDS logs show: "📥 LAYOUTGET" and "✅ LAYOUTGET successful"
- ✅ Client successfully writes files via pNFS

---

**Ready to implement?** This approach is clean, modular, and safe.

