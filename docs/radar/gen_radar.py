#!/usr/bin/env python3
"""Generate flint-approach-radar.html: four 6-axis radar charts (consistency /
performance / security / day-2 ops) in a 2x2 grid, each plotting all approaches.
Data (axes, scores, notes) comes from radar_data.json next to this script.
"""
import json
import re, math, os

# ---------------------------------------------------------------- palette ---
INK      = "#0b0b0b"
INK2     = "#52514e"
MUTED    = "#898781"
GRID     = "#e1e0d9"
AXIS     = "#c3c2b7"
SURFACE  = "#fcfcfb"
PAGE     = "#f9f9f7"

# 4-set validated --pairs all light: worst CVD dE 9.2, normal 19.9; contrast
# WARNs on #5598e7/#1baf7a relieved by per-card tables + dashed encoding.
SERIES_STYLE = {           # short -> (display label, color, dash)
    "NFS":   ("NFS-over-S3 (either flavor)", "#184f95", ""),
    "sys":   ("sec=sys",  "#5598e7", "5 3"),
    "krb5p": ("krb5p",    "#184f95", ""),
    "FUSE":  ("FUSE",     "#eb6834", ""),
    "Lean":  ("Lean",     "#1baf7a", ""),
    "lakeFS": ("lakeFS",  "#4a3aa7", ""),   # slot-7 violet; pair vs aqua validated (CVD dE 31.1)
    # Wine: validated --pairs all against the four flint series (worst CVD
    # dE 9.2 is the pre-existing green/orange pair; normal-vision floor 19.0
    # vs orange) and against lakeFS violet for the page-5 table (22.9).
    "Forge": ("Forge",    "#b03060", ""),
}

RMAX = 5

# ---------------------------------------------------------------- geometry --
W, H = 386, 316
CX, CY, R = 193, 162, 108

def pt(axis_i, v, n):
    ang = math.radians(-90 + axis_i * (360 / n))
    r = R * v / RMAX
    return (CX + r * math.cos(ang), CY + r * math.sin(ang))

def poly(vals):
    n = len(vals)
    return " ".join(f"{x:.1f},{y:.1f}" for x, y in
                    (pt(i, v, n) for i, v in enumerate(vals)))

def rings(n):
    out = []
    for v in range(1, RMAX + 1):
        out.append(f'<polygon points="{poly([v]*n)}" fill="none" '
                   f'stroke="{GRID}" stroke-width="1"/>')
    for v in (1, 3, 5):
        x, y = pt(0, v, n)
        out.append(f'<text x="{x+4:.1f}" y="{y+3:.1f}" font-size="9" '
                   f'fill="{MUTED}">{v}</text>')
    return "\n".join(out)

def label_pos(i, n):
    ang = math.radians(-90 + i * (360 / n))
    c, s = math.cos(ang), math.sin(ang)
    if c > 0.25:
        dx, anc = 7, "start"
    elif c < -0.25:
        dx, anc = -7, "end"
    else:
        dx, anc = 0, "middle"
    dy = (-9 if s < 0 else 16) if anc == "middle" else 4 + 6 * s
    return dx, dy, anc

def axes_svg(names):
    n = len(names)
    out = []
    for i, name in enumerate(names):
        x, y = pt(i, RMAX, n)
        out.append(f'<line x1="{CX}" y1="{CY}" x2="{x:.1f}" y2="{y:.1f}" '
                   f'stroke="{AXIS}" stroke-width="1"/>')
        dx, dy, anc = label_pos(i, n)
        out.append(f'<text x="{x+dx:.1f}" y="{y+dy:.1f}" font-size="11.5" '
                   f'font-weight="600" fill="{INK2}" text-anchor="{anc}">{name}</text>')
    return "\n".join(out)

def series_svg(vals, color, dash, nseries):
    n = len(vals)
    p = poly(vals)
    dash_attr = f' stroke-dasharray="{dash}"' if dash else ""
    fo = 0.12 if nseries <= 3 else 0.08
    marks = "\n".join(
        f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3.6" fill="{color}" '
        f'stroke="{SURFACE}" stroke-width="2"/>'
        for x, y in (pt(i, v, n) for i, v in enumerate(vals)))
    return (f'<polygon points="{p}" fill="{color}" fill-opacity="{fo}" '
            f'stroke="{color}" stroke-width="2.2" stroke-linejoin="round"{dash_attr}/>\n{marks}')

