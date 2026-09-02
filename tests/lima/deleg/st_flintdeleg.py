"""Design §9's negative delegation legs, as pynfs tests.

Negative legs find the defects — that is the GSS lesson this file is
named after. Every test here asserts a REFUSAL or a well-formed error,
which is the class of behaviour no positive test can reach: a server
that grants nothing passes every "did it grant?" test by accident.

Each test carries its own control where one is possible. "No delegation
was handed out" is the expected result of a working gate AND of a
server that cannot grant at all, so a leg that only asserts the absence
proves nothing. Where the leg is about a gate, it first shows the
server granting on an adjacent path, then shows the gate closing it.

Run:  testserver.py HOST:PORT/path --maketree --nocleanup flintneg
"""
from xdrdef.nfs4_const import *
from xdrdef.nfs4_type import *
from .environment import check, fail, create_file, open_file
import nfs_ops
import nfs4lib

op = nfs_ops.NFS4ops()


def _deleg_of(res):
    """The delegation union from an OPEN result."""
    return res.resarray[-2].delegation


def _got_deleg(deleg):
    return (deleg.delegation_type != OPEN_DELEGATE_NONE and
            deleg.delegation_type != OPEN_DELEGATE_NONE_EXT)


def _shape(res):
    """(opcode, status) per result — what a shape failure should print."""
    return [(r.resop, r.status) for r in res.resarray]


def testCompoundShapeCalibration(t, env):
    """Calibrate what a compound reply looks like, before judging others

    FLINTNEG2 and FLINTNEG3 both assert that the reply contains a result
    for the operation that FAILED. That assertion is only meaningful if
    this client counts results the way the server writes them — and if
    the server's ordinary behaviour is already to include the failing
    op. So this pins both, on operations whose handling is not in
    question:

      [PUTROOTFH, GETATTR]        both succeed -> 2 results
      [PUTFH(garbage), GETATTR]   PUTFH fails  -> 1 result, the failed
                                  PUTFH, and no GETATTR

    Measured, not assumed: pynfs's `resarray` does NOT carry the
    SEQUENCE result that `sess.compound` prepends, so the count is the
    number of ops PASSED IN. Getting this backwards made three
    well-formed replies look like three truncation defects.

    If this test fails, the other two say nothing about the server: they
    are measuring this client's arithmetic.

    FLAGS: deleg flintneg
    CODE: FLINTNEG5
    """
    sess = env.c1.new_client_session(env.testname(t))

    res = sess.compound([op.putrootfh(), op.getattr(1 << FATTR4_LEASE_TIME)])
    if len(res.resarray) != 2:
        fail("all-succeed compound returned %d results, expected 2: %s"
             % (len(res.resarray), _shape(res)))
    if res.resarray[-1].resop != OP_GETATTR:
        fail("all-succeed compound: last op is %d, expected GETATTR: %s"
             % (res.resarray[-1].resop, _shape(res)))

    res = sess.compound([op.putfh(b"\xff" * 8),
                         op.getattr(1 << FATTR4_LEASE_TIME)])
    if len(res.resarray) != 1:
        fail("a compound whose PUTFH fails returned %d results, expected "
             "1 (the failed PUTFH, and nothing after it): %s"
             % (len(res.resarray), _shape(res)))
    if res.resarray[-1].resop != OP_PUTFH:
        fail("the failing op's own result is missing — last op is %d, "
             "expected PUTFH: %s" % (res.resarray[-1].resop, _shape(res)))
    if res.resarray[-1].status == NFS4_OK:
        fail("PUTFH of a garbage filehandle answered OK: %s" % (_shape(res),))


