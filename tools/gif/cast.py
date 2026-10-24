"""Turn rec.sh output (real commands and real output) into an asciicast v2 file with typing."""
import base64, json, sys

src, dst, title = sys.argv[1], sys.argv[2], sys.argv[3]
steps, cur = [], None
for line in open(src, encoding="utf-8", errors="replace").read().split("\n"):
    if line.startswith("@@CMD "):
        cur = [base64.b64decode(line[6:]).decode(), []]
    elif line == "@@END":
        steps.append(cur); cur = None
    elif cur is not None:
        cur[1].append(line)

W, t, ev = 88, 0.0, []
folder = "main"
def out(s, dt=0.0):
    global t
    t += dt
    ev.append([round(t, 3), "o", s])
def prompt():
    out(f"\x1b[1;34m{folder}\x1b[0m \x1b[90m$\x1b[0m ", 0.0)

rows = 0
for cmd, lines in steps:
    prompt()
    t += 0.5
    for ch in cmd:
        out(ch, 0.035)
    out("\r\n", 0.35)
    while lines and lines[-1] == "":
        lines.pop()
    for l in lines:
        l = l.replace("\t", "    ")
        if l.startswith("fatal:") or "No such file" in l:
            l = f"\x1b[31m{l}\x1b[0m"
        out(l + "\r\n", 0.03)
    rows += 1 + (len(cmd) + 8) // W + sum(1 + len(l.replace('\t','    ')) // W for l in lines)
    if cmd.startswith("cd "):
        folder = cmd.split("/")[-1].strip()
    t += 1.1
prompt()
t += 2.5
out("", 0)
H = max(rows + 2, 12)
with open(dst, "w") as f:
    f.write(json.dumps({"version": 2, "width": W, "height": H, "title": title}) + "\n")
    for e in ev:
        f.write(json.dumps(e) + "\n")
print(dst, "rows", rows, "height", H)
