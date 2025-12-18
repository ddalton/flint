# Ignored Tests Analysis

**Date**: December 2024  
**Total Ignored**: 15 tests

---

## Test Categories

### Category 1: Compound Encoding Tests (3 tests)

**Location**: `src/nfs/v4/compound.rs`

1. `test_getattr_response_encoding` - Encoding API test
2. `test_getattr_no_double_wrapping` - Encoding API test  
3. `test_secinfo_no_name_dual_flavors` - Encoding API test

**Issue**: Tests reference `encode_single_result()` method that doesn't exist in current API

**Fix Difficulty**: 🟡 Medium (need to understand current encoding API)

**Value**: 🟢 Low (these are internal encoding tests, functionality works)

**Recommendation**: ⏳ **Low priority** - Skip for now, fix if encoding bugs appear

---

### Category 2: I/O Operation Tests (7 tests)

**Location**: `src/nfs/v4/operations/ioops.rs`

1. `test_open_with_file_creation` - Create and open file
2. `test_commit` - COMMIT operation
3. `test_read_with_relaxed_stateid_validation` - READ with special stateids
4. `test_full_write_workflow` - Complete write workflow
5. `test_write_with_relaxed_stateid_validation` - WRITE with special stateids
6. `test_write` - Basic WRITE operation
7. `test_read` - Basic READ operation

**Issue**: "Path outside export" errors (same issue we fixed for filehandle tests)

**Fix Difficulty**: 🟢 Easy (apply same fix: use TempDir + canonicalize)

**Value**: 🔴 High (these test core I/O functionality)

**Recommendation**: ⭐ **HIGH PRIORITY** - Should fix these!

---

### Category 3: Performance Operation Tests (2 tests)

**Location**: `src/nfs/v4/operations/perfops.rs`

1. `test_copy` - NFSv4.2 COPY operation
2. `test_clone` - NFSv4.2 CLONE operation

**Issue**: "Path outside export" errors

**Fix Difficulty**: 🟢 Easy (same TempDir fix)

**Value**: 🟡 Medium (performance features, but already used in production)

**Recommendation**: ⭐ **MEDIUM PRIORITY** - Good to fix

---

### Category 4: Filehandle Test (1 test)

**Location**: `src/nfs/v4/filehandle.rs`

1. `test_filehandle_roundtrip` - Test handle generation and resolution

**Issue**: Unknown (need to run to see)

**Fix Difficulty**: 🟡 Unknown

**Value**: 🟡 Medium (redundant with other filehandle tests)

**Recommendation**: 🔄 **MEDIUM PRIORITY** - Investigate

---

### Category 5: Snapshot Test (1 test)

**Location**: `src/snapshot/snapshot_models.rs`

1. `test_snapshot_name_validation` - Validate snapshot naming

**Issue**: Validation logic bug (assertion fails)

**Fix Difficulty**: 🟢 Easy (update validation logic or test expectations)

**Value**: 🟢 Low (snapshot naming validation)

**Recommendation**: 🔄 **LOW PRIORITY** - Fix if time permits

---

### Category 6: Already Passing (1 test)

**Location**: `src/nfs/v4/operations/ioops.rs`

1. `test_open_without_create` - Test was marked ignored but already works

**Fix Difficulty**: ✅ None (just remove #[ignore])

**Value**: ✅ Free test coverage

**Recommendation**: ⭐⭐⭐ **IMMEDIATE** - Just remove #[ignore]!

---

## Priority Recommendations

### Immediate (1 test) - 5 minutes

**Just remove #[ignore]**:
- ✅ `test_open_without_create` - Already works!

**Impact**: +1 test (113 passing)

---

### High Priority (7 tests) - 30-60 minutes

**I/O Operation Tests** - Core functionality:
- `test_read` - Basic READ
- `test_write` - Basic WRITE
- `test_commit` - COMMIT operation
- `test_open_with_file_creation` - File creation
- `test_full_write_workflow` - Complete workflow
- `test_read_with_relaxed_stateid_validation` - Special stateids
- `test_write_with_relaxed_stateid_validation` - Special stateids

**Fix**: Apply TempDir + canonicalize fix (same as filehandle tests)

**Impact**: +7 tests (119 passing), core I/O verified

---

### Medium Priority (3 tests) - 30 minutes

**Performance Operations**:
- `test_copy` - NFSv4.2 COPY
- `test_clone` - NFSv4.2 CLONE

**Filehandle**:
- `test_filehandle_roundtrip` - Handle roundtrip

**Fix**: Apply TempDir fix, investigate roundtrip issue

**Impact**: +3 tests (122 passing)

---

### Low Priority (4 tests) - Skip for now

**Encoding Tests** (3):
- Require API understanding
- Low value (internal tests)
- Functionality works

**Snapshot Test** (1):
- Validation logic issue
- Low impact

**Impact**: Nice to have, not critical

---

## Recommended Approach

### Phase 1: Quick Win (5 minutes)

1. Remove #[ignore] from `test_open_without_create`

**Result**: 113/127 tests passing (89%)

### Phase 2: Core I/O Tests (30-60 minutes)

1. Fix all 7 I/O operation tests
2. Apply same TempDir + canonicalize pattern
3. Verify READ/WRITE/COMMIT all work

**Result**: 120/127 tests passing (94%)

### Phase 3: Performance Tests (30 minutes)

1. Fix `test_copy` and `test_clone`
2. Fix `test_filehandle_roundtrip`

**Result**: 123/127 tests passing (97%)

### Phase 4: Optional (Later)

1. Fix or remove 4 low-priority tests

**Result**: 127/127 tests passing (100%)

---

## Estimated Total Effort

- **Phase 1**: 5 minutes → 113 passing
- **Phase 2**: 30-60 minutes → 120 passing
- **Phase 3**: 30 minutes → 123 passing
- **Phase 4**: 1-2 hours → 127 passing

**Total**: 2-3 hours to get to 97% (123/127)

---

## Quick Assessment: Run Ignored Tests

Let me run them to see which actually fail vs which might pass now:

