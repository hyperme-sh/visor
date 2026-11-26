# Cloud Visor workers

The cloud worker uses stock Debian 13 with the existing static headless Visor
and Conductor package. This follows the distribution choice in the
[container base decision](container-base.md) and keeps the graphical desktop
image separate. Native builds support AMD64 and ARM64.

A maintained Visor derivative could offer an image with a custom kernel,
device policy or tailored boot chain. Those features would add release
engineering, upstream security tracking, kernel and boot qualification,
architecture-specific images, and incident ownership. Estimate two engineer
days per month for a derivative plus one day per security rebuild, before
provider or architecture qualification. The stock profile leaves kernel,
cloud-init and boot maintenance with Debian. The Visor repository owns the
package, the pinned distribution refresh, inventory generation and regression
contracts. Infra owns any managed placement and first-boot identity handoff
under its [worker placement decision](https://github.com/FyberLabs/infra/issues/3542).
The Debian reference cloud images are published by the
[Debian Cloud Team](https://cloud.debian.org/images/cloud/).

## CI artifact

The headless workflow builds both variants from the same Visor and Conductor
revisions. Download the matching `visor-cloud-worker-x86_64` or
`visor-cloud-worker-aarch64` artifact. It includes the base archive, its image
config ID and SHA-256, the distro base digest, the Debian package snapshot,
source revisions, package inventories, license notices where available and a
package-level SPDX document for each profile. Review the archive alongside its
manifest from the trusted CI run. The coding inventory identifies the
operator-buildable profile.

```sh
python3 ci/cloud-worker/manifest.py verify ARTIFACT_DIRECTORY
docker image load --input ARTIFACT_DIRECTORY/visor-cloud-worker-amd64.tar.gz
```

The stock image runs as UID 1000 and reports readiness through its private
control socket. Run `hypermesh-visor-headless worker-status` as the worker
account to see readiness and available tool versions. The image contains
empty runtime and state directories.
Supply deployment-specific authority through the existing enrollment path
after boot. Keep account credentials, browser state, and working data in
separate operator-controlled storage. Keep remote access off until the
deployment supplies its own enrollment and assignment. See the
[remote bot contract](remote-bots.md) for the scoped bot credential and local
consent requirements.

On a stock Debian VM, use the matching `.deb` and its checksum from the
[native headless bundle](headless.md). Provision a dedicated system account
named `visor` with group `visor`, install the verified package, then explicitly
enable `hypermesh-visor-headless.service` with systemd. The package installs the
unit disabled. Its runtime and state directories are private to that account.
If placement supplies worker settings, put environment settings in a
root-owned `/etc/hypermesh-visor-headless/worker.env` and point them at separate
owner-only credential files delivered through the existing first-boot path.
Keep secret values out of cloud-init data and image builds.

For updates, rebuild against the current approved source, base digest and signed
Debian snapshot. Review the new inventory and verify the candidate archive.
Replace the disposable worker using the moving TEST channel when operating in
TEST. For standalone artifacts, keep the last verified archive for rollback
and use the deployment identity and task controls to drain and replace the
worker. For a Debian package upgrade, stop the service, install the verified
candidate and start again. Roll back with the earlier verified package and the
same external state. When replacing enrolled workers, let the existing
placement and enrollment controller reconcile identity and assignment.

## Optional profiles

Run `hypermesh-visor-headless catalog packs` to inspect the profiles or
`hypermesh-visor-headless catalog packs coding-v1` for one plan.

- **coding-v1** uses the stock Bash terminal shell, plus Debian Git and ShellCheck.
  Build the optional profile on demand from the same source revision as base.
  The base artifact keeps the optional packages out of its inventory.
- **local-ai-v1** lists the Visor-managed CPU runner from the catalog. It needs
  a compiler with static libc, Bubblewrap and suitable CPU memory for the
  selected model. Model and tokenizer terms apply separately.
- **business-v1** lists Gitea and Vikunja as separately installed services.
  Plan persistent data and database capacity based on workload.
  Both the AI and business plans link to the catalog license and install route.

```sh
docker build -f ci/headless/Dockerfile --target cloud-coding \
  --build-arg EXPECTED_ARCH="$(uname -m)" \
  --build-arg SOURCE_REVISION="$(git rev-parse HEAD)" \
  --tag visor-cloud-worker-coding:local .
```

APT verifies packages against Debian's signed snapshot during the optional
build. To add or remove the pack, replace the worker with the selected image
and keep workspace data on its existing storage. The prior image remains
available after an unsuccessful build. A repeat build can reuse the same
inputs. Debian's upstream copyright files describe package redistribution
terms. The optional coding image stays an operator build.

## Qualification

Pull requests build both profiles and check their inventories, base ancestry
and archive checksums. The headless workflow also has a manual qualification
event and runs worker boot, fixture Git and lint operations, profile removal
and shutdown on main. These jobs use the existing native CI runners and a
disposable, network-isolated container workspace. Managed VM boot, identity
enrollment and scoped task delivery use the Infra placement and Conductor
contracts. Track that work in [Visor #346](https://github.com/hyperme-sh/visor/issues/346)
and [Conductor #114](https://github.com/hyperme-sh/conductor/issues/114).
