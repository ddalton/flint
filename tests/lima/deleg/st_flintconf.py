"""Design §9's conflict-site matrix, on the wire.

Two clients. A holds a READ delegation on a file; B performs each
mutating operation §5.2 routes through the fence. For every site the
sequence has to be:

    B's FIRST attempt          -> NFS4ERR_DELAY
    A                          -> observes CB_RECALL, returns
    B's retry                  -> succeeds

The first line is the one that matters and the one a looser test drops.
pynfs's own DELEG1 accepts `[NFS4_OK, NFS4ERR_DELAY]` from the
conflicting open, which passes just as happily against a server that
never fenced anything and let B straight through — silent success is
precisely the failure this matrix exists to catch, so DELAY is required
here, not merely permitted.

Each test also requires A to hold a real delegation before B moves. With
the flag off that fails, which is the intended shape: these tests are
expected to FAIL on the control arm, and the runner's table says so.

Run:  testserver.py HOST:PORT/path --maketree --nocleanup flintconf
"""
import threading

from xdrdef.nfs4_const import *
from xdrdef.nfs4_type import *
from .environment import check, fail, create_file, open_file
import nfs_ops

op = nfs_ops.NFS4ops()


def _delegated_file(t, env, name):
    """Client A: create `name`, open it no-create, hold a READ delegation.

    Returns (sess_a, fh, recall_event). The recall event is armed before
    the delegation exists, so no recall can be missed between handing it
    out and B's first move.
    """
    recall = threading.Event()

    def pre_hook(arg, env_):
        recall.stateid = arg.stateid       # must precede set()
        env_.notify = recall.set

    def post_hook(arg, env_, res):
        return res

    sess = env.c1.new_client_session(b"%s_A" % name)
    sess.client.cb_pre_hook(OP_CB_RECALL, pre_hook)
    sess.client.cb_post_hook(OP_CB_RECALL, post_hook)

    # READ-only create: create_file's default is SHARE_ACCESS_BOTH, and
    # a write open by this same client refuses the grant under rule 5.
    check(create_file(sess, name, access=OPEN4_SHARE_ACCESS_READ))
    # And a NO-CREATE open: rule 3 says the CREATE arm never grants.
    res = open_file(sess, name,
                    access=OPEN4_SHARE_ACCESS_READ |
                           OPEN4_SHARE_ACCESS_WANT_READ_DELEG)
    check(res)
    deleg = res.resarray[-2].delegation
    if deleg.delegation_type in (OPEN_DELEGATE_NONE, OPEN_DELEGATE_NONE_EXT):
        fail("CONTROL: client A got no delegation, so nothing below can "
             "say anything about conflicts. Is FLINT_NFS_DELEGATIONS set?")
    return sess, res.resarray[-1].object, recall


def _conflict(t, env, site, ops_fn):
    """The matrix body: B runs `ops_fn`, and must be DELAYed first."""
    name = env.testname(t)
    sess_a, fh, recall = _delegated_file(t, env, name)
    sess_b = env.c1.new_client_session(b"%s_B" % name)

    # ── B's FIRST attempt: must be refused, not quietly served ──────
    res = sess_b.compound(ops_fn(env, name, fh))
    if res.status != NFS4ERR_DELAY:
        fail("site %s: B's first attempt answered %d, expected "
             "NFS4ERR_DELAY (%d). Silent success here means the mutation "
             "ran while A still held a read delegation."
             % (site, res.status, NFS4ERR_DELAY))

    # ── A must actually have been recalled ──────────────────────────
    if not recall.wait(10):
        fail("site %s: B was DELAYed but no CB_RECALL reached A within "
             "10s — a DELAY with no recall behind it never resolves"
             % site)
    env.sleep(.1)
    check(sess_a.compound([op.putfh(fh), op.delegreturn(recall.stateid)]),
          msg="site %s: A returning its delegation" % site)

    # ── B's retry must now succeed ──────────────────────────────────
    for _ in range(20):
        res = sess_b.compound(ops_fn(env, name, fh))
        if res.status != NFS4ERR_DELAY:
            break
        env.sleep(.25)
    if res.status != NFS4_OK:
        fail("site %s: after A returned, B's retry answered %d, expected "
             "OK — the barrier did not lift" % (site, res.status))


def _open_write_ops(env, name, fh):
    claim = open_claim4(CLAIM_NULL, name)
    owner = open_owner4(0, b"conflict owner B")
    how = openflag4(OPEN4_NOCREATE)
    return env.home + [op.open(0, OPEN4_SHARE_ACCESS_WRITE,
                               OPEN4_SHARE_DENY_NONE, owner, how, claim)]


def _remove_ops(env, name, fh):
    return env.home + [op.remove(name)]


def _rename_ops(env, name, fh):
    return env.home + [op.savefh()] + env.home + [
        op.rename(name, name + b"_moved")]


def _link_ops(env, name, fh):
    return [op.putfh(fh), op.savefh()] + env.home + [
        op.link(name + b"_hardlink")]


def _setattr_ops(env, name, fh):
    return [op.putfh(fh),
            op.setattr(stateid4(0, b""), {FATTR4_MODE: 0o600})]


def testConflictOpenWrite(t, env):
    """A write OPEN by another client is DELAYed until the recall resolves

    FLAGS: deleg flintconf
    CODE: FLINTCONF1
    """
    _conflict(t, env, "open_write", _open_write_ops)


def testConflictRemove(t, env):
    """REMOVE of a delegated file is DELAYed until the recall resolves

    FLAGS: deleg flintconf
    CODE: FLINTCONF2
    """
    _conflict(t, env, "remove", _remove_ops)


def testConflictRename(t, env):
    """RENAME of a delegated file is DELAYed until the recall resolves

    The rename_src site. It is worth its own leg because the fence keys
    on the file's identity, not its name.

    FLAGS: deleg flintconf
    CODE: FLINTCONF3
    """
    _conflict(t, env, "rename_src", _rename_ops)


def testConflictLink(t, env):
    """LINK to a delegated file is DELAYed until the recall resolves

    The site that makes hardlink aliasing observable: the new name is a
    second path to the same inode, and a fence that keyed on the
    filehandle rather than the identity would let this through.

    FLAGS: deleg flintconf
    CODE: FLINTCONF4
    """
    _conflict(t, env, "link", _link_ops)


def testConflictSetattr(t, env):
    """SETATTR on a delegated file is DELAYed until the recall resolves

    FLAGS: deleg flintconf
    CODE: FLINTCONF5
    """
    _conflict(t, env, "setattr", _setattr_ops)