INTRO = """<div class="intro">
  <div class="ib" style="flex:1.25">
    <h3><span class="sw" style="background:#184f95"></span>NFS-over-S3 &nbsp;<span class="ibt">flint-lite &ldquo;hub&rdquo;</span><span class="badge">shipped + drilled</span></h3>
    <p>One <b>hub pod</b> runs an NFS server; client pods mount it over the network with the ordinary
    kernel NFS client &mdash; nothing installed in the pod. The hub keeps the live tree on its own disk
    (a PVC) and asynchronously publishes whole files to an S3 bucket. Because every client talks to one
    server, it is the only approach with true shared-filesystem semantics. Its two flavors differ only
    in how NFS clients are trusted: <b>sec=sys</b> (today&rsquo;s deployment &mdash; the client self-
    declares its uid; the network is the only boundary) and <b>krb5p</b> (Kerberos &mdash; cryptographic
    per-user identity plus wire encryption; needs a KDC and a keytab on every client node; implemented,
    interop-verified against MIT krb5 and mounted as of 2026-08-27 &mdash; still scored as designed,
    because no krb5p throughput has been measured).
    <i class="arch">Archetype: NFS gateway over object storage (AWS Storage Gateway, Nasuni-class).</i></p>
  </div>
  <div class="ib">
    <h3><span class="sw" style="background:#eb6834"></span>FUSE &nbsp;<span class="ibt">flint-fuse</span><span class="badge">designed only, deprioritized</span></h3>
    <p>No server. Each pod gets an injected <b>FUSE filesystem sidecar</b> that intercepts the app&rsquo;s
    file I/O, keeps data in the pod, and publishes that pod&rsquo;s workspace subtree straight to the same
    S3 bucket on a cadence. Each pod holds prefix-scoped bucket credentials, and needs a privileged
    sidecar (/dev/fuse). Designed in full; currently deprioritized in favor of Lean.
    <i class="arch">Archetype: bucket-native FUSE (Mountpoint for S3, gcsfuse, s3fs).</i></p>
  </div>
  <div class="ib">
    <h3><span class="sw" style="background:#1baf7a"></span>Lean &nbsp;<span class="ibt">flint-lean</span><span class="badge">built + chaos-drilled &middot; cluster drill open</span></h3>
    <p>No server and no interception: the app works on <b>plain local files</b>. An ordinary, unprivileged
    sidecar (flint-sync) checks the workspace out of S3 at pod start and re-publishes changed files on a
    cadence, writing to the bucket directly; a small <b>gateway</b> carries human-in-the-loop writes and
    the publish/sync verbs. The agent container holds no bucket credentials. The three file-shaped approaches share one bucket format and are mutually convertible.</p>
  </div>
  <div class="ib">
    <h3><span class="sw" style="background:#b03060"></span>Forge &nbsp;<span class="ibt">flint forge</span><span class="badge">built &middot; drilled on EC2 &middot; numbers verified</span></h3>
    <p>A <b>git server per repository</b> with S3 as its only durable state. Agent pods are stock git
    clients; a stateless <b>door</b> (the lite gateway with a git arm) turns the pod&rsquo;s own ServiceAccount
    token into a principal and routes to the repository&rsquo;s server pod, where real git (nginx +
    <code>http-backend</code>) serves from a local disk that is a cache and one <b>syncer</b> owns every write
    to the bucket &mdash; a push is acknowledged only once its pack and one CAS&rsquo;d snapshot are in S3.
    Idles to zero; the bucket is a bare repository, clonable with the server down. The durable unit is a
    commit: untracked and uncommitted work is not forge&rsquo;s. Numbers checked by four verifiers and
    eight refuters (none moved); the prose is single-pass.
    <i class="arch">Archetype: in-cluster git server over object storage (AWS CodeCommit, closed to new customers 2024).</i></p>
  </div>
</div>"""

def fmt(v):
    return f"{v:g}"


CMP_RE = re.compile(r"\{\{(cmp|cmpL|lean|lake|score):([^/}]+)/([^}]+)\}\}")


def resolve_refs(text, data, lk):
    """Substitute score cross-references with the LIVE numbers.

    The lakeFS page compares lakeFS cells against Lean cells in prose. Those
    numbers were hand-copied, and by 2026-08-27 three of them were
    quoting pre-rescore Lean values — a page silently disagreeing with
    the chart printed above it. Writing {{cmp:Consistency/Ack survives}}
    instead makes the comparison derived, so it cannot drift again, and
    an axis that gets renamed fails the build instead of going quiet.

    That guard covered the lakeFS page only, so the flint pages kept hand-copied
    numbers and drifted exactly the same way: the Consistency note said
    both direct modes sat at the 1.5 durability floor after Lean was
    rescored to 2.5, and the Security note said FUSE won tenancy when
    Lean had passed it. Neither could fail a build that never read them.
    {{score:Chart/Axis/Column}} is the flint-to-flint form — same
    contract, any column — and the chart notes and the deck foot now go
    through here too.
    """
    def one(m):
        kind, chart, rest = m.group(1), m.group(2).strip(), m.group(3).strip()
        column = None
        if kind == "score":
            axis, _, column = rest.rpartition("/")
            axis, column = axis.strip(), column.strip()
            if not axis:
                raise SystemExit(
                    f"{{{{score:...}}}} needs Chart/Axis/Column, got {rest!r}")
        else:
            axis = rest
        ch = next((c for c in data["charts"] if c["title"] == chart), None)
        if ch is None:
            raise SystemExit(f"cross-reference to unknown chart {chart!r}")
        i = next((j for j, a in enumerate(ch["axes"])
                  if axis in (a["short"], a["name"])), None)
        if i is None:
            raise SystemExit(f"cross-reference to unknown axis {chart}/{axis!r}")
        if kind == "score":
            if column not in ch["scores"]:
                raise SystemExit(
                    f"cross-reference to unknown column {chart}/{column!r}")
            return fmt(ch["scores"][column][i])
        if not lk:
            raise SystemExit(
                f"{{{{{kind}:...}}}} compares against lakeFS, which is not loaded")
        lean, lake = fmt(ch["scores"]["Lean"][i]), fmt(lk["scores"][chart][i])
        return {"cmp": f"{lake} vs {lean}",
                "cmpL": f"{lake} vs Lean {lean}",
                "lean": lean,
                "lake": lake}[kind]
    return CMP_RE.sub(one, text)

