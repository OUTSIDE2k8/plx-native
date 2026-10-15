#!/usr/bin/env python3
"""A SYNTHETIC Jellyfin server — enough of the REST surface for the app's Jellyfin bridge
(`rust-modules/jellyfin`) to sign in, browse, search, open details, play and report progress, with
no household data in it.

Like `mock_pms.py`, every title and name is drawn from one closed alphabet (`s` + eight hex
digits) so a capture can be committed. It answers only what the bridge asks (`routes.rs` names
every call), in the shapes Jellyfin 10.10 returns, and logs anything else to stderr.

Sign-in: user `test`, password `test`; Quick Connect codes are approved on the `--approve-after`th
poll (default 2). Discovery: answers `Who is JellyfinServer?` on UDP `--discovery-port`.

    python3 tests/mock_jellyfin.py --port 8096
"""

import argparse
import hashlib
import json
import random
import socket
import sys
import threading
import time
import urllib.parse
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SERVER_ID = "0f0e0d0c0b0a09080706050403020100"
USER_ID = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"
TOKEN = "mocktoken0123456789abcdef01234567"
MOVIES_VIEW = "1111aaaa1111aaaa1111aaaa1111aaaa"
SHOWS_VIEW = "2222bbbb2222bbbb2222bbbb2222bbbb"
TICKS = 10_000_000


def gid(*parts):
    return hashlib.md5("/".join(map(str, parts)).encode()).hexdigest()


def sname(rng):
    return "s%08x" % rng.getrandbits(32)


def iso(epoch):
    return time.strftime("%Y-%m-%dT%H:%M:%S.0000000Z", time.gmtime(epoch))


