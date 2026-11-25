#!/usr/bin/env python3
"""Build godovoy-online with Cargo.lock and package the server and operating guide."""
import argparse
import gzip
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]
DOCS = ("SERVER.md", "ONLINE.md", "ONLINE-API.md", "DATA-PROTECTION.md")
LINUX_EXAMPLES = ("godovoy-calendar.service", "godovoy-office", "office.env.example")
PLATFORMS = {
    "linux-x64": ("Linux-x64", ["x86_64-unknown-linux-musl"]),
    "linux-arm64": ("Linux-ARM64", ["aarch64-unknown-linux-musl"]),
    "windows-x64": ("Windows-x64", ["x86_64-pc-windows-msvc" if os.name == "nt" else "x86_64-pc-windows-gnullvm"]),
    "macos": ("macOS", ["aarch64-apple-darwin", "x86_64-apple-darwin"]),
}


def build(cargo, target_dir, target, program):
    env = os.environ.copy()
    key = "CARGO_TARGET_" + target.upper().replace("-", "_")
    if target.endswith("-musl") and key + "_LINKER" not in env:
        rustc = env.get("RUSTC", "rustc")
        info = subprocess.check_output([rustc, "-vV"], text=True, cwd=ROOT).splitlines()
        host = next(line.split(": ", 1)[1] for line in info if line.startswith("host: "))
        sysroot = subprocess.check_output([rustc, "--print", "sysroot"], text=True, cwd=ROOT).strip()
        lld = Path(sysroot, "lib", "rustlib", host, "bin", "rust-lld" + (".exe" if os.name == "nt" else ""))
        env[key + "_LINKER"] = str(lld)
        env[key + "_RUSTFLAGS"] = (env.get(key + "_RUSTFLAGS", "") + " -C linker-flavor=ld.lld").strip()
    if "-windows-" in target:
        env.setdefault(key + "_RUSTFLAGS", "-C target-feature=+crt-static")
    subprocess.run([cargo, "build", "--locked", "--release", "--bin", "godovoy-online",
                    "--no-default-features", "--features", "server", "--target", target],
                   env=env, cwd=ROOT, check=True)
    return target_dir / target / "release" / program


def write_archive(path, entries, linux):
    if linux:
        with path.open("wb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name, source, mode in entries:
                    data = source.read_bytes()
                    item = tarfile.TarInfo(name)
                    item.size, item.mode = len(data), mode
                    archive.addfile(item, io.BytesIO(data))
    else:
        with zipfile.ZipFile(path, mode="w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, source, mode in entries:
                item = zipfile.ZipInfo(name)
                item.create_system = 3
                item.external_attr = (0o100000 | mode) << 16
                archive.writestr(item, source.read_bytes(), compress_type=zipfile.ZIP_DEFLATED)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("platform", choices=tuple(PLATFORMS))
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.platform == "macos" and sys.platform != "darwin":
        parser.error("A Mac with lipo and codesign is required for the universal macOS package.")
    cargo = os.environ.get("CARGO", "cargo")
    meta = json.loads(subprocess.check_output([cargo, "metadata", "--no-deps", "--locked", "--format-version", "1"], cwd=ROOT))
    version = next(pkg["version"] for pkg in meta["packages"] if pkg["name"] == "godovoy-plan")
    target_dir = Path(meta["target_directory"])
    label, targets = PLATFORMS[args.platform]
    linux = args.platform.startswith("linux-")
    program = "godovoy-online.exe" if args.platform == "windows-x64" else "godovoy-online"
    binaries = [build(cargo, target_dir, target, program) for target in targets]
    args.output.mkdir(parents=True, exist_ok=True)
    output = args.output / ("GodovoyServer-" + version + "-" + label + (".tar.gz" if linux else ".zip"))
    with tempfile.TemporaryDirectory() as tmp:
        binary = binaries[0]
        if args.platform == "macos":
            binary = Path(tmp) / program
            subprocess.run(["lipo", "-create", *binaries, "-output", binary], check=True)
            subprocess.run(["codesign", "--force", "--sign", "-", "--timestamp=none",
                            "--identifier", "local.godovoyplan.server", binary], check=True)
            subprocess.run(["codesign", "--verify", "--strict", "--all-architectures", binary], check=True)
        entries = [(program, binary, 0o755)] + [(doc, ROOT / doc, 0o644) for doc in DOCS]
        if linux:
            for filename in LINUX_EXAMPLES:
                path = Path("packaging/server") / filename
                entries.append((path.as_posix(), ROOT/path, 0o755 if filename == "godovoy-office" else 0o644))
        # Publish only a complete archive, including when two builds share the output directory.
        with tempfile.TemporaryDirectory(dir=output.parent) as pending:
            staged = Path(pending) / output.name
            write_archive(staged, entries, linux)
            staged.replace(output)
    print(output.resolve())


if __name__ == "__main__":
    main()