def card(ch, data, lk, page):
    """One page-1 card: radar + score table + a pointer to the chart's page.

    The chart's note used to sit under the table. It never fit: measured
    in headless Chrome, every card on the committed page 1 was 233px
    tall with 284-351px of content, so the Consistency and Performance
    notes were cut mid-sentence by the card below and the Security and
    Day-2 notes ran into the foot. Nothing warned -- .card has no
    overflow rule and the next card's opaque background hid the tail.
    The note is now the lede of the chart's own rationale page (the
    page-count axis, as with the rationale columns), and the card keeps
    what fits: the picture and the numbers.
    """
    shorts = ch["series"]
    legend = "".join(
        f'<span class="lg"><span class="sw" style="background:{SERIES_STYLE[s][1]}"></span>'
        f'{SERIES_STYLE[s][0]}</span>' for s in shorts)
    svg_series = "\n".join(
        series_svg(ch["scores"][s], SERIES_STYLE[s][1], SERIES_STYLE[s][2], len(shorts))
        for s in shorts)
    head = "<th></th>" + "".join(
        f'<th><span class="sw" style="background:{SERIES_STYLE[s][1]}"></span>{s}</th>'
        for s in shorts)
    rows = "".join(
        f'<tr><td>{ax["short"]}</td>' +
        "".join(f'<td>{fmt(ch["scores"][s][i])}</td>' for s in shorts) + "</tr>"
        for i, ax in enumerate(ch["axes"]))
    return f'''<div class="card">
  <div class="cardhead">
    <h2>{ch["title"]}</h2>
    <div class="legend">{legend}</div>
  </div>
  <p class="sub">{ch["sub"]}</p>
  <div class="cardbody">
    <svg viewBox="0 0 {W} {H}" role="img" aria-label="{ch["title"]} radar">
      {rings(len(ch["axes"]))}
      {axes_svg([a["short"] for a in ch["axes"]])}
      {svg_series}
    </svg>
    <div class="side">
      <table>
        <thead><tr>{head}</tr></thead>
        <tbody>{rows}</tbody>
      </table>
      <p class="note">The reading of this chart, and the reason behind every number: page {page}.</p>
    </div>
  </div>
</div>'''

def rat_section(ch, lo, hi, head, data, lk):
    """One rationale column: axes [lo, hi) of `ch`.

    A chart's rationales used to be ONE column, two charts to a page. All
    four columns overflowed the fixed 8.5in page and the surplus was
    silently clipped -- 11 cells, the last axis of every chart, including
    the 935-character Conflicts cell that renders 7 characters. Nothing
    warned: the .page block has a fixed height and no overflow rule, so
    the text simply stopped. A chart now gets a whole page and splits its
    axes across two columns of the SAME width as before, which fixes it
    on the page-count axis instead of the type-size axis: font, leading
    and gaps are untouched.
    """
    shorts = ch["series"]
    blocks = []
    for i, ax in list(enumerate(ch["axes"]))[lo:hi]:
        rows = "".join(
            f'<p class="rr"><span class="sw" style="background:{SERIES_STYLE[s][1]}"></span>'
            f'<b>{s} {fmt(ch["scores"][s][i])}</b> — {resolve_refs(ch["rationales"][s][i], data, lk)}</p>'
            for s in shorts)
        defn = ax.get("definition", "")
        blocks.append(f'<div class="axb"><p class="axt"><b>{ax["name"]}</b>'
                      f'{" — " + defn if defn else ""}</p>{rows}</div>')
    # The continuation column keeps the rule but not the words, so the
    # two halves stay optically level.
    h = ch["title"] if head else "&nbsp;"
    return f'<div class="ratcol"><h2 class="rath">{h}</h2>' + "".join(blocks) + "</div>"

S_GOOD, S_WARN, S_SER, S_CRIT = "#0ca30c", "#fab219", "#ec835a", "#d03b3b"

AVAIL_COLS = [("NFS-over-S3 (hub — both flavors)", "#184f95"),
              ("FUSE", "#eb6834"), ("Lean", "#1baf7a"), ("Forge", "#b03060")]

AVAIL_DEPS = [
    "The hub pod, its PVC, and the network path to :2049 — for every read and every write.",
    "The in-pod FUSE daemon for every I/O; S3 for non-resident reads and all publishes.",
    "Nothing standing — the app reads and writes plain local files; S3 / gateway are needed only at checkout and publish.",
    "The door and the repository&rsquo;s server pod for every clone, fetch and push; S3 on every push (the ack) and at restore. The working tree is plain local files.",
]

