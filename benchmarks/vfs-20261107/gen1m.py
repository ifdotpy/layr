"""Create a synthetic tree like a big checkout: 250 x 250 dirs x 16 files = 1,000,000 files."""
import os, sys, random
root = sys.argv[1]
top, mid, per = 250, 250, 16
random.seed(1)
words = ['fn', 'let', 'struct', 'impl', 'void', 'int', 'return', 'include', 'class', 'namespace', 'static', 'const']
lines = [' '.join(random.choice(words) for _ in range(8)) + '\n' for _ in range(4096)]
n = 0
for a in range(top):
    for b in range(mid):
        d = os.path.join(root, f'm{a:03}', f's{b:03}')
        os.makedirs(d, exist_ok=True)
        for c in range(per):
            k = random.randint(10, 60)
            s = random.randrange(0, 4096 - k)
            with open(os.path.join(d, f'f{c:02}.cc'), 'w') as f:
                f.writelines(lines[s:s + k])
            n += 1
    if a % 25 == 0:
        print(a, n, flush=True)
print('files', n)
