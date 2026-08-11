#!/usr/bin/env python3
"""Host-side TCP fan-in for the ZOMBIE drill (block-rig.sh ZOMBIE=1).

lima VMs sit on ISOLATED user-mode networks (every VM is 192.168.5.15
on its own private subnet), so the zombie VM cannot dial the rig VM
directly. It CAN reach the macOS host (host.lima.internal), and lima
auto-forwards the rig VM's loopback listeners to host 127.0.0.1 — this
proxy closes the last gap: it listens on 0.0.0.0:<lport> (reachable
from the zombie VM) and forwards to 127.0.0.1:<rport> (lima's forward
into the rig VM). Pure TCP — NVMe/TCP, NFS, and gRPC all ride it
unmodified; throughput is irrelevant to the drill's assertions.

Usage: tcp-proxy.py LPORT:RPORT [LPORT:RPORT ...]
Prints "ready" once every listener is bound.
"""
import asyncio
import sys


async def pump(reader, writer):
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except OSError:
        pass
    finally:
        try:
            writer.close()
        except OSError:
            pass


def handler_for(rport):
    async def handle(client_r, client_w):
        try:
            up_r, up_w = await asyncio.open_connection("127.0.0.1", rport)
        except OSError:
            client_w.close()
            return
        await asyncio.gather(pump(client_r, up_w), pump(up_r, client_w))

    return handle


async def main():
    servers = []
    for spec in sys.argv[1:]:
        lport, rport = (int(x) for x in spec.split(":"))
        servers.append(
            await asyncio.start_server(handler_for(rport), "0.0.0.0", lport)
        )
    print("ready", flush=True)
    await asyncio.gather(*(s.serve_forever() for s in servers))


asyncio.run(main())
