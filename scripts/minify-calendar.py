"""Regenerate the shipped JavaScript with esbuild 0.28.2."""
import argparse
import hashlib
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--esbuild", default="esbuild")
tool = parser.parse_args().esbuild
if subprocess.check_output([tool, "--version"], text=True).strip() != "0.28.2":
    raise SystemExit("Expected esbuild 0.28.2")
assets = Path(__file__).resolve().parents[1] / "src/online"
sources = ("calendar.js", "calendar-wire.mjs")
source = b"\0".join((assets / name).read_text(encoding="utf-8").encode() for name in sources)
compact = subprocess.run(
    [tool, "calendar.js", "--bundle", "--format=iife", '--banner:js="use strict";',
     "--charset=utf8", "--target=es2021", "--minify"],
    cwd=assets,
    stdout=subprocess.PIPE,
    check=True,
).stdout
(assets / "calendar.min.js").write_bytes(compact)
for name, contents in [("calendar.source.sha256", source), ("calendar.min.sha256", compact)]:
    (assets / name).write_text(hashlib.sha256(contents).hexdigest() + "\n", encoding="ascii")