def testNoBackchannelNoDeleg(t, env):
    """A session with no back channel must never be given a delegation

    Grant rule 7: a delegation the server cannot recall is a promise it
    cannot keep. The client here creates its session WITHOUT
    CREATE_SESSION4_FLAG_CONN_BACK_CHAN, so there is no path for
    CB_RECALL, and the server must refuse.

    The first half of this test is the control, and it is not optional:
    "no delegation" is also what a server with the feature switched off
    says, and what a server that is broken says. So this opens on a
    NORMAL session first and requires a delegation there. Only then does
    the same open on a backchannel-less session mean anything.

    FLAGS: deleg flintneg
    CODE: FLINTNEG1
    """
    name = env.testname(t)
    acc = OPEN4_SHARE_ACCESS_READ | OPEN4_SHARE_ACCESS_WANT_READ_DELEG

    # ── control: an ordinary session DOES get a delegation ──────────
    #
    # The open must be a NO-CREATE open. Design §4 rule 3 says the
    # CREATE arm never grants, so a control built on create_file could
    # not have succeeded on any server and would have condemned a
    # working one. Create the file first, then open it.
    sess_ok = env.c1.new_client_session(b"%s_ok" % name)
    # SHARE_ACCESS_BOTH is create_file's default, and a write open
    # blocks the grant by rule 5 — create READ-only.
    check(create_file(sess_ok, name + b"_ok",
                      access=OPEN4_SHARE_ACCESS_READ))
    res = open_file(sess_ok, name + b"_ok", access=acc)
    check(res)
    if not _got_deleg(_deleg_of(res)):
        fail("CONTROL: an ordinary session got no delegation on a "
             "no-create OPEN, so this server cannot say anything about "
             "back channels. Is FLINT_NFS_DELEGATIONS set (and _PNFS "
             "on an MDS)?")

    # ── treatment: no back channel, same open ───────────────────────
    c = env.c1.new_client(b"%s_nocb" % name)
    sess_nocb = c.create_session(flags=0)          # no CONN_BACK_CHAN
    sess_nocb.compound([op.reclaim_complete(FALSE)])

    check(create_file(sess_nocb, name + b"_nocb",
                      access=OPEN4_SHARE_ACCESS_READ))
    res = open_file(sess_nocb, name + b"_nocb", access=acc)
    check(res)
    deleg = _deleg_of(res)
    if _got_deleg(deleg):
        fail("server handed a delegation (type %d) to a client with no "
             "back channel — it can never recall it" % deleg.delegation_type)


def testDelegpurgeCompoundNotTruncated(t, env):
    """A compound containing DELEGPURGE must answer every operation

    The BACKCHANNEL_CTL defect: an operation the dispatcher did not
    understand fell through to an arm that stopped the compound, so the
    client mis-decoded the NEXT operation and blamed that one. The
    symptom is never "DELEGPURGE failed" — it is a confusing error about
    an unrelated op. So the assertion is on the SHAPE of the reply, not
    on DELEGPURGE's own status.

    FLAGS: deleg flintneg
    CODE: FLINTNEG2
    """
    sess = env.c1.new_client_session(env.testname(t))
    res = sess.compound([op.putrootfh(),
                         op.delegpurge(sess.client.clientid),
                         op.getattr(1 << FATTR4_LEASE_TIME)])
    if True:
        # 3 = DELEGPURGE succeeded and GETATTR ran after it.
        # 2 = DELEGPURGE answered an error and stopped the compound,
        #     which is legal AND still carries its own result.
        # 1 would mean DELEGPURGE's own result is missing: the
        #     BACKCHANNEL_CTL shape, where the client then mis-decodes
        #     whatever follows and blames the wrong operation.
        if len(res.resarray) < 2:
            fail("compound [PUTROOTFH, DELEGPURGE, GETATTR] came back "
                 "with %d results: %s — DELEGPURGE's own result is "
                 "missing, so the client mis-decodes whatever follows"
                 % (len(res.resarray), _shape(res)))
        if res.resarray[1].resop != OP_DELEGPURGE:
            fail("result 2 is op %d, expected DELEGPURGE: %s"
                 % (res.resarray[1].resop, _shape(res)))
        if res.resarray[1].status not in (NFS4_OK, NFS4ERR_NOTSUPP):
            fail("DELEGPURGE answered %d; expected OK or NOTSUPP: %s"
                 % (res.resarray[1].status, _shape(res)))
        if res.resarray[1].status == NFS4_OK and len(res.resarray) != 3:
            fail("DELEGPURGE succeeded but GETATTR did not run: %s"
                 % (_shape(res),))