AVAIL_ROWS = [
    ("Serving process crashes (hub pod / FUSE daemon / sync sidecar)", [
        (S_WARN, "Brief stall", "restart + epoch re-claim &asymp; 13 s; clients park and resume — nothing durable at risk (PVC)."),
        (S_SER, "Visible errors", "ENOTCONN — an error apps can see; restart-in-place recovers the emptyDir dirty set; open fds stay severed, apps reopen."),
        (S_GOOD, "App unaffected", "plain files — the app never notices; the restarted sidecar reloads its baseline, rescans, self-recognizes its lease; publishes pause one beat."),
        (S_WARN, "Pushes in flight fail", "the syncer is the pod&rsquo;s main container: every push in the batch fails at the client, none acknowledged; the pod restarts and restores from the snapshot (sub-second on loopback); clones during it fail fast; the agent&rsquo;s tree is untouched."),
    ]),
    ("Node hosting it dies (or spot-reclaims uncleanly)", [
        (S_CRIT, "Share-wide hang", "every client hard-hangs in D-state (SIGKILL cannot touch it); Recreate + RWO &asymp; 6 min force-detach; F29&rsquo;s manual arm. Acked data is safe on the PVC."),
        (S_SER, "Per-pod loss", "that pod&rsquo;s unflushed set is gone (hot files: unbounded) plus a 60–110 s subtree write-lockout for the successor; every other pod unaffected."),
        (S_SER, "Per-pod loss", "a hard crash drains nothing — loss = RPO exactly; the successor waits out the same lease lockout; every other pod unaffected."),
        (S_WARN, "Restore elsewhere", "no volume to detach: a replacement waits out the lease&rsquo;s quiet polls (&asymp;60 s after an unclean death), rotates the snapshot, restores from S3, serves. Nothing acknowledged is lost; unpushed work on the dead node is gone — git&rsquo;s contract."),
    ]),
    ("S3 outage or throttle", [
        (S_WARN, "Serving unaffected", "reads and writes continue from the PVC; the RPO grows; one coordinated retry queue drains the backlog."),
        (S_SER, "Degraded + compound risk", "non-resident reads fail; N uncoordinated engines re-fetch N&times;; outage + spot reclaim compound — that pod&rsquo;s dirty set is gone."),
        (S_WARN, "Running pods unaffected", "local files keep serving and publishes pause — but new pods cannot check out until S3 returns."),
        (S_SER, "Read-only, then fenced", "pushes get <code>ng</code> — the syncer refuses to acknowledge what it cannot store; clones and fetches keep working until the lease TTL, then the server self-fences and exits rather than serve refs it cannot prove it holds."),
    ]),
    ("Control plane down (operator / webhook / gateway + proxy)", [
        (S_WARN, "Serving unaffected", "running hubs keep serving; wake and reconcile are blocked — a suspended share cannot wake while no operator reconciles."),
        (S_SER, "New pods blocked", "failurePolicy: Fail — every matched pod CREATE blocks while the webhook is down; running pods unaffected (Ignore would be worse: a silent data black hole)."),
        (S_SER, "Four wedges — at the proxy", "proxy unreachable: publishes pause AND checkouts/restarts wedge AND sync is unavailable AND HITL writes fail loudly (chaos leg C12). Gateway down alone: barriers continue — the shipped sidecar writes its cells straight to the store (C8); running pods keep serving throughout."),
        (S_SER, "Door down = no git", "the door is in the path of every verb (N stateless replicas). Operator down: live servers keep serving, but a parked repository cannot wake — the door arms the annotation, only the operator scales — and the held request fails after 180 s."),
    ]),
    ("KDC / Kerberos infrastructure down — krb5p flavor only", [
        (S_SER, "New mounts fail", "new GSS contexts and credential renewals fail; established tickets ride out their lifetime; sec=sys is untouched — it carries no GSS context at all."),
        (None, "n/a", "no NFS wire."),
        (None, "n/a", "no NFS wire."),
        (None, "n/a", "no Kerberos. The analogue is the apiserver: a TokenReview outage refuses NEW sessions once the &le; 60 s cache expires; sessions in flight finish."),
    ]),
    ("Share idle / parked, then accessed", [
        (S_WARN, "Wake required", "an NFS mount against a scaled-to-zero hub hangs until something writes the wake annotation — the data path cannot; the file API answers 503 fast; wake &asymp; 41 s suspended, and 17 s hibernated once the epoch cell was released &mdash; ~80 s when it was not."),
        (S_GOOD, "Nothing to wake", "idle = Hibernated structurally; the price moves to every pod start being a cold start — hydration on touch."),
        (S_GOOD, "Nothing to wake", "idle = S3-only structurally; the price is a full checkout at pod start, budgeted by derived probes."),
        (S_WARN, "Wake required — held", "replicas 0 after <code>suspendAfterSecs</code>; the next request at the door arms the wake and is HELD up to 180 s (git does not retry a 503); restore = one pack at single-stream rate (4–8 s/GB at EC2 rates) plus &asymp;60 s of lease wait if the last death was unclean."),
    ]),
]

def p5_card(ch, lscores, lnote):
    axes = ch["axes"]
    legend = "".join(
        f'<span class="lg"><span class="sw" style="background:{SERIES_STYLE[s][1]}"></span>'
        f'{SERIES_STYLE[s][0]}</span>' for s in ("Lean", "lakeFS"))
    svg_series = (series_svg(ch["scores"]["Lean"], *SERIES_STYLE["Lean"][1:], 2) + "\n" +
                  series_svg(lscores, *SERIES_STYLE["lakeFS"][1:], 2))
    shorts = ch["series"] + ["lakeFS"]
    head = "<th></th>" + "".join(
        f'<th><span class="sw" style="background:{SERIES_STYLE[s][1]}"></span>{s}</th>'
        for s in shorts)
    rows = ""
    for i, ax in enumerate(axes):
        vals = [ch["scores"][s][i] for s in ch["series"]] + [lscores[i]]
        rows += (f'<tr><td>{ax["short"]}</td>' +
                 "".join(f'<td>{fmt(v)}</td>' for v in vals) + "</tr>")
    return f"""<div class="card p5">
  <div class="cardhead">
    <h2>{ch["title"]}</h2>
    <div class="legend">{legend}</div>
  </div>
  <p class="sub">lakeFS overlaid on Lean — its nearest archetype; full scores in the table</p>
  <div class="cardbody">
    <svg viewBox="0 0 {W} {H}" role="img" aria-label="{ch["title"]}: lakeFS vs Lean radar">
      {rings(len(axes))}
      {axes_svg([a["short"] for a in axes])}
      {svg_series}
    </svg>
    <div class="side">
      <table>
        <thead><tr>{head}</tr></thead>
        <tbody>{rows}</tbody>
      </table>
      <p class="note">{lnote}</p>
    </div>
  </div>
</div>"""

def p5_page(data, lk):
    cards = "".join(p5_card(ch, lk["scores"][ch["title"]],
                            resolve_refs(lk["notes"][ch["title"]], data, lk))
                    for ch in data["charts"])
    heads = "".join(f'<p>{resolve_refs(h, data, lk)}</p>' for h in lk["headlines"])
    return f"""<div class="page">
  <h1>lakeFS — scored from source</h1>
  <p class="deck">{lk["meta"]["sub"]}</p>
  <div class="grid">{cards}</div>
  <div class="beyond" style="margin-top:0.08in">{heads}</div>
  <p class="foot">{resolve_refs(lk["foot"], data, lk)}</p>
</div>"""

