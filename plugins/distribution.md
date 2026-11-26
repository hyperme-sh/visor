# Publish and manage a Visor plugin

Publish the same [plugin manifest and executable](README.md) for every publisher.
A signed publication binds the plugin ID, version, HTTPS source, and exact
manifest digest to the publisher's Ed25519 key. The manifest binds the
executable digest and declares the requested tools.

## Publish from your repository

Build your Linux executable and place it beside `manifest.json` as `plugin`.
Set `executable_sha256` in the manifest from those exact bytes. Choose a
version-specific URL on a static host you control. A repository can link to
that URL or serve the artifact directly from a commit-specific raw file.
Visor accepts canonical public HTTPS URLs that serve the file directly.
Keep the path unchanged after publication.

Create a dedicated publisher key once and keep it in your signing environment.

```sh
umask 077
openssl genpkey -algorithm Ed25519 -outform DER -out publisher.der
cargo build --locked --bin hypermesh-plugin-publish
target/debug/hypermesh-plugin-publish ./package \
  https://packages.example.org/plugin-1.2.0.visor-plugin \
  < publisher.der > plugin-1.2.0.visor-plugin
```

Use an owner-only package directory and files. The command writes the
publication to stdout and prints its public key and digest to stderr. Upload
the artifact to the declared URL and share its URL and `publication_sha256`. Each package version names immutable bytes. Use a new
version for a changed artifact. For an offline handoff, the installer also accepts
an absolute path to a private copy of the exact artifact.

The [publication schema](publication-v1.json) describes its JSON envelope and
decoded statement. The Ed25519 signature signs the literal bytes
`visor-plugin-publication-v1` followed by a NUL byte and the decoded statement
bytes. The fields use standard padded base64. The whole artifact SHA-256,
including the signature and envelope bytes, is the digest a user confirms.
There is a 12 MiB publication limit and an 8 MiB executable limit.

## Verify an app owner

A community publisher uses a signing key and can publish without a domain
proof. An app owner controls the HTTPS origin for its reverse-domain plugin
namespace. For example, `example.org` can authorize keys for IDs starting
with `org.example.`. Pass `example.org` after the source URL when signing and
serve this JSON at `https://example.org/.well-known/visor-plugin-publisher.json`.

```json
{"version":1,"keys":["PUBLISHER_KEY_FROM_PUBLISH_SCRIPT"]}
```

Visor checks the origin over HTTPS and requires the exact signing key in its
current document. An unavailable authority, unrelated domain, or different key
fails verification. The `sh.hyperme.` first-party namespace requires the
`hyperme.sh` authority. All verified namespaces use the same package,
verification, and execution path. An external software brand can direct users
to its authorized domain from its official site.

The installer reports `community`, `app_owner_verified`, or `first_party`,
the verified domain when present, and the key's SHA-256 fingerprint.
`scan_status` is an independent field and currently reports `not_scanned`.
The installed record retains the verification at install or update time.
Install and update recheck the proof. Existing installations stay bound to
their verified key and namespace. For a key or publisher change, review the
new publisher and explicitly remove and reinstall the package.

## Inspect, install, and update

On Linux use the existing owner-only Visor companion socket. The companion
CLI prints the metadata as JSON so you can inspect it or use it in a local
catalog client. Inspect before copying the digest and revision into a
mutating command.

```sh
hypermesh-companion --plugins inspect https://packages.example.org/plugin-1.2.0.visor-plugin
hypermesh-companion --plugins install https://packages.example.org/plugin-1.2.0.visor-plugin PUBLICATION_SHA256
hypermesh-companion --plugins list
hypermesh-companion --plugins inspect https://packages.example.org/plugin-1.3.0.visor-plugin
hypermesh-companion --plugins update https://packages.example.org/plugin-1.3.0.visor-plugin PUBLICATION_SHA256 INSTALLED_REVISION
```

Inspect shows requested tools and highlights new or changed tool declarations.
The install request binds the reviewed publication hash and the installed
revision so changed downloads and stale confirmations fail. A successful update
revokes the old grants. Give the selected tool and exact workspace file a fresh
session grant for the new revision with
`hypermesh-companion --plugins grant SESSION ID INSTALLED_REVISION TOOL TARGET`.
The [existing plugin call](README.md#enable-invoke-and-revoke-from-visor)
still uses Visor's workspace read admission and sandbox.

Use `--plugins disable ID INSTALLED_REVISION` to keep the package and revoke
its grants, or `--plugins remove ID INSTALLED_REVISION` to delete the installed
copy and revoke its grants. Running calls are cancelled. Re-enabling the
install with `--plugins enable ID INSTALLED_REVISION` makes it eligible for
a new grant. List again for the new revision after a state change.

## Catalog and owner API

All endpoints use the companion socket. The publication response follows
the [catalog metadata schema](publication-v1.json) with `metadata_version: 1`.
The same fields are available to the s3r.ch catalog. A hosted catalog can verify an artifact from its public source and
publisher authority, then display the signed ID, version, source and manifest
digest and the derived publisher classification and key fingerprint. Installation
and session grants stay with Visor.

| Request | Body and response |
|---|---|
| `POST /plugins/inspect` | Send `{"source":"https://…"}`. Returns `publication`, `installed` and `added_or_changed_tools`. |
| `POST /plugins` | Send `source`, `publication_sha256`, and `expected_revision` from the preview. Use null revision for a new installation. Returns the new installed record. |
| `GET /plugins` | Returns `{"plugins":[…]}` containing installed records. Records have `record_version`, `revision`, `enabled`, and `metadata`. |
| `PUT /plugins/{id}` | Send `expected_revision` and `enabled`. Returns the current record. |
| `DELETE /plugins/{id}` | Send `expected_revision`. Returns 204. |
| `POST /session/{session}/plugins` | For an installed package, send `plugin_id`, `revision`, `tool`, and `target` to grant that exact version a tool in the session. |

Package mutations are serialized in the private store. A single Visor process
owns that store while running. A stale digest or revision yields
`publication_changed`. After an interrupted request, list the current
records before retrying. The package store belongs to the signed-in OS user.
