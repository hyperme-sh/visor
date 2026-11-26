# Open-source alternatives catalog

Run `hypermesh-visor catalog list` to see alternatives for code and task
tracking, team chat, files and office work, and local AI. Use
`hypermesh-visor catalog show ID` for one entry. Both commands return JSON.
The [catalog snapshot](../src/tool_catalog/catalog.json) records the reviewed
upstream release, software license, self-hosting or local execution options,
and platforms for the linked install route.

The catalog shows which tools are installable from Visor and which use their
upstream installers. The redistribution field distinguishes included source,
permissive source eligible for packaging subject to its notices and dependency
licenses, and tools kept as separate installs under the default bundle policy.
Local AI entries track model-weight licensing separately.

### Install the local CPU runner

The managed entry builds a pinned MIT-licensed [llama2.c revision with Visor hardening](../src/tool_catalog/llama2c/NOTICE.md)
on the selected Linux machine. It supports x86-64 and ARM64 with a system C
compiler, static libc development files, and a functioning Bubblewrap sandbox.
Use the unprivileged account that owns the Visor configuration on that machine.

```sh
hypermesh-visor catalog plan llama2-c
hypermesh-visor catalog install llama2-c MACHINE_ID PLAN_SHA256
hypermesh-visor catalog health llama2-c
```

The plan reports the exact settings directory, source revision and digest,
machine ID, OS, architecture, and whether the compiler and launcher are
present. Copy the machine ID and plan digest from the output. The approval is
bound to the current Linux boot, mount namespace, and configuration path.
Generate a fresh plan on the target after a reboot or workspace change.

Install checks the source hash, builds in a private staging directory, and
runs the actual binary against a tiny synthetic checkpoint before committing the
installation. The health command verifies the installed binary digest and
repeats the sandboxed probe. A healthy probe generates two expected tokens.
The probe uses repository-generated weights, runs without network access,
and accepts no prompt or model input from a caller. Visor's model requests
continue through the configured Conductor and AIuditor route.

Install prints the executable path. To use the standalone runner with a
compatible checkpoint, keep the checkpoint and tokenizer in your own storage
and review their separate terms. The runner accepts those files
with `RUNNER CHECKPOINT -z TOKENIZER`.

### Remove an installation

```sh
hypermesh-visor catalog remove llama2-c MACHINE_ID INSTALLATION_ID
```

Get the current installation ID from install or health output. If health is
unavailable after a local binary change, use the previously reported ID or
read `installation.json` from the planned runner directory. Remove deletes
the binary, its record, and its license. Keep models and other user data in
a separate directory. The command refuses to remove a directory that
contains additional files.

### Contribute an alternative

Submit a change to the catalog with the upstream source, a reviewed version
and license, a reproducible install route, the applicable platforms, and the
redistribution assessment. App-owner, community, and AI-assisted submissions
use the same provenance and maintenance requirements. For a managed
installation, include package integrity, failure recovery, and data-preserving
removal contracts in CI. Use the [public plugin distribution contract](../plugins/distribution.md)
for packages that implement Visor tool capabilities.
