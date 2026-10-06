"""Republish a real deployment for a bounded run: the same schema, mappings
and ABIs, fetched from The Graph's IPFS and pinned to the rig's, with every
data source's startBlock moved to START and an endBlock of END. Prints the
new manifest's hash. Usage: prepare.py HASH START END LOCAL_IPFS_API"""

import json
import re
import sys
import urllib.request

SOURCE = "https://ipfs.thegraph.com"


def post(url, data=None, headers=None):
    req = urllib.request.Request(url, data=data or b"", headers={"user-agent": "curl/8", **(headers or {})})
    return urllib.request.urlopen(req, timeout=120).read()


def cat(cid):
    return post(f"{SOURCE}/api/v0/cat?arg={cid}")


def add(api, name, content):
    boundary = "waxwing-corpus"
    body = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{name}"\r\n'
            "Content-Type: application/octet-stream\r\n\r\n").encode() + content + f"\r\n--{boundary}--\r\n".encode()
    out = post(f"{api}/api/v0/add?pin=true&cid-version=0", body,
               {"content-type": f"multipart/form-data; boundary={boundary}"})
    return json.loads(out)["Hash"]


def main():
    cid, start, end, api = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4].rstrip("/")
    manifest = cat(cid).decode()
    for linked in sorted(set(re.findall(r"/ipfs/(Qm[1-9A-HJ-NP-Za-km-z]{44}|baf[a-z2-7]{50,})", manifest))):
        pinned = add(api, linked, cat(linked))
        manifest = manifest.replace(f"/ipfs/{linked}", f"/ipfs/{pinned}")
    # endBlock needs specVersion 0.0.9; older manifests are raised to it.
    version = re.search(r"^specVersion:\s*['\"]?([\d.]+)", manifest, flags=re.M)
    if version and tuple(map(int, version.group(1).split("."))) < (0, 0, 9):
        print(f"specVersion {version.group(1)} raised to 0.0.9 for endBlock", file=sys.stderr)
        manifest = manifest[: version.start(1)] + "0.0.9" + manifest[version.end(1):]
    manifest = re.sub(r"^\s*endBlock:\s*\d+\s*\n", "", manifest, flags=re.M)
    # startBlock appears only on data sources; templates have none.
    manifest, count = re.subn(
        r"^(\s*)startBlock:\s*\d+\s*$",
        lambda m: f"{m.group(1)}startBlock: {start}\n{m.group(1)}endBlock: {end}",
        manifest,
        flags=re.M,
    )
    if count == 0:
        sys.exit("the manifest has no startBlock to move")
    print(add(api, "subgraph.yaml", manifest.encode()))


main()
