#!/bin/bash
# Screen pixels, structure snapshots, retained artifacts, and queue pixel limits.
. "$(dirname "$0")/lib.sh"
set -e
mkdir -p "$T/src"
printf '0\n' > "$T/src/version"
printf 'out/\n' > "$T/src/.gitignore"
cat > "$T/src/layr.toml" <<'TOML'
[probe.storefront-screens]
run = ['python3', 'screens.py', 'screens']
outputs = ['out/screens']
kind = 'screenshots'
fail_over = 0.05
[probe.quiet]
run = ['python3', 'screens.py', 'quiet']
outputs = ['out/quiet']
kind = 'screenshots'
[probe.strict]
run = ['python3', 'screens.py', 'strict']
outputs = ['out/strict']
kind = 'screenshots'
tolerance = 0
[probe.bad]
run = ['sh', '-c', 'mkdir -p out/bad; echo wrong > out/bad/Bad@Phone.png']
outputs = ['out/bad']
kind = 'screenshots'
TOML
cat > "$T/src/screens.py" <<'PY'
import json, pathlib, struct, sys, zlib
v = int(pathlib.Path('version').read_text())
kind = sys.argv[1]
out = pathlib.Path('out') / kind
out.mkdir(parents=True, exist_ok=True)
def png(name, w=100, h=100, boxes=(), delta=255):
    pixels = bytearray([0, 0, 0, 255] * w * h)
    for x, y, width, height in boxes:
        for yy in range(y, y + height):
            for xx in range(x, x + width):
                pixels[(yy * w + xx) * 4] = delta
    def chunk(tag, data):
        return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data))
    rows = b''.join(b'\0' + pixels[y*w*4:(y+1)*w*4] for y in range(h))
    data = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0))
    (out / (name + '.png')).write_bytes(data + chunk(b'IDAT', zlib.compress(rows, 0 if v else 9)) + chunk(b'IEND', b''))
if kind in ('quiet', 'strict'):
    png('Checkout@iPhone-15', boxes=[(0, 0, 10, 10)] if v else [], delta=8)
else:
    png('_same@1280')
    png('_checkout@1280', boxes=[(4, 4, 10, 10)] if v else [])
    png('_two@1280', boxes=[(1, 1, 5, 5), (70, 70, 5, 5)] if v else [])
    png('_near@1280', boxes=[(1, 1, 2, 2), (9, 1, 2, 2)] if v else [])
    png('_nine@1280', boxes=[(0, 0, 10, 10)] if v else [], delta=9)
    png('_many@1280', boxes=[(x, y, 1, 1) for x in range(0, 100, 20) for y in range(0, 100, 20)] if v else [])
    png('_size@1280', w=120 if v else 100)
    png('_added@1280' if v else '_removed@1280')
    (out / '_same@1280.html').write_text('<main z="2" a="1" nonce="new" data-reactid="2">\n <b>A &amp; B</b>\n</main>' if v else '<main nonce="old" data-reactid="1" a="1" z="2"><b>A &#38; B</b></main>')
    (out / '_checkout@1280.html').write_text('<main><h1>New</h1><p>Next</p><button>Go</button></main>' if v else '<main><h1>Old</h1><p>Before</p></main>')
    (out / 'OnlyDOM@desktop.html').write_text('<div><p>New</p></div>' if v else '<div><span>Old</span></div>')
    (out / 'Native@Android.xml').write_text('<root bounds="[0,0][100,100]"><node bounds="[10,20][30,40]" text="New" role="button"/><added bounds="[50,50][60,60]"/></root>' if v else '<root bounds="[0,0][100,100]"><node role="label" text="Old" bounds="[10,20][30,40]"/><removed bounds="[70,70][80,80]"/></root>')
    (out / 'Frames@Android.xml').write_text('<node bounds="[2,3][10,20]" role="button"/>' if v else '<node role="button" bounds="[0,0][5,5]"/>')
    tree = {'frame': {'x': 2 if v else 0, 'y': 0, 'width': 20, 'height': 10}, 'label': 'New' if v else 'Old', 'role': 'button' if v else 'label'}
    (out / 'Checkout@iPhone-15.json').write_text(json.dumps(tree, sort_keys=bool(v)))
    (out / 'Dump@iPhone-15.txt').write_text('New\n' if v else 'Old\n')