def mc_page():
    mc_paras = """<div class="beyond">
  <p><b>Hub:</b> coherence survives trivially (one authority) — the contract is network identity. Remote
  clusters SNAT to one address (1486/1486 measured; nfsClientCIDRs structurally cannot work), per-node NFS
  client names must be unique across clusters (RFC 8881 case 5 read a colliding name as a reboot and took the
  incumbent&rsquo;s locks — six 1.37.0 defects, one cause), and remote mounts need a keepalive or idle-suspend
  kills them. Prefix uniqueness stops at the cluster boundary — equal prefixes contend on the epoch cell,
  nested ones never meet: allocate prefixes upstream of Kubernetes.</p>
  <p><b>FUSE / Lean:</b> native — the bucket is the rendezvous and the arbitration state (epoch, claim, lease,
  manifest CAS, lean&rsquo;s gateway refusal) is store-side, so cross-cluster behaves exactly like cross-pod:
  no client identity on any wire, no keepalive, nothing to wake. The residual is the same allocation gap —
  per-cluster webhooks cannot see each other&rsquo;s subtree grants.</p>
  <p><b>lakeFS:</b> the best client story — HTTP + per-request SigV4 means the NFS identity class simply does
  not exist; N clusters are N sets of HTTP clients. The pivot moves to the database: every replica must share
  ONE KV (cross-region, a KV round-trip sits inside every commit CAS), and two independent installs on one
  namespace are fenced only at CREATE time (ensureStorageNamespace&rsquo;s dummy object, controller.go:2426) —
  a birth check, not a live lease; past it, each install&rsquo;s uncommitted GC treats the other&rsquo;s staged
  objects as garbage. In one line: the hub makes multicluster a <i>network-identity</i> problem, the direct
  modes an <i>allocation</i> problem, lakeFS a <i>database-topology</i> problem — share one KV or don&rsquo;t.</p>
  <p><b>Forge:</b> HTTP plus a projected token, so the NFS identity class does not exist here either — but the
  token is proven by THIS cluster&rsquo;s apiserver (<code>TokenReview</code>), and a pod in another cluster carries
  nothing the door can verify: cross-cluster agents fall back to the human bearer until the Knox chain lands.
  The bucket is a rendezvous for READERS only — a dumb-protocol clone works with the server down — never for a
  second server: one lease, one prefix, one syncer, and the fence turns a foreign cluster&rsquo;s server into a
  straggler that exits. Every clone leaves the server&rsquo;s NIC across the cluster boundary, so the storm lever
  (bundle URIs, served from S3) is the cross-cluster lever too. In one line: forge makes multicluster an
  <i>identity-provider</i> problem.</p>
  </div>
  """
    return f'''<div class="page">
  <h1>Multicluster — many clusters, one bucket</h1>
  <p class="deck">The radar scores on page 1 anchor to the single-cluster case — clients co-located with
  the share&rsquo;s cluster. This page states each approach&rsquo;s multicluster contract and lists the
  cells that move when clients span clusters. The direct modes and lakeFS are topology-invariant by
  construction; the hub column is where multicluster costs land.</p>
  {mc_paras}
  <h2 class="rath" style="margin-top:0.16in">What moves — score deltas (single &rarr; multicluster)</h2>
  <table class="avail">
    <thead><tr><th style="width:26%">Chart &middot; axis</th><th style="width:12%">Column</th>
      <th style="width:17%">single &rarr; multi</th><th>Why</th></tr></thead>
    <tbody>
      <tr><td class="ae">Performance &middot; Stream read / write</td><td class="ac">hub sys &middot; krb5p</td>
        <td class="ac">sys 3.5/3 &rarr; 3/2.5<br>krb5p 2/1.5 &rarr; 1.5/1</td>
        <td class="ac">cross-cluster RTT on every RPC; one NIC now shared by N clusters&rsquo; clients; often an inter-AZ/region billing path too.</td></tr>
      <tr><td class="ae">Performance &middot; Small files, Hot loops</td><td class="ac">hub sys &middot; krb5p</td>
        <td class="ac">sys 1.5/2 &rarr; 1/1.5<br>krb5p 1/1.5 &rarr; 0.5/1</td>
        <td class="ac">the metadata path is already round-trip-bound (create 12.5&times;, delete 222&times;); every RTT gets longer.</td></tr>
      <tr><td class="ae">Performance &middot; Small files</td><td class="ac">lakeFS</td>
        <td class="ac">2.5 &rarr; 2</td>
        <td class="ac">only when clusters are remote from the lakeFS service; and if replicas span regions, the KV round-trip sits inside every commit CAS.</td></tr>
      <tr><td class="ae">Day-2 &middot; Failure shape</td><td class="ac">hub (both)</td>
        <td class="ac">2 &rarr; 1.5</td>
        <td class="ac">a hub node death now D-state-hangs pods in every mounting cluster — the blast radius multiplies by topology.</td></tr>
      <tr><td class="ae">Day-2 &middot; Consumer footprint</td><td class="ac">hub sys</td>
        <td class="ac">4 &rarr; 3</td>
        <td class="ac">the cross-cluster operating contract lands on consumers: unique per-node client names, a keepalive, a routable advertiseAddress, peering/SGs. (krb5p stays 1 — already the floor.)</td></tr>
      <tr><td class="ae">Day-2 &middot; Idle lifecycle &amp; wake</td><td class="ac">hub (both)</td>
        <td class="ac">2.5 &rarr; 2</td>
        <td class="ac">idle-suspend kills a live remote mount (suspendWithSessions defaults to suspending anyway), and nothing in a remote cluster can write the wake annotation.</td></tr>
      <tr><td class="ae">Security &middot; Auth</td><td class="ac">Forge</td>
        <td class="ac">{{score:Security/Auth/Forge}} &rarr; 2</td>
        <td class="ac">TokenReview proves only this cluster&rsquo;s ServiceAccount tokens; a remote cluster&rsquo;s pods hold no credential the door accepts, so cross-cluster access is the static human bearer until Knox.</td></tr>
      <tr><td class="ae">Performance &middot; Cold start, Scale-out</td><td class="ac">Forge</td>
        <td class="ac">{{score:Performance/Cold start/Forge}}/{{score:Performance/Scale-out/Forge}} &rarr; 3.5/2.5</td>
        <td class="ac">every clone crosses the cluster boundary from one pod&rsquo;s NIC, often an inter-AZ/region billing path; bundle URIs (client opt-in) move those bytes to S3.</td></tr>
    </tbody>
  </table>
  <p class="atake"><b>Unchanged by design:</b> every Consistency cell (the hub&rsquo;s coherence is identical
  for remote clients; the direct modes&rsquo; contract is already per-subtree snapshots wherever pods run);
  every FUSE and Lean cell (cross-cluster is cross-pod — the bucket is the rendezvous); every Forge
  Consistency cell (the server arbitrates wherever the client is); krb5p&rsquo;s auth
  and wire cells (per-user identity survives any network — this is its selling point). And note the
  <b>sec=sys security cells are already scored at the multicluster boundary</b>: their evidence came from
  the cross-cluster drills (the 1486/1486 SNAT measurement, &ldquo;nfsClientCIDRs cannot express a client
  in another cluster&rdquo;), so they take no further delta here.</p>
  <p class="foot">Sources: the N-clusters-one-hub drill campaign and flint-lite-architecture p5 (the
  cross-cluster operating contract), the fleets guide performance tables (single-cluster, same-AZ anchor),
  and lakeFS controller.go:2426 (the create-time namespace check). Deltas are estimates from the measured
  single-cluster numbers plus the documented cross-cluster mechanics — not re-measured; treat them as the
  direction and rough size of the shift.</p>
</div>'''

