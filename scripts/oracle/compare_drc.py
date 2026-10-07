#!/usr/bin/env python3
"""Compare FluidEDA's in-context DRC markers with KLayout's (Phase 7.5).

  python3 scripts/oracle/compare_drc.py <ours.jsonl> <klayout.lyrdb>

ours: JSON lines {kind: width|space, layer, datatype, bbox: [x0, y0, x1, y1]}
(nm, the top cell's frame) from `drc_context --out`. KLayout: the report of
scripts/oracle/sky130_deck.drc (flat), whose items are edge pairs in µm.

Markers are matched per rule by overlap (2 nm slack): ours are one box per
connected violation region, KLayout's one edge pair per measured pair, so
the counts differ while the regions agree. Prints, per rule, how many of
each side the other covers, and exits non-zero if any marker is unmatched.
"""
import json, re, sys
import xml.etree.ElementTree as ET
from collections import defaultdict

NAMES = {
    (64, 20): 'nwell', (65, 20): 'diff', (65, 44): 'tap', (66, 20): 'poly', (66, 44): 'licon1',
    (67, 20): 'li1', (67, 44): 'mcon', (68, 20): 'met1', (68, 44): 'via', (69, 20): 'met2',
    (69, 44): 'via2', (70, 20): 'met3', (70, 44): 'via3', (71, 20): 'met4', (71, 44): 'via4',
    (72, 20): 'met5', (75, 20): 'hvi', (78, 44): 'hvtp', (93, 44): 'nsdm', (94, 20): 'psdm',
    (95, 20): 'npc', (125, 44): 'lvtn',
}
SLACK = 2

def ours(path):
    out = defaultdict(list)
    for line in open(path):
        if not line.strip():
            continue
        v = json.loads(line)
        name = NAMES.get((v['layer'], v['datatype']), f"{v['layer']}/{v['datatype']}")
        out[f"{name} {v['kind']}"].append(tuple(v['bbox']))
    return out

def theirs(path):
    root = ET.parse(path).getroot()
    out = defaultdict(list)
    for item in root.iter('item'):
        cat = (item.findtext('category') or '').strip().strip("'")
        for value in item.iter('value'):
            nums = [float(x) for x in re.findall(r'-?\d+(?:\.\d+)?(?:e-?\d+)?', value.text or '')]
            if len(nums) < 4:
                continue
            xs, ys = nums[0::2], nums[1::2]
            box = (round(min(xs) * 1000), round(min(ys) * 1000), round(max(xs) * 1000), round(max(ys) * 1000))
            out[cat].append(box)
    return out

def overlaps(a, b):
    return a[0] <= b[2] + SLACK and b[0] <= a[2] + SLACK and a[1] <= b[3] + SLACK and b[1] <= a[3] + SLACK

def main():
    a, b = ours(sys.argv[1]), theirs(sys.argv[2])
    bad = 0
    print(f"{'rule':16} {'ours':>6} {'covered':>8}   {'KLayout':>7} {'covered':>8}")
    for rule in sorted(set(a) | set(b)):
        mine, kl = a.get(rule, []), b.get(rule, [])
        mine_hit = sum(any(overlaps(m, k) for k in kl) for m in mine)
        kl_hit = sum(any(overlaps(k, m) for m in mine) for k in kl)
        bad += (len(mine) - mine_hit) + (len(kl) - kl_hit)
        print(f"{rule:16} {len(mine):6} {mine_hit:8}   {len(kl):7} {kl_hit:8}")
    total_a, total_b = sum(map(len, a.values())), sum(map(len, b.values()))
    print(f"{'all':16} {total_a:6} {'':8}   {total_b:7}")
    print('MATCH' if bad == 0 else f'{bad} unmatched marker(s)')
    sys.exit(0 if bad == 0 else 1)

main()