class Library:
    def __init__(self, seed=1, movies=24, shows=4, seasons=2, episodes=6):
        rng = random.Random(seed)
        self.items = {}
        self.people = [{"Id": gid("person", i), "Name": sname(rng)} for i in range(12)]
        self.genres = [{"Id": gid("genre", i), "Name": sname(rng)} for i in range(5)]
        self.user = {}
        for n in range(movies):
            iid = gid("movie", n)
            self.items[iid] = self._video(rng, iid, "Movie", MOVIES_VIEW, n, rng.randrange(80, 160))
        for s in range(shows):
            sid = gid("show", s)
            show = self._base(rng, sid, "Series", SHOWS_VIEW, 1000 + s)
            show.update({"ChildCount": seasons, "RecursiveItemCount": seasons * episodes})
            self.items[sid] = show
            for k in range(1, seasons + 1):
                kid = gid("season", s, k)
                season = self._base(rng, kid, "Season", sid, 2000 + s * 10 + k)
                season.update({"IndexNumber": k, "SeriesId": sid, "SeriesName": show["Name"],
                               "SeriesPrimaryImageTag": "t" + sid[:6], "ChildCount": episodes,
                               "ParentId": sid})
                self.items[kid] = season
                for e in range(1, episodes + 1):
                    eid = gid("episode", s, k, e)
                    ep = self._video(rng, eid, "Episode", kid, 3000 + s * 100 + k * 10 + e,
                                     rng.randrange(40, 60))
                    ep.update({"IndexNumber": e, "ParentIndexNumber": k, "SeasonId": kid,
                               "SeasonName": season["Name"], "SeriesId": sid, "SeriesName": show["Name"],
                               "SeriesPrimaryImageTag": "t" + sid[:6], "ParentBackdropItemId": sid,
                               "ParentBackdropImageTags": ["b" + sid[:6]]})
                    self.items[eid] = ep
        # Something to continue: one movie half watched, one episode a quarter in.
        first_movie = gid("movie", 0)
        self.user[first_movie] = {"PlaybackPositionTicks": 30 * 60 * TICKS, "Played": False,
                                  "LastPlayedDate": iso(1_700_000_000)}
        self.user[gid("episode", 0, 1, 2)] = {"PlaybackPositionTicks": 10 * 60 * TICKS,
                                              "LastPlayedDate": iso(1_700_000_100)}
        self.user[gid("episode", 0, 1, 1)] = {"Played": True, "PlayCount": 1}

    def _base(self, rng, iid, kind, parent, n):
        return {"Id": iid, "Name": sname(rng), "SortName": sname(rng), "Type": kind, "ParentId": parent,
                "ServerId": SERVER_ID, "Overview": " ".join(sname(rng) for _ in range(20)),
                "ProductionYear": 1990 + rng.randrange(35), "PremiereDate": iso(700_000_000 + n * 86400),
                "DateCreated": iso(1_690_000_000 + n * 3600), "OfficialRating": "PG-13",
                "CommunityRating": round(5 + rng.random() * 4, 1), "CriticRating": rng.randrange(40, 99),
                "ImageTags": {"Primary": "p" + iid[:6]}, "BackdropImageTags": ["b" + iid[:6]],
                "GenreItems": rng.sample(self.genres, 2),
                "People": [dict(p, Type="Actor", Role=sname(rng), PrimaryImageTag="h" + p["Id"][:6])
                           for p in rng.sample(self.people, 4)]
                + [dict(rng.choice(self.people), Type="Director")]}

    def _video(self, rng, iid, kind, parent, n, minutes):
        it = self._base(rng, iid, kind, parent, n)
        ticks = minutes * 60 * TICKS
        src = gid("source", iid)
        it.update({"RunTimeTicks": ticks, "Taglines": [sname(rng)],
                   "Chapters": [{"StartPositionTicks": i * ticks // 4, "Name": sname(rng)} for i in range(4)],
                   "MediaSources": [{
                       "Id": src, "Path": f"/media/{sname(rng)}.mkv", "Container": "mkv",
                       "Size": 4_000_000_000, "Bitrate": 8_000_000, "RunTimeTicks": ticks,
                       "DefaultAudioStreamIndex": 1,
                       "MediaStreams": [
                           {"Type": "Video", "Index": 0, "Codec": "h264", "Width": 1920, "Height": 1080,
                            "Profile": "High", "RealFrameRate": 23.976, "BitDepth": 8,
                            "PixelFormat": "yuv420p", "DisplayTitle": "1080p H264 SDR"},
                           {"Type": "Audio", "Index": 1, "Codec": "ac3", "Channels": 6,
                            "Language": "spa", "IsDefault": True, "DisplayTitle": "Spa - Dolby Digital - 5.1"},
                           {"Type": "Audio", "Index": 2, "Codec": "aac", "Channels": 2,
                            "Language": "eng", "DisplayTitle": "Eng - AAC - Stereo"},
                           {"Type": "Subtitle", "Index": 3, "Codec": "subrip", "Language": "spa",
                            "DisplayTitle": "Spa - SUBRIP"},
                       ]}]})
        return it

    def view(self, it, detail=False):
        out = {k: v for k, v in it.items() if detail or k not in ("MediaSources", "Chapters", "People")}
        ud = dict(self.user.get(it["Id"], {}))
        ud.setdefault("PlaybackPositionTicks", 0)
        ud.setdefault("Played", False)
        ud.setdefault("PlayCount", 1 if ud["Played"] else 0)
        if it["Type"] in ("Series", "Season"):
            leaves = [x for x in self.descendants(it["Id"]) if x["Type"] == "Episode"]
            ud["UnplayedItemCount"] = sum(1 for x in leaves if not self.user.get(x["Id"], {}).get("Played"))
        out["UserData"] = ud
        return out

    def descendants(self, iid):
        kids = [x for x in self.items.values() if x.get("ParentId") == iid]
        return kids + [d for k in kids for d in self.descendants(k["Id"])]


class Mock:
    def __init__(self, lib, approve_after):
        self.lib = lib
        self.approve_after = approve_after
        self.qc = {}
        self.lock = threading.Lock()
        self.reports = []

    def views(self):
        return [{"Name": "Peliculas", "Id": MOVIES_VIEW, "CollectionType": "movies", "Type": "CollectionFolder",
                 "ImageTags": {"Primary": "v1"}},
                {"Name": "Series", "Id": SHOWS_VIEW, "CollectionType": "tvshows", "Type": "CollectionFolder",
                 "ImageTags": {"Primary": "v2"}},
                {"Name": "Musica", "Id": gid("music"), "CollectionType": "music", "Type": "CollectionFolder"}]

    def query_items(self, q):
        lib = self.lib
        detail = "MediaSources" in q.get("Fields", "")
        rows = list(lib.items.values())
        if "Ids" in q:
            wanted = q["Ids"].split(",")
            rows = [lib.items[i] for i in wanted if i in lib.items]
        if "ParentId" in q:
            pid = q["ParentId"]
            pool = lib.descendants(pid) if q.get("Recursive") == "true" else [x for x in rows if x.get("ParentId") == pid]
            if pid == MOVIES_VIEW:
                pool = [x for x in rows if x.get("ParentId") == MOVIES_VIEW]
            elif pid == SHOWS_VIEW:
                pool = [x for x in rows if x["Type"] == "Series" or x.get("SeriesId")] if q.get("Recursive") == "true" \
                    else [x for x in rows if x.get("ParentId") == SHOWS_VIEW]
            rows = pool
        if "IncludeItemTypes" in q:
            kinds = q["IncludeItemTypes"].split(",")
            rows = [x for x in rows if x["Type"] in kinds]
        if "searchTerm" in q:
            term = q["searchTerm"].lower()
            rows = [x for x in rows if term in x["Name"].lower()]
        if "PersonIds" in q:
            rows = [x for x in rows if any(p["Id"] == q["PersonIds"] for p in x.get("People", []))]
        if "GenreIds" in q:
            rows = [x for x in rows if any(g["Id"] == q["GenreIds"] for g in x.get("GenreItems", []))]
        filters = q.get("Filters", "")
        if "IsUnplayed" in filters:
            rows = [x for x in rows if not lib.user.get(x["Id"], {}).get("Played")]
        if "IsResumable" in filters:
            rows = [x for x in rows if lib.user.get(x["Id"], {}).get("PlaybackPositionTicks", 0) > 0]
        by = q.get("SortBy", "SortName").split(",")[0]
        key = {"SortName": lambda x: x["SortName"], "DateCreated": lambda x: x["DateCreated"],
               "PremiereDate": lambda x: x["PremiereDate"], "ProductionYear": lambda x: x["ProductionYear"],
               "CommunityRating": lambda x: x["CommunityRating"], "CriticRating": lambda x: x["CriticRating"],
               "IndexNumber": lambda x: (x.get("ParentIndexNumber", 0), x.get("IndexNumber", 0)),
               "ParentIndexNumber": lambda x: (x.get("ParentIndexNumber", 0), x.get("IndexNumber", 0)),
               "Random": lambda x: x["Id"]}.get(by, lambda x: x["SortName"])
        rows.sort(key=key, reverse=q.get("SortOrder") == "Descending")
        total = len(rows)
        start = int(q.get("StartIndex", 0))
        limit = int(q.get("Limit", total))
        rows = rows[start:start + limit]
        return {"Items": [lib.view(x, detail) for x in rows], "TotalRecordCount": total, "StartIndex": start}

    def handle(self, method, path, body):
        u = urllib.parse.urlsplit(path)
        p = u.path
        q = {k: v[-1] for k, v in urllib.parse.parse_qs(u.query, keep_blank_values=True).items()}
        segs = [s for s in p.split("/") if s]
        lib = self.lib
        j = lambda obj, status=200: (status, "application/json", json.dumps(obj).encode())

        if p == "/System/Info/Public":
            return j({"Id": SERVER_ID, "ServerName": "Mock Jellyfin", "Version": "10.10.7",
                      "ProductName": "Jellyfin Server", "StartupWizardCompleted": True})
        if p == "/Users/AuthenticateByName" and method == "POST":
            creds = json.loads(body or b"{}")
            if creds.get("Username") == "test" and creds.get("Pw") == "test":
                return j(self.session())
            return j({"error": "bad credentials"}, 401)
        if p == "/QuickConnect/Initiate" and method == "POST":
            code = "%06d" % random.randrange(1_000_000)
            secret = gid("qc", code, time.time())
            with self.lock:
                self.qc[secret] = 0
            return j({"Code": code, "Secret": secret, "Authenticated": False})
        if p == "/QuickConnect/Connect":
            with self.lock:
                if q.get("secret") not in self.qc:
                    return j({"error": "unknown secret"}, 404)
                self.qc[q["secret"]] += 1
                done = self.qc[q["secret"]] >= self.approve_after
            return j({"Authenticated": done, "Secret": q["secret"]})
        if p == "/Users/AuthenticateWithQuickConnect" and method == "POST":
            return j(self.session())

        # Everything below needs the token.
        if p.startswith("/Items/") and "/Images/" in p:
            return (200, "image/png", flat_png(int(q.get("fillWidth", 300) or 300),
                                               int(q.get("fillHeight", 450) or 450), colour(p)))
        if p == "/UserViews" or (len(segs) == 3 and segs[0] == "Users" and segs[2] == "Views"):
            return j({"Items": self.views(), "TotalRecordCount": 3})
        if p == "/Items" or (len(segs) == 3 and segs[0] == "Users" and segs[2] == "Items"):
            return j(self.query_items(q))
        if p in ("/UserItems/Resume",) or (len(segs) == 4 and segs[2:] == ["Items", "Resume"]):
            rows = [x for x in lib.items.values()
                    if lib.user.get(x["Id"], {}).get("PlaybackPositionTicks", 0) > 0
                    and (not q.get("ParentId") or x["Id"] in {d["Id"] for d in lib.descendants(q["ParentId"])}
                         or x.get("ParentId") == q.get("ParentId"))]
            rows.sort(key=lambda x: lib.user[x["Id"]].get("LastPlayedDate", ""), reverse=True)
            return j({"Items": [lib.view(x) for x in rows], "TotalRecordCount": len(rows)})
        if p == "/Shows/NextUp":
            out = []
            for show in [x for x in lib.items.values() if x["Type"] == "Series"]:
                if q.get("SeriesId") and show["Id"] != q["SeriesId"]:
                    continue
                eps = sorted((x for x in lib.items.values() if x.get("SeriesId") == show["Id"] and x["Type"] == "Episode"),
                             key=lambda x: (x["ParentIndexNumber"], x["IndexNumber"]))
                played = [i for i, e in enumerate(eps) if lib.user.get(e["Id"], {}).get("Played")]
                if played and played[-1] + 1 < len(eps):
                    out.append(lib.view(eps[played[-1] + 1]))
            return j({"Items": out, "TotalRecordCount": len(out)})
        if len(segs) == 3 and segs[0] == "Shows" and segs[2] == "Episodes":
            eps = sorted((x for x in lib.items.values() if x.get("SeriesId") == segs[1] and x["Type"] == "Episode"),
                         key=lambda x: (x["ParentIndexNumber"], x["IndexNumber"]))
            if q.get("StartItemId"):
                ids = [e["Id"] for e in eps]
                if q["StartItemId"] in ids:
                    eps = eps[ids.index(q["StartItemId"]):]
            eps = eps[:int(q.get("Limit", len(eps)))]
            return j({"Items": [lib.view(x) for x in eps], "TotalRecordCount": len(eps)})
        if len(segs) == 3 and segs[0] == "Items" and segs[2] == "Similar":
            it = lib.items.get(segs[1])
            rows = [x for x in lib.items.values() if it and x["Type"] == it["Type"] and x is not it][:int(q.get("Limit", 12))]
            return j({"Items": [lib.view(x) for x in rows], "TotalRecordCount": len(rows)})
        if p == "/Genres":
            return j({"Items": [dict(g, Type="Genre") for g in lib.genres], "TotalRecordCount": len(lib.genres)})
        if len(segs) == 2 and segs[0] == "MediaSegments":
            it = lib.items.get(segs[1])
            if it and it["Type"] == "Episode":
                end = it["RunTimeTicks"]
                return j({"Items": [{"Type": "Intro", "StartTicks": 30 * TICKS, "EndTicks": 90 * TICKS},
                                    {"Type": "Outro", "StartTicks": end - 60 * TICKS, "EndTicks": end}]})
            return j({"Items": []})
        if p.startswith("/Sessions/Playing") and method == "POST":
            report = json.loads(body or b"{}")
            with self.lock:
                self.reports.append((p, report))
                iid = report.get("ItemId")
                if iid in lib.items:
                    ud = lib.user.setdefault(iid, {})
                    ud["PlaybackPositionTicks"] = report.get("PositionTicks", 0)
                    ud["LastPlayedDate"] = iso(time.time())
            return (204, "text/plain", b"")
        if (segs[:1] == ["UserPlayedItems"] and len(segs) == 2) or (len(segs) == 4 and segs[2] == "PlayedItems"):
            iid = segs[-1]
            with self.lock:
                ud = lib.user.setdefault(iid, {})
                ud["Played"] = method == "POST"
                ud["PlaybackPositionTicks"] = 0
            return j(lib.view(lib.items[iid])["UserData"] if iid in lib.items else {})
        if (len(segs) == 3 and segs[0] == "UserItems" and segs[2] == "UserData") or (len(segs) == 5 and segs[4] == "UserData"):
            iid = segs[1] if segs[0] == "UserItems" else segs[3]
            data = json.loads(body or b"{}")
            with self.lock:
                lib.user.setdefault(iid, {}).update(data)
            return j({})
        if segs[:1] == ["Videos"]:
            if p.startswith("/Videos/ActiveEncodings"):
                return (204, "text/plain", b"")
            return (200, "video/x-matroska", b"\x1a\x45\xdf\xa3" + b"\x00" * 60)
        print(f"mock_jellyfin: UNKNOWN {method} {p}", file=sys.stderr, flush=True)
        return j({"error": "not found"}, 404)

    def session(self):
        return {"AccessToken": TOKEN, "ServerId": SERVER_ID,
                "User": {"Id": USER_ID, "Name": "test", "ServerId": SERVER_ID}}


def flat_png(w, h, rgb):
    w, h = max(1, min(w, 1920)), max(1, min(h, 1080))
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))

    def chunk(tag, data):
        return len(data).to_bytes(4, "big") + tag + data + zlib.crc32(tag + data).to_bytes(4, "big")
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", w.to_bytes(4, "big") + h.to_bytes(4, "big") + b"\x08\x02\x00\x00\x00")
            + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


