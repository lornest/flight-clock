#!/usr/bin/env python3
"""A stand-in flight feed with one of everything, to see how the radar draws
each kind of aircraft without waiting for them to fly past.

    scripts/demo-feed.py            # serves on http://localhost:8099
    FLIGHT_CLOCK_ADSB_URL=http://localhost:8099 FLIGHT_CLOCK_SCREEN=radar cargo run

Answers any /lat/<lat>/lon/<lon>/dist/<nm> request with the same made-up
aircraft, placed around whatever point was asked for: each size class
(category) twice on different headings, one with no category and one with
no track (both drawn as the plain square). /hex/<address> answers with that
one aircraft, as the details screen asks once an aircraft leaves the area.
"""
import json, math, re, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

# (callsign, category, type, bearing from centre, distance as a share of the
#  radius, track, altitude ft, speed kt)
AIRCRAFT = [
    ("LIGHT1", "A1", "C172", 20, 0.35, 45, 2500, 105),
    ("LIGHT2", "B1", "GLID", 200, 0.80, 300, 3200, 60),
    ("SMALL1", "A2", "CL35", 70, 0.60, 250, 14000, 310),
    ("SMALL2", "A6", "PC24", 250, 0.45, 110, 9000, 280),
    ("LARGE1", "A3", "A320", 120, 0.75, 300, 6000, 250),
    ("LARGE2", "A4", "B752", 300, 0.70, 135, 11000, 320),
    ("HEAVY1", "A5", "B77W", 160, 0.40, 0, 4000, 180),
    ("HEAVY2", "A5", "A388", 340, 0.85, 180, 34000, 480),
    ("HELI1", "A7", "EC35", 45, 0.85, 200, 1200, 95),
    ("HELI2", "A7", "R44", 225, 0.20, 20, 900, 80),
    ("NOCAT", None, "P28A", 95, 0.30, 330, 2000, 100),
    ("NOTRACK", "A3", "B738", 275, 0.88, None, 37000, 450),
    ("GROUND", "C2", "GND", 180, 0.65, 90, None, 12),
]


# The last area asked about, so /hex/ answers place aircraft the same way
LAST_AREA = [51.47, -0.4543, 6.0]


class Feed(BaseHTTPRequestHandler):
    def do_GET(self):
        m = re.match(r"/lat/(-?[\d.]+)/lon/(-?[\d.]+)/dist/([\d.]+)", self.path)
        one = re.match(r"/hex/([0-9a-f]{6})$", self.path)
        if m:
            LAST_AREA[:] = map(float, m.groups())
        elif not one:
            self.send_error(404)
            return
        lat, lon, radius = LAST_AREA
        ac = []
        for i, (callsign, category, kind, bearing, share, track, alt, speed) in enumerate(AIRCRAFT):
            nm = share * radius
            b = math.radians(bearing)
            a = {
                "hex": "dem%03x" % i,
                "flight": callsign,
                "t": kind,
                "lat": lat + nm * math.cos(b) / 60,
                "lon": lon + nm * math.sin(b) / (60 * math.cos(math.radians(lat))),
                "alt_baro": alt if alt is not None else "ground",
                "gs": speed,
            }
            if category:
                a["category"] = category
            if track is not None:
                a["track"] = track
            if one and a["hex"] != one.group(1):
                continue
            ac.append(a)
        body = json.dumps({"ac": ac, "resultCount": len(ac)}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
    print("Demo flight feed on http://localhost:%d" % port)
    HTTPServer(("127.0.0.1", port), Feed).serve_forever()
