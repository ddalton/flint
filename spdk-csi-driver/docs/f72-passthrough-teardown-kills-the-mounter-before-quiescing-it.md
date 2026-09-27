# F72 — passthrough teardown aborts the mounter's FUSE transport BEFORE asking it to stop; lean, in the same function, does the opposite

Status: **ORDERING DEFECT CONFIRMED by a code read 2026-09-22. The
data-loss consequence is PLAUSIBLE AND UNMEASURED — see "What is not
proven". NOT FIXED.** Found reviewing passthrough for improvements after
the ROX work (F70/F71).

## The two orders, side by side

Both live in `s3csi/node.rs`, in the same `NodeUnpublishVolume`.

**Passthrough** (`node.rs:1398-1403`):

```rust
unmount_all(target)…;                                  // the tenant bind
unmount_all(&fuse::ro_stage_of(Path::new(&st.src)))…;  // the ro stage
unmount_all(Path::new(&st.src))…;                      // THE FUSE TRANSPORT
self.release_worker(&st);
worker::delete(&self.client, …, Some(10)).await…;      // fixed 10s
```

**Lean** (`unpublish_lean`, `node.rs:1249+`):

```
refresh_for_drain            → a DRAIN-LENGTH credential, first
release_worker
worker::delete(grace = derived_grace)   → the derived grace, not a constant
record drain_started_unix
wait for worker::is_gone                → WAITS
…then the tree is dealt with
```

Exactly inverted. Lean gives the process its quiesce window and waits for
it to end before touching anything the process is serving. Passthrough
detaches the FUSE mount the mounter is serving, and only then writes the
release marker and deletes the pod.

**That contrast is the argument.** This is not a considered trade in
passthrough's favour; it is the same teardown written twice, carefully
once.

## Why the 10 seconds do not help

`release_worker` (`node.rs:1084-1091`) writes the `released` marker, and
the worker's preStop `await_release`
(`crates/flint-s3-worker/src/main.rs:142-149`) exits **immediately** when
it finds it:

```rust
if marker.exists() {
    // The ordinary path: the plugin released the volume and then deleted us.
    std::process::exit(0);
}
```

That is correct for the preStop's own purpose — it exists to stop a
worker dying while its volume is still PUBLISHED, leaving a tenant on a
dead mount — but it means nothing in the normal unpublish path grants
the mounter any time at all. By the time the marker is written, its FUSE
connection is already gone.

## A second, smaller defect in the same three lines

`unmount_all` (`node.rs:1467-1475`) always calls `fuse::unmount(path,
true)` — `MNT_DETACH` (`fuse.rs:181-188`) — and never attempts a plain
umount first, on any of the three mounts. A lazy umount cannot return
`EBUSY`, so the one signal that would say "something is still using this
mount" is never observed. For `target` that is right (the tenant is
gone). For `src` it discards the only cheap evidence that the mounter is
mid-flight.

## What is not proven, and must not be claimed

**That this loses data.** By the time `NodeUnpublishVolume` runs, kubelet
guarantees every tenant container has exited, so their descriptors are
closed and FUSE `RELEASE` has already been sent. Whether mount-s3 still
has an in-flight `CompleteMultipartUpload` at that instant depends on
whether it finalizes synchronously inside `RELEASE` and how long that
takes — Mountpoint internals, not visible from this repo. `close(2)` does
not wait for the `RELEASE` reply, so a window provably EXISTS; its width
is unmeasured.

So: a real race, with a real mechanism, of unknown probability. The
review that raised it stated the consequence more strongly than the
evidence supports, and that overstatement is corrected here.

## The experiment that settles it — and F71's lesson applies

A code read cannot close this. The rig leg:

1. mount a READ-WRITE passthrough volume;
2. write a large file (large enough to be a real multipart upload —
   several hundred MiB against the pinned mount-s3 1.24.0);
3. delete the tenant pod so the container exits and unpublish follows
   immediately;
4. call `ListMultipartUploads` on the bucket.

**A lingering incomplete multipart upload is the fingerprint.** It is
worth detecting even if no byte is lost: an incomplete MPU bills until a
lifecycle rule reaps it, and nothing in this driver creates such a rule.
Also compare the object's content against what was written.

Per [F71](f71-rox-multi-pod-cannot-fail.md): the leg must be able to
FAIL. A version that only asserts the pod terminated, or that the file
is readable afterwards, would pass against the defect and prove nothing.
The assertion is on `ListMultipartUploads` being empty and the bytes
matching.

## The fix, which is worth making either way

Adopt lean's shape, because the present order has no argument in its
favour and the change is cheap:

1. `unmount_all(target)` — the tenant has already exited, so this is free;
2. `release_worker` + `worker::delete` with a configurable quiesce grace
   (mirror `workers.prestopSecs`; e.g. `workers.mounterQuiesceSecs`,
   default ~15s), then **wait for `worker::is_gone`** as
   `unpublish_lean` does;
3. then `unmount_all(ro_stage)` and `unmount_all(src)`, and
   `remove_state_dir`.

Keep the current order as the fallback once the grace expires, so the
worst case is no worse than today. Consider one non-lazy `unmount`
attempt on `src` before the detaching one, purely to observe `EBUSY`.