def testDelegreturnBogusStateidIsWellFormed(t, env):
    """DELEGRETURN of an unknown stateid errors without corrupting the reply

    GETATTR is placed BEFORE the failing operation deliberately: a
    compound legitimately stops at the first error, so putting the good
    op first is what distinguishes "stopped correctly at the error"
    from "the decoder lost its place".

    FLAGS: deleg flintneg
    CODE: FLINTNEG3
    """
    sess = env.c1.new_client_session(env.testname(t))
    res = create_file(sess, env.testname(t), access=OPEN4_SHARE_ACCESS_READ)
    check(res)
    fh = res.resarray[-1].object

    bogus = stateid4(1, b"\xa5" * 12)
    res = sess.compound([op.putfh(fh),
                         op.getattr(1 << FATTR4_SIZE),
                         op.delegreturn(bogus)])
    if len(res.resarray) != 3:
        fail("compound [PUTFH, GETATTR, DELEGRETURN] came back with %d "
             "results, not 3: %s — a per-op error must not shorten the "
             "reply BEFORE the op that caused it; DELEGRETURN's own "
             "result has to be there"
             % (len(res.resarray), _shape(res)))
    if res.resarray[1].status != NFS4_OK:
        fail("GETATTR before the bad DELEGRETURN answered %d; it runs "
             "first and must succeed: %s"
             % (res.resarray[1].status, _shape(res)))
    last = res.resarray[2]
    if last.resop != OP_DELEGRETURN:
        fail("result 4 is op %d, expected DELEGRETURN" % last.resop)
    if last.status not in (NFS4ERR_BAD_STATEID, NFS4ERR_STALE_STATEID,
                           NFS4ERR_EXPIRED):
        fail("DELEGRETURN of an unknown stateid answered %d; expected "
             "BAD_STATEID" % last.status)


def testWantNoDelegIsAnsweredNotIgnored(t, env):
    """WANT_NO_DELEG must be answered with NONE_EXT / WND4_NOT_WANTED

    Two things are pinned. The reason code, because a server that
    merely could not grant used to answer WND4_RESOURCE to a client
    that had asked for nothing — "I would have, but I could not" is a
    different statement from "you told me not to". And the reply LENGTH,
    because open_none_delegation4 switches on ond_why and only
    CONTENTION and RESOURCE carry a trailing bool: a bool on a void arm
    raises no error, it shifts every following word of the compound.

    FLAGS: deleg flintneg
    CODE: FLINTNEG4
    """
    sess = env.c1.new_client_session(env.testname(t))
    res = create_file(sess, env.testname(t),
                      access=OPEN4_SHARE_ACCESS_READ |
                             OPEN4_SHARE_ACCESS_WANT_NO_DELEG)
    check(res)
    deleg = _deleg_of(res)
    if deleg.delegation_type != OPEN_DELEGATE_NONE_EXT:
        fail("WANT_NO_DELEG answered delegation_type %d, expected "
             "NONE_EXT (%d)" % (deleg.delegation_type,
                                OPEN_DELEGATE_NONE_EXT))
    if deleg.ond_why != WND4_NOT_WANTED:
        fail("WANT_NO_DELEG answered ond_why %d, expected WND4_NOT_WANTED "
             "(%d) — the client's own instruction must be consulted "
             "before any server-side gate" % (deleg.ond_why,
                                              WND4_NOT_WANTED))
    # The op after OPEN in the compound still decodes: proof the void
    # arm carried no stray bool.
    if res.resarray[-1].resop not in (OP_GETFH, OP_OPEN):
        fail("the operation after OPEN decoded as op %d — the NONE_EXT "
             "arm shifted the rest of the compound"
             % res.resarray[-1].resop)
