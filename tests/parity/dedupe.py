"""Dedupes a harvested parity corpus: unique (fn, args), bounded line size and count per fn."""
import json, hashlib, sys, collections
src, dst = sys.argv[1], sys.argv[2]
limit = int(sys.argv[3]) if len(sys.argv) > 3 else 400
seen = set(); per = collections.Counter(); out = []
for line in open(src):
    if len(line) > 48_000:
        continue
    try:
        e = json.loads(line)
    except Exception:
        continue
    key = e['module'] + '.' + e['fn']
    h = hashlib.sha1((key + json.dumps(e['args'], sort_keys=True)).encode()).hexdigest()
    if h in seen or per[key] >= limit:
        continue
    seen.add(h); per[key] += 1
    if e.get('after') == e.get('args'):
        e.pop('after', None)
    out.append(json.dumps(e, ensure_ascii=False, separators=(',', ':')))
out.sort()
open(dst, 'w').write('\n'.join(out) + '\n')
print(len(out), 'records', sum(len(x) for x in out) / 1e6, 'MB')
