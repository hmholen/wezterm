"""Count display changes the user did not cause: changes more than 20 ms
after the most recent key press. With working local echo these should only
be the final settle (e.g. syntax colours, an autosuggestion appearing) and
the results of keys that cannot be predicted (Enter, Ctrl+U)."""
import re, sys
for path in sys.argv[1:]:
    last_key, counts = -1e9, {}
    for line in open(path):
        m = re.match(r'\s*(\d+) (\w+) (KEY \S+|x=.*)$', line.rstrip('\n'))
        if not m:
            continue
        t, phase, rest = int(m.group(1)), m.group(2), m.group(3)
        if rest.startswith('KEY'):
            last_key = t
        elif phase != 'init' and t - last_key > 20:
            counts[phase] = counts.get(phase, 0) + 1
    total = sum(counts.values())
    print(f"{path.split('/')[-2]:12s} unprompted changes: {total:3d}  {counts}")
