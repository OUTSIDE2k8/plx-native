#!/usr/bin/env python3
"""Write the Homebrew Channel repository for the Jellyfin edition's rolling release.

    tools/jellyfin-repo.py <built .ipk> <release download base URL> <out dir>

Writes `<out>/<app id>.manifest.json` (the package manifest Homebrew Channel installs from) and
`<out>/repo.json` (the index a user adds under Settings → Add repository). Both name assets of the
same release, so the three files are uploaded together by `.github/workflows/jellyfin-ipk.yml`.

The package's id, version and title are read from the staged flavour appinfo the build wrote
(`pkg/.flavor/<flavour>/appinfo.json`), never retyped here, so the manifest cannot describe a
different package than the one beside it; the sha256 is computed from the .ipk itself, because
Homebrew Channel checks it on the television and a mismatch fails the install there.
"""

import glob
import hashlib
import json
import pathlib
import sys

IPK_ASSET = "plxnative-jellyfin.ipk"


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    ipk, base, out = pathlib.Path(sys.argv[1]), sys.argv[2].rstrip("/"), pathlib.Path(sys.argv[3])
    infos = sorted(glob.glob("pkg/.flavor/*/appinfo.json"))
    if not infos:
        sys.exit("no staged flavour appinfo under pkg/.flavor — run `make ipk` first")
    app = json.loads(pathlib.Path(infos[0]).read_text())
    sha = hashlib.sha256(ipk.read_bytes()).hexdigest()
    out.mkdir(parents=True, exist_ok=True)
    icon = "https://raw.githubusercontent.com/GLinnik21/plx-native/main/pkg/nightly/largeIcon.png"
    manifest_name = f"{app['id']}.manifest.json"
    manifest = {
        "id": app["id"],
        "version": app["version"],
        "type": "native",
        "title": app["title"],
        "appDescription": "Native Jellyfin client for LG webOS (PlxNative with a Jellyfin bridge)",
        "iconUri": icon,
        "sourceUrl": "https://github.com/GLinnik21/plx-native",
        "rootRequired": False,
        "ipkUrl": f"{base}/{IPK_ASSET}",
        "ipkHash": {"sha256": sha},
    }
    (out / manifest_name).write_text(json.dumps(manifest, indent=2) + "\n")
    repo = {"packages": [{
        "id": app["id"],
        "title": app["title"],
        "shortDescription": "Native Jellyfin client — 60 fps UI, direct play, Dolby Vision",
        "iconUri": icon,
        "manifestUrl": f"{base}/{manifest_name}",
    }]}
    (out / "repo.json").write_text(json.dumps(repo, indent=2) + "\n")
    print(f"{app['id']} {app['version']} sha256={sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
