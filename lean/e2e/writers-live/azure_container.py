#!/usr/bin/env python3
"""Create an Azure Blob container, signed with the account's Shared Key.

    azure_container.py <blob-endpoint> <account> <key-b64> <container>

For rigs against Azurite (`http://127.0.0.1:10000/<account>`) or an
account's own endpoint (`https://<account>.blob.core.windows.net`), where
the Azure CLI is not installed. A container that exists already is fine.
The signature is the Blob service's Shared Key, as flint-store's
`azure.rs` signs it; standard library only.
"""
import base64
import hashlib
import hmac
import sys
import urllib.error
import urllib.parse
import urllib.request
from email.utils import formatdate

API_VERSION = "2023-11-03"


def main():
    endpoint, account, key, container = sys.argv[1:5]
    path = urllib.parse.urlparse(endpoint).path.rstrip("/") + "/" + container
    date = formatdate(usegmt=True)
    # VERB, the eleven standard headers (a zero Content-Length is empty;
    # Content-Type is named, or urllib sends its form type unsigned), the
    # x-ms headers, then /account/path and the query.
    ctype = "application/octet-stream"
    std = ["", "", "", "", ctype, "", "", "", "", "", ""]
    sts = "PUT\n" + "".join(h + "\n" for h in std)
    sts += f"x-ms-date:{date}\nx-ms-version:{API_VERSION}\n/{account}{path}\nrestype:container"
    sig = base64.b64encode(hmac.new(base64.b64decode(key), sts.encode(), hashlib.sha256).digest()).decode()
    req = urllib.request.Request(
        endpoint.rstrip("/") + "/" + container + "?restype=container",
        data=b"",
        method="PUT",
        headers={
            "Content-Type": ctype,
            "x-ms-date": date,
            "x-ms-version": API_VERSION,
            "Authorization": f"SharedKey {account}:{sig}",
        },
    )
    try:
        urllib.request.urlopen(req, timeout=10)
        print(f"container {container}: created")
    except urllib.error.HTTPError as e:
        if e.code == 409:
            print(f"container {container}: exists")
            return 0
        print(f"container {container}: {e.code} {e.headers.get('x-ms-error-code', '')}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
