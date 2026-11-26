"""Record and verify the worker archive and separate pack inventory."""

import argparse
import hashlib
import json
import re
from pathlib import Path
from urllib.parse import quote


def sha256(path):
    with path.open("rb") as data:
        return hashlib.file_digest(data, "sha256").hexdigest()


def record_file(path):
    return {"name": path.name, "sha256": sha256(path), "bytes": path.stat().st_size}


def read_inventory(path):
    packages = {}
    for row in (path / "packages.tsv").read_text().splitlines():
        name, version, arch, source, source_version, status = row.split("\t")
        if status != "installed":
            continue
        if name in packages:
            raise ValueError("duplicate package")
        packages[name] = {
            "version": version, "architecture": arch, "source": source,
            "source_version": source_version,
        }
    return packages


def inspect_image(path, architecture, revision, profile):
    image, = json.loads(path.read_text())
    config = image["Config"]
    labels = config["Labels"]
    if (
        image["Os"] != "linux" or image["Architecture"] != architecture
        or config["User"] != "1000:1000"
        or config["Entrypoint"] != ["/usr/bin/hypermesh-visor-headless"]
        or config["Healthcheck"]["Test"] != ["CMD", "/usr/bin/hypermesh-visor-headless", "worker-status"]
        or config.get("ExposedPorts") or config.get("Volumes")
        or labels["org.opencontainers.image.revision"] != revision
        or labels["sh.hyperme.visor.profile"] != profile
    ):
        raise ValueError("worker image does not match its build profile")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image["Id"]):
        raise ValueError("invalid image ID")
    return image


def spdx(inventory, image, revision):
    components = []
    relationships = []
    for index, (name, entry) in enumerate(sorted(inventory.items()), 1):
        component = f"SPDXRef-Package-{index}"
        purl = (
            f"pkg:deb/{'hypermesh' if name == 'hypermesh-visor-headless' else 'debian'}/"
            f"{name}@{quote(entry['version'], safe='')}?arch={entry['architecture']}"
        )
        components.append({
            "name": name, "SPDXID": component, "versionInfo": entry["version"],
            "filesAnalyzed": False, "downloadLocation": "NOASSERTION",
            "licenseConcluded": "NOASSERTION", "licenseDeclared": "NOASSERTION",
            "copyrightText": "NOASSERTION",
            "externalRefs": [{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl", "referenceLocator": purl,
            }],
        })
        relationships.append({
            "spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES",
            "relatedSpdxElement": component,
        })
    return {
        "spdxVersion": "SPDX-2.3", "dataLicense": "CC0-1.0", "SPDXID": "SPDXRef-DOCUMENT",
        "name": "hypermesh-visor-cloud-worker",
        "documentNamespace": f"https://github.com/hyperme-sh/visor/spdx/{revision}/{image['Id'][7:]}",
        "creationInfo": {"created": image["Created"].split(".")[0].removesuffix("Z") + "Z",
                         "creators": ["Tool: visor-worker-inventory"]},
        "packages": components, "relationships": relationships,
    }


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")


def build(args):
    if not re.fullmatch(r"[0-9a-f]{40}", args.revision):
        raise ValueError("expected a full source commit")
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}[args.arch]
    output = args.output
    base = inspect_image(args.base_image, args.arch, args.revision, "base")
    pack = inspect_image(args.pack_image, args.arch, args.revision, "coding-v1")
    base_layers = base["RootFS"]["Layers"]
    if pack["RootFS"]["Layers"][:len(base_layers)] != base_layers or base["Id"] == pack["Id"]:
        raise ValueError("coding pack must extend the selected base image")
    base_packages, pack_packages = (read_inventory(output / profile) for profile in ("base", "coding-v1"))
    if (
        "bash" not in base_packages or "hypermesh-visor-headless" not in base_packages
        or any(name in base_packages or name not in pack_packages for name in ("git", "shellcheck"))
        or pack_packages.get("hypermesh-visor-headless") != base_packages["hypermesh-visor-headless"]
    ):
        raise ValueError("unexpected base or optional tool inventory")
    labels = base["Config"]["Labels"]
    base_ref = labels["org.opencontainers.image.base.name"]
    snapshot = labels["sh.hyperme.visor.debian-snapshot"]
    if not re.fullmatch(r"debian:trixie-slim@sha256:[0-9a-f]{64}", base_ref):
        raise ValueError("base image source is not digest-pinned Debian")
    if not re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", snapshot):
        raise ValueError("invalid Debian snapshot")
    sources = dict(line.split("=", 1) for line in (output / "base/build.txt").read_text().splitlines())
    if sources["visor"] != args.revision or sources["architecture"] != architecture:
        raise ValueError("headless package does not match its worker image")
    for profile, image, packages in (("base", base, base_packages), ("coding-v1", pack, pack_packages)):
        write_json(output / profile / "sbom.spdx.json", spdx(packages, image, args.revision))
        write_json(output / profile / "packages.json", packages)
    manifest = {
        "schema_version": 1, "source_revision": args.revision, "platform": f"linux/{args.arch}",
        "base_source": base_ref, "debian_snapshot": snapshot, "headless_sources": sources,
        "image_id": base["Id"], "archive": record_file(output / f"visor-cloud-worker-{args.arch}.tar.gz"),
        "coding_pack_image_id": pack["Id"], "coding_pack_artifact": "operator_image_build",
        "files": {
            file.relative_to(output).as_posix(): sha256(file)
            for profile in ("base", "coding-v1")
            for file in sorted((output / profile).rglob("*")) if file.is_file()
        },
    }
    write_json(output / "manifest.json", manifest)


def verify(output):
    manifest = json.loads((output / "manifest.json").read_text())
    if manifest["schema_version"] != 1 or manifest["platform"] not in ("linux/amd64", "linux/arm64"):
        raise ValueError("unknown worker artifact format")
    architecture = manifest["platform"].split("/")[1]
    archive = output / f"visor-cloud-worker-{architecture}.tar.gz"
    if record_file(archive) != manifest["archive"]:
        raise ValueError("worker archive checksum mismatch")
    actual = {
        file.relative_to(output).as_posix(): sha256(file)
        for profile in ("base", "coding-v1")
        for file in sorted((output / profile).rglob("*")) if file.is_file()
    }
    if actual != manifest["files"]:
        raise ValueError("worker inventory checksum mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    build_parser = sub.add_parser("build")
    build_parser.add_argument("output", type=Path)
    build_parser.add_argument("base_image", type=Path)
    build_parser.add_argument("pack_image", type=Path)
    build_parser.add_argument("arch", choices=("amd64", "arm64"))
    build_parser.add_argument("revision")
    sub.add_parser("verify").add_argument("output", type=Path)
    args = parser.parse_args()
    if args.command == "build":
        build(args)
    else:
        verify(args.output)


if __name__ == "__main__":
    main()
