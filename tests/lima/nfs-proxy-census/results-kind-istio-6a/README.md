# §6a Istio ambient checks — kind on the box, Istio 1.31.1, Gateway API v1.6.2

`step6a-istio.sh`; host kernel 6.12 mounts with `xprtsec=mtls` (ktls-utils 1.0 tlshd).

- `run-2.txt` — 17/17, all five questions answered (see `answers.txt`).
- `run-1-gwapi-1.3-route-never-attached.txt` — the first run installed the
  Gateway API v1.3.0 CRDs. istiod logged, and then ignored every TCPRoute:

      CRD tcproutes.gateway.networking.k8s.io version 1.3.0 is below minimum
      version 1.6.0 required by this Istio version; resources of this kind
      will not be processed.

  The listener had 0 attached routes, so the path-B mount timed out, and the
  rig printed "Q1: NO". That answer was the broken precondition's, not the
  bypass's. Run 2 checks `attachedRoutes == 1` before answering Q1. Q2–Q5
  gave the same answers in both runs.