def colour(path):
    d = hashlib.md5(path.split("?")[0].encode()).digest()
    return (60 + d[0] % 150, 60 + d[1] % 150, 60 + d[2] % 150)


def make_handler(mock):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):
            pass

        def _do(self):
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n) if n else b""
            public = self.path.split("?")[0] in ("/System/Info/Public", "/Users/AuthenticateByName",
                                                  "/QuickConnect/Initiate", "/QuickConnect/Connect",
                                                  "/Users/AuthenticateWithQuickConnect")
            auth = (self.headers.get("Authorization", "") + self.headers.get("X-Emby-Token", "")
                    + self.path)
            if not public and TOKEN not in auth:
                status, ctype, out = 401, "text/plain", b"unauthorized"
            else:
                status, ctype, out = mock.handle(self.command, self.path, body)
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(out)

        do_GET = do_POST = do_DELETE = do_PUT = do_HEAD = _do
    return Handler


def discovery_responder(port, http_port, host):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    while True:
        data, addr = s.recvfrom(256)
        if data.strip() == b"Who is JellyfinServer?":
            s.sendto(json.dumps({"Address": f"http://{host}:{http_port}", "Id": SERVER_ID,
                                 "Name": "Mock Jellyfin", "EndpointAddress": None}).encode(), addr)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--port", type=int, default=8096)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--approve-after", type=int, default=2)
    ap.add_argument("--discovery-port", type=int, default=0,
                    help="answer LAN discovery on this UDP port (7359 is Jellyfin's); 0 = off")
    a = ap.parse_args()
    mock = Mock(Library(seed=a.seed), a.approve_after)
    if a.discovery_port:
        threading.Thread(target=discovery_responder, args=(a.discovery_port, a.port, a.host),
                         daemon=True).start()
    srv = ThreadingHTTPServer((a.host, a.port), make_handler(mock))
    print(f"mock_jellyfin: http://{a.host}:{a.port}  user=test password=test", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