def avail_page():
    heads = "".join(
        f'<th><span class="sw" style="background:{c}"></span>{n}</th>'
        for n, c in AVAIL_COLS)
    deps = "".join(f'<td class="ac">{d}</td>' for d in AVAIL_DEPS)
    rows = ""
    for event, cells in AVAIL_ROWS:
        tds = ""
        for sev, verdict, text in cells:
            dot = f'<span class="adot" style="background:{sev}"></span>' if sev else ""
            tds += f'<td class="ac">{dot}<b>{verdict}</b> — {text}</td>'
        rows += f'<tr><td class="ae">{event}</td>{tds}</tr>'
    return f'''<div class="page">
  <h1>Availability — what still works when a component dies</h1>
  <p class="deck">The radar charts price failure as a score; this matrix shows its shape. Verdicts:
  <span class="adot" style="background:{S_GOOD}"></span>unaffected &nbsp;
  <span class="adot" style="background:{S_WARN}"></span>degraded &nbsp;
  <span class="adot" style="background:{S_SER}"></span>impaired &nbsp;
  <span class="adot" style="background:{S_CRIT}"></span>outage.
  Both NFS flavors share one column — failure shape is identical; the KDC row exists only for krb5p.</p>
  <table class="avail">
    <thead><tr><th style="width:15%"></th>{heads}</tr></thead>
    <tbody>
      <tr><td class="ae">What the data path depends on</td>{deps}</tr>
      {rows}
    </tbody>
  </table>
  <p class="atake">The pattern the docs themselves draw: the hub <b>concentrates</b> failure into rare,
  share-wide, hang-shaped outages; the direct modes <b>distribute</b> it into frequent, per-pod,
  loss-shaped events — &ldquo;durability becomes an operational statistic — interruption handlers, grace
  periods, dirty-set caps — instead of an architecture property.&rdquo; Lean&rsquo;s refinement over FUSE is
  taking the app out of the sidecar&rsquo;s blast radius entirely; its residual concentration point is the
  gateway/proxy pair. Forge concentrates like the hub — one server per repository, in the path of every
  git verb — but fails like Lean: errors, never hangs, and a restore in place of a recovery.</p>
  <p class="foot">Sources: fuse-architecture p4 (failure matrix) &middot; lean plan §2.2/§7 (gateway outage) &middot; flint-lite-architecture p4 + fleets guide (idle/suspend) &middot; pnfs-operator-runbook (Kerberos) &middot; flint-forge-design §5, §11 (the syncer, the lease, the wake).</p>
</div>'''

def beyond_page(data, lk, p_lake):
    """Industry analogues, then the method and maturity record.

    Both used to live on page 1's foot and page 6's lower half. The foot
    was 9-12 lines of 7.5px grey text on the front page and the notes
    ran into it; page 6 overflowed by 86px once the availability matrix
    grew a fourth column. A page of their own costs nothing and lets the
    maturity record be read as paragraphs instead of one 2,300-character
    line.
    """
    foot = resolve_refs(data["foot"], data, lk)
    parts = re.split(r" (?:·|&middot;) ", foot.replace("shown separately: ", "shown separately. · "))
    method = "".join(f"<p>{x}</p>" for x in parts)
    return f'''<div class="page">
  <h1>Beyond flint — the same archetypes in the industry</h1>
  <div class="beyond">
  <p>These pages are grounded in flint, but the columns model archetypes, and most orderings are
  structural — set by where arbitration, bytes, and credentials live — so they survive across
  implementations: the hub column reads on any NFS-gateway-over-object product, FUSE on the
  bucket-native class, Lean on any checkout-and-sync pattern. The measured decimals (the 12.5&times;/222&times;
  metadata cliff, hydrate and checkout rates) and the hardening state are flint&rsquo;s own. Where other
  implementations genuinely move numbers:</p>
  <p><b>The FUSE column is the serverless, bucket-native class.</b> Metadata-server FUSE filesystems
  (JuiceFS, Alluxio, CephFS-FUSE) are a different animal: a coherence authority returns, so on the
  Consistency chart they land near the NFS polygon (real locks, close-to-open) while keeping FUSE&rsquo;s
  privilege posture, per-pod failure shape, and Day-2 costs.</p>
  <p><b>fsync semantics are a dial, not a constant.</b> Mountpoint for S3 completes the upload on
  fsync/close — &ldquo;Ack survives&rdquo; rises toward 4 while streaming-write and hot-loop scores fall;
  flint&rsquo;s emptyDir ack is the write-back end of the same dial, chosen for latency.</p>
  <p><b>Lean&rsquo;s CAS and conflict scores are the well-engineered end of sync.</b> A plain
  aws-s3-sync-style loop does silent last-writer-wins (conflict surfacing &asymp; 1, CAS &asymp; 0), and most
  bucket-native FUSE clients send no conditional writes at all. The sec=sys and krb5p rows, by
  contrast, are textbook industry NFS facts and transfer unchanged.</p>
  <p><b>lakeFS is the closest whole-system analogue</b> — the same doctrine of one bucket format,
  several front ends: its S3 gateway is lean&rsquo;s grade-1 proxy, its presigned direct mode is the
  grade-2 lean deferred, lakectl local is checkout-and-sync, and lakeFS Mount (&ldquo;Everest&rdquo;,
  enterprise; NFS v3 or FUSE, write support since 0.2.0, a CSI driver in preview) adds a mount front
  end — though its NFS is local snapshot delivery, not a coherence authority. The contrasts that
  matter: visibility is explicit-commit, not cadence (&ldquo;other users or mounts will not see your
  changes until those changes are committed&rdquo;); write-mode POSIX is narrower than plain files (no
  rename, links, or locks — rename alone disqualifies git loops in-mount) and same-branch concurrent
  mounts resolve <i>source-wins</i> — a silent winner. Against that, its server-side arbitration —
  atomic branch commits, first-class merge conflicts, per-user RBAC, clients holding no bucket
  credentials — is roughly lean&rsquo;s designed endgame already shipped, at the price flint-lean
  refuses to pay: an operated KV store (Postgres/DynamoDB) standing beside the bucket. Page {p_lake} scores it from source.</p>
  </div>
  <h2 class="rath" style="margin-top:0.2in">How these numbers were made — method, evidence, maturity</h2>
  <div class="beyond">{method}</div>
</div>'''