PY
l init screens --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/screens/lines/main"
BASE=$(l rev-parse head)
for name in storefront-screens quiet strict; do l probe run "$BASE" --probe "$name" --json >/dev/null; done
RUN=$(l probe run "$BASE" --probe storefront-screens --json)
ok 'screens reuse the stored result' python3 -c 'import json,sys; assert json.loads(sys.argv[1])[0]["reused"]' "$RUN"
OUTPUT=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])[0]["output_state"])' "$RUN")
l gc -q
ok 'derived states retain PNG and structure files after GC' test -f "$LAYR_ROOT/projects/screens/states/$OUTPUT/out/screens/_checkout@1280.png"
ok 'the HTML snapshot stays beside its PNG' test -f "$LAYR_ROOT/projects/screens/states/$OUTPUT/out/screens/_checkout@1280.html"
SAME=$(l probe diff "$BASE" "$BASE" --json)
ok 'equal screens report zero pixels and no regions' python3 -c 'import json,sys; p=next(p for p in json.loads(sys.argv[1])["probes"] if p["probe"]=="storefront-screens"); assert p["status"]=="unchanged"; assert all(c["changed_share"] in (0,None) and not c["regions"] for c in p["changes"])' "$SAME"
no 'invalid PNG content is refused' l probe run "$BASE" --probe bad
l line new feature -q
cd ../feature
printf '1\n' > version
l save -qam screens
HEAD=$(l rev-parse head)
for name in storefront-screens quiet strict; do l probe run "$HEAD" --probe "$name" >/dev/null; done
no 'the explicit diff fails on a screen over its pixel limit' l probe diff "$BASE" "$HEAD" --json
JSON=$(l probe diff "$BASE" "$HEAD" --json || true)
HUMAN=$(l probe diff "$BASE" "$HEAD" || true)
# Keep small real command samples in the test log.
printf '%s\n' "$HUMAN"
printf '%s\n' "$JSON"
check_screen() { python3 - "$JSON" "$@" <<'PY'
import json, sys
rows = json.loads(sys.argv[1])['probes']
p = next(p for p in rows if p['probe'] == 'storefront-screens')
s = {c['screen']: c for c in p['changes']}
exec(sys.argv[2])
PY
}
ok 'the one-region pixel share uses decoded pixels' check_screen 'c=s["_checkout@1280"]; assert c["changed_share"]==0.01 and c["changed_percent"]==1.0; assert c["regions"]==[dict(x=4,y=4,width=10,height=10)]'
ok 'distant areas produce two boxes' check_screen 'assert len(s["_two@1280"]["regions"])==2'
ok 'areas closer than eight pixels merge' check_screen 'assert s["_near@1280"]["regions"]==[dict(x=1,y=1,width=10,height=2)]'
ok 'more than sixteen regions stay covered by sixteen boxes' check_screen 'c=s["_many@1280"]; assert len(c["regions"])<=16; assert all(any(r["x"]<=x<r["x"]+r["width"] and r["y"]<=y<r["y"]+r["height"] for r in c["regions"]) for x in range(0,100,20) for y in range(0,100,20))'
ok 'a channel delta above tolerance changes pixels' check_screen 'assert s["_nine@1280"]["changed_pixels"]==100'
ok 'a new screen reports added' check_screen 'assert s["_added@1280"]["status"]=="added" and s["_added@1280"]["changed_share"]==1'
ok 'an absent screen reports removed' check_screen 'assert s["_removed@1280"]["status"]=="removed" and s["_removed@1280"]["changed_share"]==1'
ok 'size changes include the new area and both dimensions' check_screen 'c=s["_size@1280"]; assert c["size_changed"] and c["before"]["width"]==100 and c["after"]["width"]==120 and c["changed_pixels"]==2000 and c["changed_percent"]==16.7'
ok 'PNG encoding and HTML noise do not change a screen' check_screen 'c=s["_same@1280"]; assert c["changed_share"]==0 and c["structure_diff"]==""'
ok 'HTML reports new elements, text counts, and a unified diff' check_screen 'c=s["_checkout@1280"]; assert c["elements_added"]==1 and c["text_changes"]>=2 and "+New" in c["structure_diff"]; assert not c["structure_regions"]'
ok 'a screen can contain only structure' check_screen 'c=s["OnlyDOM@desktop"]; assert c["changed_share"] is None and c["base_png"] is None and c["elements_added"]==c["elements_removed"]==1'
ok 'XML node changes retain native boxes' check_screen 'c=s["Native@Android"]; assert c["elements_added"]==c["elements_removed"]==1 and c["text_changes"]==1; assert {r["change"] for r in c["structure_regions"]}>={"text","added","removed"}; assert any(r["x"]==10 and r["y"]==20 and r["width"]==20 and r["height"]==20 for r in c["structure_regions"])'
ok 'XML coordinates and attribute order do not change structure' check_screen 'assert s["Frames@Android"]["structure_diff"]==""'
ok 'JSON labels and roles have frame regions' check_screen 'c=s["Checkout@iPhone-15"]; assert c["text_changes"]==1; assert {r["change"] for r in c["structure_regions"]}=={"text","attribute"}; assert "frame" not in c["structure_diff"]'
ok 'plain text snapshots report text changes' check_screen 'assert s["Dump@iPhone-15"]["text_changes"]==1'
ok 'default tolerance includes a delta of eight' python3 -c 'import json,sys; p=next(p for p in json.loads(sys.argv[1])["probes"] if p["probe"]=="quiet"); assert p["status"]=="unchanged"' "$JSON"
ok 'an explicit zero tolerance detects that delta' python3 -c 'import json,sys; p=next(p for p in json.loads(sys.argv[1])["probes"] if p["probe"]=="strict"); assert p["status"]=="changed"' "$JSON"
has 'human output includes pixel share and regions' 'storefront-screens: _checkout@1280 1.0 % of pixels changed, 1 region; structure:' "$HUMAN"
has 'human output includes image dimensions' 'size 100x100 to 120x100' "$HUMAN"
# A separate queue project isolates the limit from added and removed screens.
mkdir -p "$T/queue-src"
cp screens.py "$T/queue-src/screens.py"
printf '0\n' > "$T/queue-src/version"
printf 'out/\n' > "$T/queue-src/.gitignore"
# This small writer uses the same standard-library PNG encoder.
python3 - "$T/queue-src/screens.py" <<'PY'
import pathlib, sys
p=pathlib.Path(sys.argv[1]); s=p.read_text(); s=s[:s.index("if kind in ('quiet', 'strict'):")]
p.write_text(s + "png('Checkout@iPhone-15', boxes=[(0,0,10 if v==1 else 30,10 if v==1 else 30)] if v else [])\n")
PY
cat > "$T/queue-src/layr.toml" <<'TOML'
[probe.screens]
run = ['python3', 'screens.py', 'screens']
outputs = ['out/screens']
kind = 'screenshots'
fail_over = 0.05
TOML
l init screen-queue --from "$T/queue-src" -q >/dev/null
cd "$LAYR_ROOT/projects/screen-queue/lines/main"
QUEUE_BASE=$(l rev-parse head)
l config queue.probes true
l line new under -q
cd ../under
printf '1\n' > version
l save -qam under
UNDER=$(l rev-parse head)
cd ../main
l queue add under >/dev/null
ok 'a one-percent pixel change passes a five-percent queue limit' l queue run
eq 'the successful queue installs the checked state' "$UNDER" "$(l rev-parse head)"
l line new over -q
cd ../over
printf '2\n' > version
l save -qam over
cd ../main
l queue add over >/dev/null
no 'an eight-percent pixel change fails a five-percent queue limit' l queue run
eq 'a failed screen limit preserves the target head' "$UNDER" "$(l rev-parse head)"
QUEUE=$(l queue list --json)
ok 'the queue stores screenshot pixels and useful limit reasons' python3 -c 'import json,sys; e=json.loads(sys.argv[1])[-1]; assert e["status"]=="failed"; p=e["probes"]; assert p["status"]=="failed"; assert "Checkout@iPhone-15" in p["probes"][0]["reasons"][0]; c=p["behavior"]["probes"][0]["changes"][0]; assert c["changed_pixels"]==800' "$QUEUE"
has 'the queue refusal reports the pixel limit' 'is over the limit 5.0 %' "$(l queue list)"
finish
