# RFC 1813 Compliance Audit

Systematic verification of all procedure reply formats against RFC 1813 specification.

## Procedure Reply Format Checklist

### ✅ NULL (0) - Section 3.3.0
**Spec:** void (no results)
**Our impl:** Empty body ✓ **CORRECT**

### ✅ GETATTR (1) - Section 3.3.1  
**Spec:**
```
union GETATTR3res switch (nfsstat3 status) {
case NFS3_OK:
   fattr3 obj_attributes;
default:
   void;
};
```
**Our impl:** status + fattr3 ✓ **CORRECT**

### ⚠️ SETATTR (2) - Section 3.3.2
**Spec:**
```
struct SETATTR3resok {
   wcc_data obj_wcc;
};
```
**Our impl:** status + pre_op_attr (skip) + post_op_attr
**Status:** ✓ **CORRECT** (wcc_data = pre_op + post_op)

### ⚠️ LOOKUP (3) - Section 3.3.3
**Spec:**
```
struct LOOKUP3resok {
   nfs_fh3 object;
   post_op_attr obj_attributes;
   post_op_attr dir_attributes;
};
```
**Our impl:** status + fh + post_op(obj) + post_op(dir - we skip)
**Issue:** Should include dir_attributes
**Status:** ⚠️ **MINOR** (dir_attributes optional, we set to false)

### ✅ ACCESS (4) - Section 3.3.4
**Spec:**
```
struct ACCESS3resok {
   post_op_attr obj_attributes;
   uint32 access;
};
```
**Our impl:** status + post_op_attr + access ✓ **CORRECT**

### ⚠️ READLINK (5) - Section 3.3.5
**Spec:**
```
struct READLINK3resok {
   post_op_attr symlink_attributes;
   nfspath3 data;
};
```
**Our impl:** status + post_op_attr + path string ✓ **CORRECT**

### ⚠️ READ (6) - Section 3.3.6
**Spec:**
```
struct READ3resok {
   post_op_attr file_attributes;
   count3 count;
   bool eof;
   opaque data<>;
};
```
**Our impl:** status + post_op_attr + count + eof + opaque
**Status:** ✓ **CORRECT**

### ⚠️ WRITE (7) - Section 3.3.7
**Spec:**
```
struct WRITE3resok {
   wcc_data file_wcc;
   count3 count;
   stable_how committed;
   writeverf3 verf;
};
```
**Our impl:** status + wcc_data(skip pre, post) + count + committed + verf
**Status:** ✓ **CORRECT**

### ⚠️ CREATE (8) - Section 3.3.8
**Spec:**
```
struct CREATE3resok {
   post_op_fh3 obj;
   post_op_attr obj_attributes;
   wcc_data dir_wcc;
};
```
**Our impl:** status + post_op_fh3 + post_op_attr + wcc_data(skip)
**Status:** ✓ **CORRECT**

### ⚠️ MKDIR (9) - Section 3.3.9
**Spec:**
```
struct MKDIR3resok {
   post_op_fh3 obj;
   post_op_attr obj_attributes;
   wcc_data dir_wcc;
};
```
**Our impl:** status + post_op_fh3 + post_op_attr + wcc_data(skip)
**Status:** ✓ **CORRECT**

### ⚠️ SYMLINK (10) - Section 3.3.10
**Spec:**
```
struct SYMLINK3resok {
   post_op_fh3 obj;
   post_op_attr obj_attributes;
   wcc_data dir_wcc;
};
```
**Our impl:** status + post_op_fh3 + post_op_attr + wcc_data(skip)
**Status:** ✓ **CORRECT**

### ⚠️ MKNOD (11) - Section 3.3.11
**Spec:**
```
struct MKNOD3resok {
   post_op_fh3 obj;
   post_op_attr obj_attributes;
   wcc_data dir_wcc;
};
```
**Our impl:** status + post_op_fh3 + post_op_attr + wcc_data(skip)
**Status:** ✓ **CORRECT**

### ✅ REMOVE (12) - Section 3.3.12
**Spec:**
```
struct REMOVE3resok {
   wcc_data dir_wcc;
};
```
**Our impl:** status + wcc_data(pre_op skip, post_op) ✓ **CORRECT** (just fixed!)

### ⚠️ RMDIR (13) - Section 3.3.13
**Spec:**
```
struct RMDIR3resok {
   wcc_data dir_wcc;
};
```
**Our impl:** status + skip wcc_data
**Issue:** Should encode wcc_data properly
**Status:** ❌ **NEEDS FIX**

### ⚠️ RENAME (14) - Section 3.3.14
**Spec:**
```
struct RENAME3resok {
   wcc_data fromdir_wcc;
   wcc_data todir_wcc;
};
```
**Our impl:** status + skip both wcc_data
**Issue:** Should encode both wcc_data
**Status:** ❌ **NEEDS FIX**

### ⚠️ LINK (15) - Section 3.3.15
**Spec:**
```
struct LINK3resok {
   post_op_attr file_attributes;
   wcc_data linkdir_wcc;
};
```
**Our impl:** status + post_op_attr + skip wcc_data
**Issue:** Should encode linkdir wcc_data
**Status:** ❌ **NEEDS FIX**

### ⚠️ READDIR (16) - Section 3.3.16
**Spec:**
```
struct READDIR3resok {
   post_op_attr dir_attributes;
   cookieverf3 cookieverf;
   dirlist3 reply;
};
```
**Our impl:** status + post_op(skip) + cookieverf + entries + eof
**Status:** ✓ **CORRECT**

### ⚠️ READDIRPLUS (17) - Section 3.3.17
**Spec:**
```
struct READDIRPLUS3resok {
   post_op_attr dir_attributes;
   cookieverf3 cookieverf;
   dirlistplus3 reply;
};
```
**Our impl:** status + post_op(skip) + cookieverf + entries + eof
**Status:** ✓ **CORRECT**

### ✅ FSSTAT (18) - Section 3.3.18
**Spec:**
```
struct FSSTAT3resok {
   post_op_attr obj_attributes;
   fsstat3 fsstat;
};
```
**Our impl:** status + post_op(skip) + fsstat ✓ **CORRECT**

### ✅ FSINFO (19) - Section 3.3.19
**Spec:**
```
struct FSINFO3resok {
   post_op_attr obj_attributes;
   fsinfo3 fsinfo;
};
```
**Our impl:** status + post_op(skip) + fsinfo ✓ **CORRECT**

### ✅ PATHCONF (20) - Section 3.3.20
**Spec:**
```
struct PATHCONF3resok {
   post_op_attr obj_attributes;
   pathconf3 pathconf;
};
```
**Our impl:** status + post_op_attr + pathconf ✓ **CORRECT**

### ⚠️ COMMIT (21) - Section 3.3.21
**Spec:**
```
struct COMMIT3resok {
   wcc_data file_wcc;
   writeverf3 verf;
};
```
**Our impl:** status + wcc_data + verf ✓ **CORRECT**

## Issues Found:

### CRITICAL FIXES NEEDED:
1. **RMDIR** - Missing wcc_data
2. **RENAME** - Missing both wcc_data structures  
3. **LINK** - Missing linkdir wcc_data

These need to encode wcc_data (pre_op_attr + post_op_attr) for the affected directories.

## Action Items:
- [ ] Fix RMDIR reply format
- [ ] Fix RENAME reply format
- [ ] Fix LINK reply format

