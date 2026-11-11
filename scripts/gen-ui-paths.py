#!/usr/bin/env python3
"""Generate SVG path data for the static shapes in ui/*.slint.

Slint's software renderer can't rotate elements and Path has no dash
patterns, so tick rings, the launch screen's dotted arc and the rotated
plane are precomputed here and pasted into the .slint files as `commands`.
Display coordinates: 480x480, centre (240, 240). Prints JSON, one entry per
shape; the key names match where each is used:

  clock_minute_ticks, clock_hour_ticks  -> ui/clock.slint
  launch_ticks, launch_arc, launch_plane -> ui/splash.slint
  radar_ticks                            -> ui/radar.slint
  plane_up                               -> reference (rotated in src/ui_paths.rs)
"""
import math, json
C=240
def f(v): return f"{v:.1f}".rstrip('0').rstrip('.')
def ticks(n, r_out, r_in, skip=None):
    out=[]
    for i in range(n):
        if skip and skip(i): continue
        a=math.radians(i*360/n); s,c=math.sin(a),math.cos(a)
        out.append(f"M{f(C+r_out*s)} {f(C-r_out*c)}L{f(C+r_in*s)} {f(C-r_in*c)}")
    return "".join(out)
# Plane, pointing up, centred at 0,0 (from the design)
PLANE=[('M',[(0,-13)]),('C',[(1.6,-13),(2.2,-10),(2.2,-7)]),('L',[(2.2,-2)]),('L',[(13,4)]),('L',[(13,7)]),('L',[(2.2,3.5)]),
 ('L',[(2.2,9)]),('L',[(5.5,11.5)]),('L',[(5.5,13.5)]),('L',[(0,12)]),('L',[(-5.5,13.5)]),('L',[(-5.5,11.5)]),('L',[(-2.2,9)]),
 ('L',[(-2.2,3.5)]),('L',[(-13,7)]),('L',[(-13,4)]),('L',[(-2.2,-2)]),('L',[(-2.2,-7)]),('C',[(-2.2,-10),(-1.6,-13),(0,-13)]),('Z',[])]
def plane(cx,cy,deg,scale=1):
    t=math.radians(deg); s,c=math.sin(t),math.cos(t); out=[]
    for cmd,pts in PLANE:
        out.append(cmd+" ".join(f"{f(cx+scale*(x*c-y*s))} {f(cy+scale*(x*s+y*c))}" for x,y in pts))
    return "".join(out)
def dotted_arc(r, a0, a1, dash=2, gap=7):
    L=r*math.radians(a1-a0); out=[]; d=0
    while d < L:
        e=min(d+dash,L)
        p=[(C+r*math.sin(math.radians(a0)+x/r), C-r*math.cos(math.radians(a0)+x/r)) for x in (d,e)]
        out.append(f"M{f(p[0][0])} {f(p[0][1])}L{f(p[1][0])} {f(p[1][1])}"); d+=dash+gap
    return "".join(out)
paths={
 "clock_minute_ticks": ticks(60,223,213,lambda i:i%5==0),
 "clock_hour_ticks": ticks(12,223,205),
 "launch_ticks": ticks(12,226,212),
 "launch_arc": dotted_arc(200,0,60),
 "launch_plane": plane(413.2,140,150),
 "radar_ticks": ticks(60,230,222),
 "plane_up": plane(0,0,0),
}
print(json.dumps(paths,indent=1))
