"""Run Windows release executables in an isolated temporary profile on the runner."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import shutil
import threading
import sys
import urllib.request
import uuid
import zipfile

def cli(exe, *args):
    result = subprocess.run([exe, *args], capture_output=True, encoding="utf-8", timeout=30)
    if result.returncode:
        raise RuntimeError(f"{exe.name} {args[0]} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout.strip()

def release_asset(release, name):
    matches = [a for a in release["assets"] if a["name"] == name]
    assert len(matches) == 1
    asset = matches[0]
    assert asset["state"] == "uploaded"
    assert isinstance(asset["id"], int) and re.fullmatch(r"sha256:[0-9a-f]{64}", asset["digest"])
    assert isinstance(asset["size"], int) and asset["size"] > 0
    return asset

def verify_archive(asset, archive):
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert asset["digest"] == "sha256:" + digest and asset["size"] == archive.stat().st_size
    return digest

def package_names(version, arch):
    desktop = f"GodovoyPlan-{version}-Windows-{arch}.zip"
    if arch == "x64":
        return desktop, f"GodovoyServer-{version}-Windows-x64.zip"
    return (desktop,)

def matching_draft(release, tag):
    return release["draft"] and release["tag_name"] == tag and release["target_commitish"] == os.environ["GITHUB_SHA"]

def fetch_draft(directory):
    tag, arch = release_args()
    draft = os.environ["RELEASE_DRAFT_ID"]
    assert re.fullmatch(r"[0-9]+", draft)
    api = "repos/" + os.environ["GITHUB_REPOSITORY"] + "/releases"
    raw = subprocess.check_output(["gh", "api", f"{api}/{draft}"], timeout=30)
    release = json.loads(raw)
    assert matching_draft(release, tag)
    directory.mkdir()
    for name in package_names(tag[1:], arch):
        asset = release_asset(release, name)
        archive = directory / name
        with archive.open("xb") as file:
            subprocess.run(["gh", "api", f"{api}/assets/{asset['id']}",
                            "-H", "Accept: application/octet-stream"],
                           stdout=file, check=True, timeout=90)
        print(name, verify_archive(asset, archive), flush=True)
    (directory / "release.json").write_bytes(raw)

def extract_release(release, name, programs, directory, assets):
    asset = release_asset(release, name)
    if assets is None:
        archive = directory / name
        urllib.request.urlretrieve(asset["browser_download_url"], archive)
    else:
        archive = assets / name
    digest = verify_archive(asset, archive)
    with zipfile.ZipFile(archive) as package:
        for prog in programs:
            assert package.namelist().count(prog) == 1
            (directory / prog).write_bytes(package.read(prog))
    print(name, digest, flush=True)

def get_authorized(port, key, path):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.request("GET", path, headers={"Authorization": "Bearer " + key})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()

def server_flow(exe, directory):
    vault = directory / "vault"
    try:
        cli(exe, "init", vault, "Windows QA")
    except RuntimeError:
        # Read ACLs only along the temporary test volume; no profile contents.
        for ancestor in [directory, *directory.parents]:
            print("Test directory permissions:", ancestor, flush=True)
            subprocess.run(["icacls.exe", str(ancestor)], check=False)
        raise
    key = cli(exe, "grant", vault, "Employee", "--read-only").splitlines()[-1]
    process = subprocess.Popen([exe, "serve", vault, "--port", "0"], stderr=subprocess.PIPE,
                               text=True, encoding="utf-8")
    try:
        lines = queue.Queue()
        threading.Thread(target=lambda: lines.put(process.stderr.readline()), daemon=True).start()
        line = lines.get(timeout=15)
        port = int(re.search(r"127\.0\.0\.1:(\d+)", line)[1])
        assert get_authorized(port, key, "/v1/session")[0] == 204
        status, body = get_authorized(port, key, "/v1/state?year=2028")
        assert status == 200 and json.loads(body)["workspaceName"] == "Windows QA"
        cli(exe, "revoke", vault, key.split(".")[0])
        assert get_authorized(port, key, "/v1/session")[0] == 401
    finally:
        process.terminate()
        process.wait(timeout=10)
        process.stderr.close()
    print("PASS: init, read-only session, year and revocation", flush=True)

def release_args():
    tag = os.environ["RELEASE_TAG"]
    arch = os.environ["PACKAGE_ARCH"]
    assert re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag)
    assert arch in ("x64", "x86", "ARM64")
    return tag, arch

def main():
    assert os.name == "nt"
    tag, arch = release_args()
    url = "https://api.github.com/repos/" + os.environ["GITHUB_REPOSITORY"] + "/releases/tags/" + tag
    assets = os.environ.get("RELEASE_ASSET_DIR")
    if assets:
        assets = Path(assets)
        release = json.loads((assets / "release.json").read_bytes())
        assert matching_draft(release, tag)
    else:
        with urllib.request.urlopen(url, timeout=30) as response:
            release = json.load(response)
        assert not release["draft"]
    assert release["tag_name"] == tag
    version = tag[1:]
    # Inherit the dedicated NTFS volume ACL. TemporaryDirectory installs its own OWNER RIGHTS ACE.
    directory = Path(os.environ["GP_TEST_VOLUME"]) / ("GodovoyQA-" + uuid.uuid4().hex)
    directory.mkdir()
    try:
        extract_release(release, f"GodovoyPlan-{version}-Windows-{arch}.zip",
                        ["godovoy-plan.exe", "godovoy-online.exe"], directory, assets)
        desktop = directory / "godovoy-plan.exe"
        assert cli(desktop, "--version").endswith(version)
        online = directory / "godovoy-online.exe"
        assert cli(online, "--version") == "godovoy-online " + version
        server_flow(online, directory)
        if arch == "x64":
            standalone = directory / "server"
            standalone.mkdir()
            extract_release(release, f"GodovoyServer-{version}-Windows-x64.zip",
                            [online.name], standalone, assets)
            server_flow(standalone / online.name, standalone)
        for dll in ("opengl32.dll", "libgallium_wgl.dll"):
            shutil.copyfile(Path(os.environ["GP_TEST_RENDERER"]) / dll, directory / dll)
        subprocess.run(["pwsh", "-NoProfile", "-File", Path(__file__).with_name("window-smoke.ps1"),
                        desktop, directory / "desktop-profile"], check=True, timeout=75)

    finally:
        shutil.rmtree(directory)

if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fetch-draft":
        fetch_draft(Path(sys.argv[2]))
    else:
        assert len(sys.argv) == 1
        main()
