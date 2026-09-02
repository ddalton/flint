# Register the flint negative-leg module in pynfs's test index.
# pynfs discovers tests from server41tests/__init__.py's __all__, NOT by
# globbing the directory: copying the file alone installs nothing, and a
# run that discovers zero tests reports zero failures.
p = "/opt/pynfs/nfs4.1/server41tests/__init__.py"
s = open(p).read()
if "st_flintdeleg.py" in s:
    print("already registered")
else:
    marker = '"st_courtesy.py",'
    assert marker in s, "anchor not found in __all__"
    s = s.replace(marker, marker + '\n           "st_flintdeleg.py",', 1)
    open(p, "w").write(s)
    print("registered")
