# Register flint's delegation test modules in pynfs's test index.
#
# pynfs discovers tests from server41tests/__init__.py's __all__, NOT by
# globbing the directory: copying a file alone installs nothing, and a
# run that discovers zero tests reports zero failures — the shape of a
# perfect pass.
import sys

MODULES = sys.argv[1:] or ["st_flintdeleg.py"]
p = "/opt/pynfs/nfs4.1/server41tests/__init__.py"
s = open(p).read()
marker = '"st_courtesy.py",'
assert marker in s, "anchor not found in __all__"
added = []
for m in MODULES:
    if m in s:
        continue
    s = s.replace(marker, marker + '\n           "%s",' % m, 1)
    added.append(m)
if added:
    open(p, "w").write(s)
    print("registered: " + " ".join(added))
else:
    print("already registered")
