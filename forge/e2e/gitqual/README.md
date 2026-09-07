# Git qualification — the reference client against forge, over HTTP

There is no published conformance suite for a git **server**. What
there is, is the reference implementation. So the qualification that
matters is whether stock `git` — every version of it a user will point
at this — can do the things the protocol says a server can do.

`run-gitqual.sh` drives the real client through that surface against
the real front (`flint-forge-gitcgi` + `git http-backend`), the real
hook, the real syncer and a real S3 API. Not the file transport the
other local drills use: everything here goes over `http://` so the
front is in the path, and the first leg proves it.

```sh
cargo build --manifest-path forge/syncer/Cargo.toml --features s3 --bins
cargo build --manifest-path forge/syncer/Cargo.toml --features gitcgi --bin flint-forge-gitcgi
bash forge/e2e/gitqual/run-gitqual.sh
```

## Why not f10

`forge/e2e/f10-git-ops.sh` is everyday git in a cluster: branch, push,
fetch, pull, merge, tag. This is the protocol's surface, and
deliberately the corners forge has never been asked about — shallow and
unshallow, partial clone, fetch by object id, atomic push,
force-with-lease, mirror clone, protocol v0 against v2, annotated and
lightweight tags, ref names with slashes and UTF-8.

## Three outcomes, and the difference is the point

| | meaning |
|---|---|
| **PASS** | git could do it — or the policy refused it, where a refusal is the documented answer |
| **FAIL** | git could not do it and should have been able to |
| **KNOWN** | git could not do it, forge never claimed it could, and the leg exists so the gap is written down rather than found by a user |

A suite that fails on everything a server might one day support is a
suite nobody reads. A suite that quietly passes what it cannot do is
worse. The `gap` legs are the third thing.

## A second implementation

`gogit/` is a small program built on
[go-git](https://github.com/go-git/go-git) — an independent
implementation of the smart HTTP protocol, written by nobody who has
read forge. Stock git proves forge speaks what git speaks; it cannot
prove forge speaks the *protocol* rather than git's habits. A server
that depended on the exact order the reference client sends things
would pass every leg of the suite above and fail the first other client
a user brings.

```sh
cd forge/e2e/gitqual/gogit && go run . http://127.0.0.1:9723/proj.git driller /tmp/gogit-wc
```