def main():
    here = os.path.dirname(os.path.abspath(__file__))
    data = json.load(open(os.path.join(here, "radar_data.json")))
    lk_path = os.path.join(here, "lakefs_data.json")
    lk = json.load(open(lk_path)) if os.path.exists(lk_path) else {}
    ch = data["charts"]
    # A chart's rationales take one page unless the data says otherwise
    # ("rationalePages": 2). Security grew a seventh axis on 2026-09-05
    # with 53 px and 14 px free in its two columns; the fix stays on the
    # page-count axis, as rat_section's history says, not the type-size
    # axis. Page numbers are derived from the same list so the chart
    # cards and the navigation sentence cannot drift from it.
    n_pages = [int(c.get("rationalePages", 1)) for c in ch]
    first_page = {}
    p = 2
    for c, n in zip(ch, n_pages):
        first_page[c["title"]] = p
        p += n
    cards = "".join(card(c, data, lk, first_page[c["title"]]) for c in ch)
    rat_pages = ""
    for c, n in zip(ch, n_pages):
        n_ax = len(c["axes"])
        per_page = -(-n_ax // n)
        for k in range(n):
            lo, hi = k * per_page, min(n_ax, (k + 1) * per_page)
            half = lo + (hi - lo + 1) // 2
            cols = (rat_section(c, lo, half, True, data, lk) +
                    rat_section(c, half, hi, False, data, lk))
            cont = " (continued)" if k else ""
            lede = (f'<p class="lede">{resolve_refs(c["note"], data, lk)}</p>'
                    if k == 0 else "")
            rat_pages += (f'<div class="page"><h1>Why these numbers — {c["title"]}{cont}</h1>'
                          f'{lede}<div class="ratrow">{cols}</div></div>')
    lakefs_pg = p5_page(data, lk) if lk else ""
    # Derived, not counted by hand: the rationale section grew from two
    # pages to one per chart, and a navigation sentence that names page
    # numbers is the same hand-copied cross-reference resolve_refs exists
    # to kill.
    p_rat_first = 2
    p_rat_last = p - 1
    p_avail = p_rat_last + 1
    p_beyond = p_avail + 1
    p_lake = p_beyond + 1 if lk else None
    p_mc = (p_lake or p_beyond) + 1
    rat_ref = (f"Pages {p_rat_first}&ndash;{p_rat_last}"
               if p_rat_last > p_rat_first else f"Page {p_rat_first}")
    lake_ref = f"; page {p_lake} scores lakeFS from its source" if lk else ""
    html = f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>Flint front ends — radar comparison</title>
<style>
  @page {{ size: 11in 8.5in; margin: 0; }}
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  html, body {{ width: 11in; }}
  body {{
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    background: {PAGE}; color: {INK};
  }}
  .page {{
    width: 11in; height: 8.5in; padding: 0.26in 0.36in 0.2in;
    display: flex; flex-direction: column; overflow: hidden;
    page-break-after: always; background: {PAGE};
  }}
  .page:last-child {{ page-break-after: auto; }}
  .ratrow {{ display: flex; gap: 0.3in; margin-top: 0.08in; flex: 1; min-height: 0; }}
  .lede {{ font-size: 7.6px; color: {INK2}; line-height: 1.38; margin-top: 4px; }}
  .ratcol {{ flex: 1; min-width: 0; }}
  .rath {{ font-size: 12.5px; font-weight: 650; border-bottom: 1px solid {GRID};
          padding-bottom: 2px; margin-bottom: 4px; }}
  .axb {{ margin-bottom: 4px; }}
  .axt {{ font-size: 8.1px; color: {INK}; margin-bottom: 1px; }}
  .rr {{ font-size: 7.4px; color: {INK2}; line-height: 1.3; margin: 1px 0 1px 2px; }}
  h1 {{ font-size: 18px; font-weight: 650; letter-spacing: -0.01em; }}
  .deck {{ font-size: 10px; color: {INK2}; margin-top: 1px; }}
  .intro {{ display: flex; gap: 0.18in; margin-top: 0.08in; }}
  .ib h3 {{ font-size: 9.5px; font-weight: 650; margin-bottom: 1.5px; }}
  .ib .ibt {{ font-weight: 400; color: {MUTED}; font-size: 8px; white-space: nowrap; }}
  .ib p {{ font-size: 7.9px; color: {INK2}; line-height: 1.42; }}
  .arch {{ color: {MUTED}; }}
  .ib {{ flex: 1; min-width: 0; }}
  .badge {{ font-size: 6.8px; font-weight: 600; color: {MUTED}; border: 1px solid {GRID};
           border-radius: 6px; padding: 0.5px 4px; margin-left: 5px; vertical-align: 1px;
           white-space: nowrap; }}
  .grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 0.14in;
          margin-top: 0.1in; flex: 1; min-height: 0; }}
  .card {{
    background: {SURFACE}; border: 1px solid rgba(11,11,11,0.10);
    border-radius: 9px; padding: 9px 12px 8px;
    display: flex; flex-direction: column; min-height: 0;
  }}
  .cardhead {{ display: flex; justify-content: space-between; align-items: baseline; gap: 8px; flex-wrap: nowrap; }}
  .card h2 {{ font-size: 12.5px; font-weight: 650; display: inline; }}
  .card .sub {{ font-size: 8px; color: {MUTED}; margin-top: 1px; }}
  .legend {{ font-size: 8px; color: {INK2}; white-space: nowrap; }}
  .lg {{ margin-left: 8px; white-space: nowrap; }}
  .sw {{ display: inline-block; width: 7px; height: 7px; border-radius: 2px;
        margin-right: 3px; vertical-align: -0.5px; }}
  .cardbody {{ display: flex; gap: 10px; align-items: flex-start; flex: 1; min-height: 0; }}
  .cardbody svg {{ align-self: center; }}
  /* 45%, not 47%: with Forge the page-1 tables are six columns of nowrap
     headers and at 47% three of them overran the side by 1-7px. */
  .cardbody svg {{ width: 45%; flex: none; }}
  /* The lakeFS page's tables carry every column — seven with Forge — and a
     nowrap header cannot shrink: at 47% the Security card clipped its
     last header. Measured, not assumed (measure.py). */
  .p5 .cardbody svg {{ width: 42%; }}
  .p5 th .sw {{ display: none; }}
  .side {{ flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 5px; }}
  table {{ width: 100%; border-collapse: collapse; font-size: 8.2px; color: {INK2}; }}
  th {{ font-weight: 600; color: {MUTED}; text-align: right; padding: 1.5px 3px;
       border-bottom: 1px solid {GRID}; font-size: 7.8px; white-space: nowrap; }}
  th:first-child {{ text-align: left; }}
  td {{ padding: 1.5px 3px; text-align: right; border-bottom: 1px solid {GRID};
       font-variant-numeric: tabular-nums; }}
  td:first-child {{ text-align: left; white-space: nowrap; }}
  tbody tr:last-child td {{ border-bottom: none; }}
  .note {{ font-size: 7.5px; color: {INK2}; line-height: 1.4; }}
  table.avail {{ margin-top: 0.12in; table-layout: fixed; }}
  table.avail th {{ text-align: left; font-size: 9px; padding: 3px 6px; }}
  table.avail td {{ text-align: left; vertical-align: top; padding: 3.5px 6px;
                   font-size: 8px; line-height: 1.38; }}
  table.avail td:first-child {{ white-space: normal; }}
  td.ae {{ font-weight: 650; color: {INK}; font-size: 8.4px; }}
  td.ac {{ color: {INK2}; }}
  .adot {{ display: inline-block; width: 7px; height: 7px; border-radius: 50%;
          margin-right: 4px; vertical-align: -0.5px; }}
  .beyond p {{ font-size: 8.2px; color: {INK2}; line-height: 1.42; margin-top: 3px;
             max-width: 9.6in; }}
  .atake {{ font-size: 8.4px; color: {INK2}; line-height: 1.45; margin-top: 0.08in;
           max-width: 8.6in; }}
  .foot {{ margin-top: 0.08in; font-size: 7.5px; color: {MUTED}; line-height: 1.45;
          border-top: 1px solid {GRID}; padding-top: 4px; }}
