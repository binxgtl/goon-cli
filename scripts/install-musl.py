#!/usr/bin/env python3
"""Install musl using authenticated APT mirrors, then a signed offline cache.

Run only on Ubuntu CI runners. No source files or system APT sources are changed.
The cache contains Debian packages plus their signed repository metadata; cached
checksums alone are deliberately not accepted as proof of authenticity.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

PACKAGES = ("musl", "musl-dev", "musl-tools")
KEYRING = "/usr/share/keyrings/ubuntu-archive-keyring.gpg"
MIRRORS = {
    "amd64": ("https://mirrors.edge.kernel.org/ubuntu", "https://mirrors.mit.edu/ubuntu",
              "https://mirrors.ocf.berkeley.edu/ubuntu", "https://ftp.osuosl.org/pub/ubuntu"),
    "arm64": ("https://mirrors.mit.edu/ubuntu-ports", "https://mirrors.ocf.berkeley.edu/ubuntu-ports",
              "https://ftp.fau.de/ubuntu-ports", "https://mirrors.tuna.tsinghua.edu.cn/ubuntu-ports"),
}


def run(args, timeout=120):
    # Let a root-owned timeout supervise APT's root-owned children as well.
    if args[0] == "sudo":
        args = ["sudo", "timeout", "--kill-after=5", str(timeout), *args[1:]]
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE, timeout=timeout + 10).stdout


def paragraphs(text):
    """Read the fields we need from Debian control/Release/Packages records."""
    for paragraph in text.strip().split("\n\n"):
        fields = {}
        key = None
        for line in paragraph.splitlines():
            if line.startswith((" ", "\t")) and key:
                fields[key] += "\n" + line.strip()
            elif ":" in line:
                key, value = line.split(":", 1)
                fields[key] = value.strip()
        if fields:
            yield fields


def signed_hashes(inrelease):
    # gpgv checks against the runner's Ubuntu archive keyring, not cached keys.
    with tempfile.TemporaryDirectory(prefix="goon-release-") as directory:
        release = Path(directory) / "Release"
        run(["gpgv", "--keyring", KEYRING, "--output", str(release), str(inrelease)])
        fields = next(paragraphs(release.read_text()))
    hashes = {}
    for line in fields.get("SHA256", "").splitlines():
        if line.strip():
            digest, size, name = line.split()
            hashes[name] = (digest, int(size))
    if not hashes:
        raise ValueError("Signed Release has no SHA256 index hashes")
    return hashes


def digest(data):
    return hashlib.sha256(data).hexdigest()


def check_index(data, hashes, metakey):
    if hashes.get(metakey) != (digest(data), len(data)):
        raise ValueError("Packages index does not match signed Release: " + metakey)


def package_identity(path):
    fields = run(["dpkg-deb", "--field", str(path), "Package", "Version", "Architecture"])
    record = next(paragraphs(fields))
    return tuple(record[key] for key in ("Package", "Version", "Architecture"))


def package_records(data):
    return {tuple(record.get(key) for key in ("Package", "Version", "Architecture")): record
            for record in paragraphs(data.decode("utf-8")) if "SHA256" in record}


def cache_packages(archives, lists, targets, cache, arch):
    """Save only indexes actually needed to authenticate the downloaded .debs."""
    debs = list(archives.glob("*.deb"))
    identities = {package_identity(path): path for path in debs}
    if not set(PACKAGES).issubset({identity[0] for identity in identities}):
        raise ValueError("APT did not download all requested musl packages")
    with tempfile.TemporaryDirectory(dir=cache.parent, prefix="goon-apt-verified-") as directory:
        stage = Path(directory)
        remaining = dict(identities)
        releases = []
        for path in lists.glob("*_InRelease"):
            try:
                releases.append((path, signed_hashes(path)))
            except (subprocess.SubprocessError, ValueError):
                # Default runner sources may include third-party signing keys.
                # Only Ubuntu archive signatures can authenticate our packages.
                continue
        for line in targets.splitlines():
            filename, metakey = line.split("|", 1)
            data = subprocess.run(["/usr/lib/apt/apt-helper", "cat-file", filename], check=True,
                                  stdout=subprocess.PIPE, timeout=120).stdout
            records = package_records(data)
            matched = {identity: path for identity, path in remaining.items() if identity in records}
            if not matched:
                continue
            release = next((path for path, hashes in releases
                            if hashes.get(metakey) == (digest(data), len(data))), None)
            if release is None:
                raise ValueError("Downloaded index lacks matching signed Release")
            bundle = stage / str(len(list(stage.iterdir())))
            bundle.mkdir()
            shutil.copyfile(release, bundle / "InRelease")
            (bundle / "Packages").write_bytes(data)
            (bundle / "metakey").write_text(metakey)
            for identity, path in matched.items():
                record = records[identity]
                contents = path.read_bytes()
                if record["SHA256"] != digest(contents) or int(record["Size"]) != len(contents):
                    raise ValueError("Downloaded package checksum mismatch: " + path.name)
                shutil.copyfile(path, stage / path.name)
                del remaining[identity]
        if remaining:
            raise ValueError("Packages missing from authenticated indexes: " + repr(list(remaining)))
        (stage / "context.json").write_text(json.dumps({"arch": arch, "suite": "noble", "schema": 1}))
        # Never leave a partially validated cache eligible for upload.
        if cache.exists():
            shutil.rmtree(cache)
        shutil.copytree(stage, cache)


def install_cached(cache, arch):
    context = json.loads((cache / "context.json").read_text())
    if context != {"arch": arch, "suite": "noble", "schema": 1}:
        raise ValueError("Offline cache architecture/suite/schema mismatch")
    records = {}
    for bundle in sorted(path for path in cache.iterdir() if path.is_dir()):
        data = (bundle / "Packages").read_bytes()
        check_index(data, signed_hashes(bundle / "InRelease"), (bundle / "metakey").read_text())
        records.update(package_records(data))
    debs = sorted(cache.glob("*.deb"))
    names = set()
    for path in debs:
        identity = package_identity(path)
        if identity[2] not in (arch, "all"):
            raise ValueError("Offline package has wrong architecture")
        record = records.get(identity, {})
        contents = path.read_bytes()
        if record.get("SHA256") != digest(contents) or int(record.get("Size", -1)) != len(contents):
            raise ValueError("Offline package lacks authenticated checksum: " + path.name)
        names.add(identity[0])
    if not set(PACKAGES).issubset(names):
        raise ValueError("Offline cache is missing required musl packages")
    # Do not force broken dependencies or downgrade an updated runner package.
    run(["sudo", "dpkg", "--skip-same-version", "--refuse-downgrade", "--install", *map(str, debs)])
    run(["sudo", "dpkg", "--configure", "-a"])
    verify_installed()


def verify_installed():
    for package in PACKAGES:
        if run(["dpkg-query", "--show", "--showformat=${db:Status-Status}", package]).strip() != "installed":
            raise ValueError("Package is not installed: " + package)
    run(["musl-gcc", "--version"])


def install_online(options, archives, lists, cache, arch):
    apt = ["sudo", "apt-get", "-q", "-o", "Acquire::http::Timeout=15", "-o", "Acquire::https::Timeout=15",
           "-o", "Acquire::Retries=2", "-o", "Acquire::ForceIPv4=true", "-o", "APT::Get::AllowUnauthenticated=false",
           "-o", "Acquire::AllowInsecureRepositories=false", "-o", "Dir::Cache::archives=" + str(archives), *options]
    run([*apt, "--error-on=any", "update"])
    run([*apt, "--download-only", "--reinstall", "-y", "install", *PACKAGES], timeout=180)
    targets = run([*apt, "indextargets", "--format", "$(FILENAME)|$(METAKEY)", "Created-By: Packages"])
    cache_packages(archives, lists, targets, cache, arch)
    run([*apt, "--no-download", "-y", "install", *PACKAGES], timeout=180)
    verify_installed()


def main():
    arch = run(["dpkg", "--print-architecture"]).strip()
    if arch not in MIRRORS or 'VERSION_CODENAME=noble' not in Path("/etc/os-release").read_text():
        raise ValueError("This bootstrap supports Ubuntu noble amd64/arm64 only")
    cache = Path(os.environ.get("GOON_APT_CACHE", "/tmp/goon-apt-cache"))
    cache.parent.mkdir(parents=True, exist_ok=True)
    errors = []
    # APT owns its partial/ directories as _apt/root. Leave those transient
    # directories to the ephemeral runner rather than masking a successful
    # install with an unprivileged TemporaryDirectory cleanup failure.
    with tempfile.TemporaryDirectory(prefix="goon-apt-", ignore_cleanup_errors=True) as directory:
        work = Path(directory)
        # _apt needs directory traversal permission for downloads.
        work.chmod(0o755)
        sources = [None, *MIRRORS[arch]]
        for index, mirror in enumerate(sources):
            attempt = work / str(index)
            archives = attempt / "archives"
            archives.mkdir(parents=True)
            attempt.chmod(0o755)
            archives.chmod(0o755)
            options = []
            lists = Path("/var/lib/apt/lists")
            if mirror:
                sources_dir = attempt / "sources.list.d"
                sources_dir.mkdir()
                (sources_dir / "ubuntu.list").write_text(
                    f"deb [arch={arch} signed-by={KEYRING}] {mirror} noble main universe\n"
                    f"deb [arch={arch} signed-by={KEYRING}] {mirror} noble-updates main universe\n"
                    f"deb [arch={arch} signed-by={KEYRING}] {mirror} noble-security main universe\n")
                lists = attempt / "lists"
                lists.mkdir()
                options = ["-o", "Dir::Etc::sourcelist=/dev/null", "-o", "Dir::Etc::sourceparts=" + str(sources_dir),
                           "-o", "Dir::State::lists=" + str(lists)]
            print("APT tier " + ("1: runner defaults" if mirror is None else "2: " + mirror), flush=True)
            try:
                install_online(options, archives, lists, cache, arch)
                return
            except (subprocess.SubprocessError, OSError, ValueError, KeyError) as error:
                errors.append(str(error))
                print("::warning::APT attempt failed: " + str(error), flush=True)
        print("APT tier 3: authenticated cached Debian packages", flush=True)
        try:
            install_cached(cache, arch)
        except (subprocess.SubprocessError, OSError, ValueError, KeyError) as error:
            raise RuntimeError("All authenticated APT sources and offline cache failed: " + "; ".join([*errors, str(error)])) from error


if __name__ == "__main__":
    main()
