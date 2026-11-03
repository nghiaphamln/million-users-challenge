#!/usr/bin/env python3
"""Compile the real compressor; check gzip CRC/length and decompression with Python's independent decoder."""
import gzip
import json
import os
from pathlib import Path
import random
import subprocess
import tempfile

root=Path(__file__).resolve().parents[1]
lib=root/'deps/isa-l-2.31.1/bin/isa-l.a'
if not lib.exists():
    raise SystemExit('Run build.sh first')
with tempfile.TemporaryDirectory() as directory:
    tmp=Path(directory)
    binary=tmp/'gzip-test'
    subprocess.run([os.environ.get('C3C','c3c'),'compile','-O3','--single-module=yes','--safe=no','-z',str(lib),'-o',str(binary),str(root/'test/gzip.c3'),str(root/'sys.c3'),str(root/'gz.c3')],check=True)
    rng=random.Random(82)
    cases=[b'a'*65535,bytes(range(256))*255,json.dumps({'posts':['quote " newline\n backslash\\ café ✓ '*20]*20},ensure_ascii=False).encode()]
    for n in (1024,1025,4096,16384,32767,32768,65534,65535):
        cases.extend([rng.randbytes(n),(b'abcababc0123456789'*5000)[:n]])
    for index,data in enumerate(cases):
        source=tmp/'input';output=tmp/'output.gz';source.write_bytes(data)
        env=dict(os.environ,INPUT=str(source),OUTPUT=str(output))
        subprocess.run([str(binary)],env=env,check=True,stdout=subprocess.DEVNULL)
        assert gzip.decompress(output.read_bytes())==data, f'case {index}: mismatch'
    print(f'PASS: {len(cases)} gzip round trips, including CRC and input-length validation')