</style></head>
<body>
  <div class="page">
  <h1>Flint front ends — consistency, performance, security, Day-2 operations</h1>
  <p class="deck">Four ways to put one S3 bucket behind agent workloads, compared on four dimensions —
  six factors each (Performance and Security carry a seventh: the workload envelope, and whether one project can hold a read-only and a read-write session at once), scored 0 (weakest) – 5 (strongest) from the architecture docs and the code, adversarially verified
  per approach. {rat_ref} give each chart&rsquo;s reading and the reason behind every number; page {p_avail} maps availability; page {p_beyond} places the columns among industry implementations and records the method and maturity behind the scores{lake_ref}; page {p_mc} states the multicluster-to-one-bucket contract. NFS-over-S3 is charted in both flavors
  (sec=sys dashed).</p>
  {INTRO}
  <div class="grid">{cards}</div>
  <p class="foot">Scores 0 (weakest) – 5 (strongest), as designed, with maturity kept out of the number: one proposer per dimension and one adversarial verifier per column for every column but Forge, whose numbers were scored in a single pass on 2026-09-04 and then checked by one verifier per chart and two refuters per proposed change (four proposed, none survived); its prose remains single-pass. The full method, evidence and maturity record: page {p_beyond}.</p>
  </div>
  {rat_pages}
  {avail_page()}
  {beyond_page(data, lk, p_lake)}
  {lakefs_pg}
  {resolve_refs(mc_page(), data, lk)}
</body></html>'''
    # Repo-relative: this script lives at docs/radar/ and writes the
    # committed page one level up. It was an absolute path to somebody's
    # home directory while it lived in a scratchpad, which is part of why
    # it was nearly lost.
    out = os.path.normpath(os.path.join(here, "..", "flint-approach-radar.html"))
    with open(out, "w") as f:
        f.write(html)
    print("wrote", out)

if __name__ == "__main__":
    main()
