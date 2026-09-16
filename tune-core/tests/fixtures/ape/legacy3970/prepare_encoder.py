#!/usr/bin/env python3
"""Build the historical reference encoder in an empty temporary directory.

Run on little-endian x86_64 Linux. Nothing is installed or used by Tune.
The historical SDK remains external, under its own licence.
"""
from pathlib import Path
import hashlib
import re
import shutil
import subprocess
import tarfile
import urllib.request

HERE = Path(__file__).resolve().parent
url = "https://tmkk.undo.jp/monkey/MAC_SDK_397_OSX_20040718.tar.gz"
archive = Path("mac397.tar.gz")
if not archive.exists():
    urllib.request.urlretrieve(url, archive)
assert hashlib.sha256(archive.read_bytes()).hexdigest() == (
    "ccc24b2fccb0eaf0dd3c8eb3ccad74b42d2bb26cd87c6b3fe0da7d0fd6ea8d83"
)
with tarfile.open(archive) as tar:
    tar.extractall(filter="data")
source = Path("MAC_SDK_397_OSX/Source")
overlay = Path("encoder397")
overlay.mkdir(exist_ok=True)

all_h = (source / "Shared/All.h").read_text()
for bits, kind in [(16, "short"), (32, "int")]:
    pattern = rf"static inline {kind} swap_endian{bits}\({kind} x\)\s*\{{.*?\}}"
    all_h, count = re.subn(pattern, f"static inline {kind} swap_endian{bits}({kind} x) {{ return x; }}", all_h, flags=re.S)
    assert count == 1
all_h += "\n#include <algorithm>\ntemplate<class A,class B> auto min(A a,B b) { return a < b ? a : b; }\nusing std::max;\n"
(overlay / "All.h").write_text(all_h)
no_windows = (source / "Shared/NoWindows.h").read_text()
no_windows = no_windows.replace("typedef unsigned long DWORD", "typedef unsigned int DWORD")
no_windows = "\n".join(line for line in no_windows.splitlines()
                       if not line.startswith(("#define min(", "#define max("))) + "\n"
(overlay / "NoWindows.h").write_text(no_windows)

nn = (source / "MACLib/NNFilter.cpp").read_text()
for signature, body in [
    ("void CNNFilter::AdaptAltiVec", "AdaptNoMMX(pM, pAdapt, nDirection, nOrder);"),
    ("int CNNFilter::CalculateDotProductAltiVec", "return CalculateDotProductNoMMX(pA, pB, nOrder);"),
]:
    start = nn.index("{", nn.index(signature))
    end, depth = start + 1, 1
    while depth:
        depth += (nn[end] == "{") - (nn[end] == "}")
        end += 1
    nn = nn[:start] + "{ " + body + " }" + nn[end:]
(overlay / "NNFilter.cpp").write_text(nn)
shutil.copyfile(HERE / "encode.cpp", overlay / "encode.cpp")
subprocess.run([
    "g++", "-O2", "-fwrapv", "-fpermissive", "-std=c++17",
    "-Iencoder397", "-I" + str(source / "MACLib"), "-I" + str(source / "Shared"),
    "encoder397/encode.cpp", "encoder397/NNFilter.cpp",
    *[str(source / "MACLib" / name) for name in
      ["BitArray.cpp", "Prepare.cpp", "NewPredictor.cpp", "APECompressCore.cpp"]],
    "-o", "encoder397/encode",
], check=True)
subprocess.run(["python3", str(HERE / "generate.py")], check=True)
