# SPDK upstream tracking

What Flint has reported to SPDK, what each report is waiting on, and which
local patch in `docker/Dockerfile.spdk` it lets us drop. **Check this list
periodically** (at least before every release that bumps the spdk-tgt image)
and update the Status column.

## How to check

```sh
# GitHub issues (bug reports)
gh issue view <N> --repo spdk/spdk --json state,comments \
  --jq '.state, (.comments[-3:][] | "[\(.author.login) \(.createdAt[:10])] \(.body[:300])")'

# Gerrit changes (patches; SPDK reviews on review.spdk.io, not GitHub PRs)
ssh -p 29418 ddalton@review.spdk.io gerrit query --current-patch-set \
  --format=TEXT change:<Change-Id>
# or in a browser: https://review.spdk.io/q/<Change-Id>
```

A change needs a CI +1 Verified and two core-maintainer +2 votes to merge.
Review comments must be answered on Gerrit: amend the commit (keep its
Change-Id), then push again to `refs/for/master`.

## Open items

Both issues were closed by maintainers before we found them, and only a
maintainer can reopen them. We commented with a reproducer, tests and fix,
and asked for a reopen (2026-10-21); the commits say `Fixes #3113` / `Fixes #2144`. If
neither is reopened within about a week, open new issues that link the old
ones, and amend the commits' `Fixes` lines (keep the Change-Ids).

| # | What | GitHub issue | Gerrit change | Local patch | Status (date) |
|---|------|--------------|---------------|-------------|---------------|
| 1 | Snapshot during a cluster allocation writes an extent page over md page 0; the lvolstore fails to load. | spdk/spdk#3113 (closed 2024, unreproducible; reopen requested) | [29555](https://review.spdk.io/c/spdk/spdk/+/29555) (`I452a0154469e29772040eed147e9ef6099709755`) | `blob-extent-page-md0.patch` | 2026-10-21: change pushed; [comment](https://github.com/spdk/spdk/issues/3113#issuecomment-6327105627) asks @jimharris to reopen |
| 2 | A write to a cluster being allocated completes before the mapping is persisted and can be overtaken (raid1 legs diverge; crash loses an acked write). | spdk/spdk#2144 (closed 2024, stale; reopen requested) | [29556](https://review.spdk.io/c/spdk/spdk/+/29556) (`Iedd02cdb146a4af3cd6aaa778006fbc9fa410f48`) | `blob-alloc-window-order.patch` | 2026-10-21: change pushed; [comment](https://github.com/spdk/spdk/issues/2144#issuecomment-6327106023) asks @tomzawadzki to reopen |

Evidence for both: bench4, 2026-10-21
(`bench/results/2026-10-21-ec2-phase3/`: `lvs-load-failure/ANALYSIS.md`,
`chaos/diverge4-rc2-off-*`, `chaos/window-summary.txt`). Upstream base:
master 58e2d77bb. The stock-master ports and the md0 reproducer
(`repro-md0.sh`) were built on the box under `/mnt/nvme/spdk-upstream/`.

## When an item lands upstream

1. Note the SPDK release that first contains it.
2. When Flint moves its SPDK base to that release, delete the local patch and
   its `COPY`/`patch -p1` lines from `docker/Dockerfile.spdk`, and bump the
   spdk-tgt tag (`scripts/release.sh` refuses a stale pin).
3. If upstream changed the fix, re-run the bench4 divergence and reload checks
   on the new image before shipping it.

## Candidates not yet reported

- Seen while porting #1/#2, on master (unverified beyond code reading):
  - a double release of an extent page (insert finds the slot occupied, then
    `blob_insert_cluster_revert` releases it again on a later write error);
  - `new_extent_page` leaks on the `!ctx->seq` ENOMEM path in
    `bs_allocate_and_copy_cluster`;
  - a `blob_sync_md` failure after `*extent_page` is set leaves the cluster
    mapped while the caller releases it.
- `blob_serialize_add_page()` allocates `md_page_size` bytes per md page but
  zeroes only `sizeof(struct spdk_blob_md_page)`, so an md page larger than
  the struct is written with uninitialized bytes after it (found when
  patch set 1 of 29555 failed SPDK's valgrind unit-test job, 2026-10-21; it
  also indexes the page array by the struct size, which looks wrong when
  `md_page_size` is larger -- unverified).
- Thin lvols: blocks of a newly allocated cluster that were never written
  read back data from deleted volumes (cross-tenant exposure). If reported,
  it may belong in SPDK's CVE process (https://spdk.io/cve_threat/), not a
  public issue.
