use super::USAGE;
use crate::plugins::stdio::{check_sandbox, sandbox_command};
use hypermesh_local_storage as storage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

const ID: &str = "llama2-c";
const REVISION: &str = "350e04fe35433e6d2941dce5a1f53308f87058eb";
const SOURCE: &[u8] = include_bytes!("llama2c/run.c");
const LICENSE: &[u8] = include_bytes!("llama2c/LICENSE");
const SOURCE_HASH: &str = "2e090e3b853dda20b2806cdf1f7070c9f19b533e68138c1f5024ab92c98d8af3";
const COMPILER: &str = "/usr/bin/cc";
const MANAGED: [&str; 3] = ["llama2-c", "installation.json", "LICENSE"];
const STORAGE: &str = "Could not access the private catalog store";
const PACKAGE: &str = "Catalog package integrity check failed";
const PREREQUISITES: &str =
    "Install requires a system C compiler with static libc and a working Bubblewrap sandbox";
const MACHINE: &str = "The catalog request does not match the current machine and plan";
const PROBE: &str = "The local runner could not generate the expected synthetic tokens";
const MAX_RUNNER: u64 = 8 * 1024 * 1024;

#[derive(Serialize)]
struct Machine {
    id: String,
    hostname: String,
    os: &'static str,
    architecture: &'static str,
}

impl Machine {
    fn current() -> Result<Self, &'static str> {
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|_| MACHINE)?;
        let boot = Uuid::parse_str(boot.trim()).map_err(|_| MACHINE)?;
        let hostname = fs::read_to_string("/proc/sys/kernel/hostname").map_err(|_| MACHINE)?;
        let hostname: String = hostname
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect();
        let mount = fs::metadata("/proc/self/ns/mnt").map_err(|_| MACHINE)?;
        let mut material = boot.as_bytes().to_vec();
        material.extend_from_slice(&mount.dev().to_le_bytes());
        material.extend_from_slice(&mount.ino().to_le_bytes());
        let id = format!("linux-{}", digest(&material));
        Ok(Self {
            id,
            hostname,
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
        })
    }

    fn check(&self) -> Result<(), &'static str> {
        if self.os != "linux"
            || !matches!(self.architecture, "x86_64" | "aarch64")
            || cfg!(target_endian = "big")
        {
            return Err("The local catalog runner supports Linux x86_64 and aarch64");
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Plan<'a> {
    plan_version: u32,
    tool: &'static str,
    machine: &'a Machine,
    directory: &'a Path,
    source_revision: &'static str,
    source_sha256: &'static str,
    software_license: &'static str,
    model_weights: &'static str,
    local_only: bool,
    compiler_available: bool,
    sandbox_available: bool,
}

impl Plan<'_> {
    fn hash(&self) -> Result<String, &'static str> {
        Ok(digest(&serde_json::to_vec(self).map_err(|_| MACHINE)?))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Installation {
    version: u32,
    id: Uuid,
    architecture: String,
    source_revision: String,
    source_sha256: String,
    binary_sha256: String,
}

struct Store {
    root: PathBuf,
}

impl Store {
    fn new(config: &Path) -> Result<Self, &'static str> {
        if !config.is_absolute() {
            return Err(STORAGE);
        }
        Ok(Self {
            root: config.join("tool-catalog"),
        })
    }

    fn lock(&self) -> Result<fs::File, &'static str> {
        fs::create_dir_all(self.root.parent().ok_or(STORAGE)?).map_err(|_| STORAGE)?;
        storage::create_dir(&self.root).map_err(|_| STORAGE)?;
        let file =
            storage::open(&self.root.join("lock"), storage::Open::Lock).map_err(|_| STORAGE)?;
        storage::lock(&file).map_err(|_| "Another catalog operation is using this store")?;
        Ok(file)
    }

    fn install_dir(&self) -> PathBuf {
        self.root.join(ID)
    }

    fn install(
        &self,
        machine: &Machine,
        build: impl FnOnce(&Path) -> Result<(), &'static str>,
    ) -> Result<Installation, &'static str> {
        let _lock = self.lock()?;
        let target = self.install_dir();
        if let Some(installation) = self.record(machine)? {
            if installation.source_revision != REVISION || installation.source_sha256 != SOURCE_HASH
            {
                return Err(
                    "Remove the previous installation before installing this source revision",
                );
            }
            checked_runner(&target, &installation)?;
            return Ok(installation);
        }
        let staged = Stage::new(&self.root)?;
        build(&staged.0)?;
        let binary = read_private(&staged.0.join(ID), MAX_RUNNER)?;
        let installation = Installation {
            version: 1,
            id: Uuid::new_v4(),
            architecture: machine.architecture.into(),
            source_revision: REVISION.into(),
            source_sha256: SOURCE_HASH.into(),
            binary_sha256: digest(&binary),
        };
        write_private(&staged.0.join("LICENSE"), LICENSE)?;
        write_private(
            &staged.0.join("installation.json"),
            &serde_json::to_vec(&installation).map_err(|_| STORAGE)?,
        )?;
        storage::sync_dir(&staged.0).map_err(|_| STORAGE)?;
        fs::rename(&staged.0, &target).map_err(|_| STORAGE)?;
        storage::sync_dir(&self.root).map_err(|_| STORAGE)?;
        Ok(installation)
    }

    fn record(&self, machine: &Machine) -> Result<Option<Installation>, &'static str> {
        let directory = self.install_dir();
        match fs::symlink_metadata(&directory) {
            Ok(_) => storage::check_dir(&directory).map_err(|_| STORAGE)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(STORAGE),
        }
        let raw = read_private(&directory.join("installation.json"), 4096)?;
        let record: Installation = serde_json::from_slice(&raw).map_err(|_| PACKAGE)?;
        if record.version != 1 || record.id.is_nil() || record.architecture != machine.architecture
        {
            return Err(PACKAGE);
        }
        Ok(Some(record))
    }

    fn health(&self, machine: &Machine) -> Result<Value, &'static str> {
        let _lock = self.lock()?;
        let Some(installation) = self.record(machine)? else {
            return Ok(json!({"tool":ID,"machine":machine,"state":"not_installed"}));
        };
        let bytes = checked_runner(&self.install_dir(), &installation)?;
        let scratch = Stage::new(&self.root)?;
        let executable = scratch.0.join(ID);
        write_private(&executable, &bytes)?;
        executable_mode(&executable)?;
        probe(&executable, &scratch.0)?;
        Ok(json!({
            "tool":ID, "machine":machine, "state":"healthy", "installation":installation,
            "probe":"synthetic_checkpoint", "generated_tokens":2, "local_only":true
        }))
    }

    fn remove(&self, machine: &Machine, installation: Uuid) -> Result<(), &'static str> {
        let _lock = self.lock()?;
        let record = self
            .record(machine)?
            .ok_or("The catalog runner is not installed")?;
        if record.id != installation {
            return Err("The installation changed. Check its health before removing it.");
        }
        let target = self.install_dir();
        for entry in fs::read_dir(&target).map_err(|_| STORAGE)? {
            let name = entry.map_err(|_| STORAGE)?.file_name();
            if !name.to_str().is_some_and(|name| MANAGED.contains(&name)) {
                return Err(
                    "The installation contains other files. Move them before removing the runner.",
                );
            }
        }
        // Rename the active directory first. Cleanup only touches the explicit managed files.
        let tombstone = self.root.join(format!(".remove-{}", Uuid::new_v4()));
        fs::rename(&target, &tombstone).map_err(|_| STORAGE)?;
        storage::sync_dir(&self.root).map_err(|_| STORAGE)?;
        for name in MANAGED {
            match fs::remove_file(tombstone.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(STORAGE),
            }
        }
        fs::remove_dir(tombstone).map_err(|_| STORAGE)?;
        storage::sync_dir(&self.root).map_err(|_| STORAGE)
    }
}

struct Stage(PathBuf);
impl Stage {
    fn new(root: &Path) -> Result<Self, &'static str> {
        let path = root.join(format!(".stage-{}", Uuid::new_v4()));
        if !storage::create_dir(&path).map_err(|_| STORAGE)? {
            return Err(STORAGE);
        }
        Ok(Self(path))
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        // Only tool-generated scratch directories use Stage.
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn manage(
    command: &str,
    id: &str,
    args: &[String],
    config: &Path,
) -> Result<Value, &'static str> {
    if id != ID {
        return Err("No managed installer is available for this tool");
    }
    if command != "plan" && unsafe { libc::geteuid() } == 0 {
        return Err("Run catalog operations as the owner of the selected Linux workspace");
    }
    let machine = Machine::current()?;
    machine.check()?;
    let store = Store::new(config)?;
    let compiler = trusted_compiler();
    let plan = Plan {
        plan_version: 1,
        tool: ID,
        machine: &machine,
        directory: &store.root,
        source_revision: REVISION,
        source_sha256: SOURCE_HASH,
        software_license: "MIT",
        model_weights: "user_supplied_with_separate_license",
        local_only: true,
        compiler_available: compiler.is_ok(),
        sandbox_available: check_sandbox().is_ok(),
    };
    match (command, args) {
        ("plan", []) => Ok(json!({"plan":plan,"plan_sha256":plan.hash()?})),
        ("install", [target, hash]) => {
            if *target != machine.id || *hash != plan.hash()? {
                return Err(MACHINE);
            }
            if !plan.sandbox_available {
                return Err(PREREQUISITES);
            }
            let compiler = compiler?;
            let installation =
                store.install(&machine, |stage| build_and_probe(&compiler, stage))?;
            Ok(
                json!({"tool":ID,"machine":machine,"installation":installation,"executable":store.install_dir().join(ID)}),
            )
        }
        ("health", []) => store.health(&machine),
        ("remove", [target, installation]) => {
            if *target != machine.id {
                return Err(MACHINE);
            }
            store.remove(
                &machine,
                Uuid::parse_str(installation).map_err(|_| "Invalid installation ID")?,
            )?;
            Ok(json!({"tool":ID,"machine":machine,"state":"removed"}))
        }
        _ => Err(USAGE),
    }
}

fn trusted_compiler() -> Result<PathBuf, &'static str> {
    let path = fs::canonicalize(COMPILER).map_err(|_| PREREQUISITES)?;
    let info = fs::symlink_metadata(&path).map_err(|_| PREREQUISITES)?;
    if !path.starts_with("/usr/bin")
        || !info.is_file()
        || info.uid() != 0
        || info.mode() & 0o6022 != 0
    {
        return Err(PREREQUISITES);
    }
    Ok(path)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_source(bytes: &[u8]) -> Result<(), &'static str> {
    if digest(bytes) != SOURCE_HASH {
        return Err(PACKAGE);
    }
    Ok(())
}

fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>, &'static str> {
    let file = storage::open(path, storage::Open::Read { private: true }).map_err(|_| PACKAGE)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PACKAGE)?;
    if bytes.is_empty() || bytes.len() as u64 > limit {
        return Err(PACKAGE);
    }
    Ok(bytes)
}

fn checked_runner(directory: &Path, installation: &Installation) -> Result<Vec<u8>, &'static str> {
    let bytes = read_private(&directory.join(ID), MAX_RUNNER)?;
    if !bytes.starts_with(b"\x7fELF") || digest(&bytes) != installation.binary_sha256 {
        return Err(PACKAGE);
    }
    Ok(bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), &'static str> {
    let mut file = storage::open(path, storage::Open::New).map_err(|_| STORAGE)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| STORAGE)
}

fn executable_mode(path: &Path) -> Result<(), &'static str> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o500)).map_err(|_| STORAGE)
}

fn build_and_probe(compiler: &Path, stage: &Path) -> Result<(), &'static str> {
    verify_source(SOURCE)?;
    let source = stage.join("run.c");
    let executable = stage.join(ID);
    write_private(&source, SOURCE)?;
    let mut cc = Command::new(compiler);
    cc.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir(stage)
        .stderr(Stdio::inherit())
        .args([
            "-std=c11",
            "-D_POSIX_C_SOURCE=200809L",
            "-O2",
            "-static",
            "-o",
            ID,
            "run.c",
            "-lm",
        ]);
    execute(cc, Duration::from_secs(45), PREREQUISITES)?;
    executable_mode(&executable)?;
    probe(&executable, stage)?;
    fs::remove_file(source).map_err(|_| STORAGE)?;
    // The installed directory contains only the binary, its record and the upstream license.
    for name in ["synthetic.bin", "synthetic-tokenizer.bin"] {
        fs::remove_file(stage.join(name)).map_err(|_| STORAGE)?;
    }
    Ok(())
}

fn probe(executable: &Path, stage: &Path) -> Result<(), &'static str> {
    check_sandbox().map_err(|_| PREREQUISITES)?;
    let checkpoint = stage.join("synthetic.bin");
    let tokenizer = stage.join("synthetic-tokenizer.bin");
    let (weights, tokens) = synthetic_model();
    write_private(&checkpoint, &weights)?;
    write_private(&tokenizer, &tokens)?;
    let cmd = sandbox_command(
        executable,
        &[(&checkpoint, "/model.bin"), (&tokenizer, "/tokenizer.bin")],
        &[
            "/model.bin",
            "-z",
            "/tokenizer.bin",
            "-t",
            "0",
            "-n",
            "2",
            "-s",
            "1",
        ],
    );
    let output = execute(cmd, Duration::from_secs(8), PROBE)?;
    if output != b"visor-okvisor-ok\n" {
        return Err(PROBE);
    }
    Ok(())
}

fn execute(
    mut command: Command,
    deadline: Duration,
    failed: &'static str,
) -> Result<Vec<u8>, &'static str> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| failed)?;
    runtime.block_on(async {
        tokio::time::timeout(deadline, async {
            let mut child = command.spawn().map_err(|_| failed)?;
            let mut output = Vec::new();
            child
                .stdout
                .take()
                .ok_or(failed)?
                .take(4097)
                .read_to_end(&mut output)
                .await
                .map_err(|_| failed)?;
            if output.len() > 4096 || !child.wait().await.map_err(|_| failed)?.success() {
                return Err(failed);
            }
            Ok(output)
        })
        .await
        .map_err(|_| failed)?
    })
}

fn synthetic_model() -> (Vec<u8>, Vec<u8>) {
    // One layer, one attention head and a four-token vocabulary.
    let mut checkpoint: Vec<u8> = [4i32, 4, 1, 1, 1, 4, 4]
        .into_iter()
        .flat_map(i32::to_le_bytes)
        .collect();
    let mut tensors = [0f32; 156];
    // Tied embedding/classifier. BOS scores token 3 highest and token 3 repeats.
    tensors[4] = 1.0;
    tensors[12] = 2.0;
    // Skip embeddings, attention RMS and four matrices, then feed-forward RMS and three matrices.
    tensors[136..140].fill(1.0);
    checkpoint.extend(tensors.into_iter().flat_map(f32::to_le_bytes));
    let mut tokenizer = 8i32.to_le_bytes().to_vec();
    for token in [b"<unk>".as_slice(), b"<s>", b"</s>", b"visor-ok"] {
        tokenizer.extend_from_slice(&0f32.to_le_bytes());
        tokenizer.extend_from_slice(&(token.len() as i32).to_le_bytes());
        tokenizer.extend_from_slice(token);
    }
    (checkpoint, tokenizer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Machine {
        Machine {
            id: "fixture-machine".into(),
            hostname: "fixture".into(),
            os: "linux",
            architecture: "x86_64",
        }
    }

    #[test]
    fn supplied_platform_and_source_digest_are_checked() {
        let mut machine = target();
        assert!(machine.check().is_ok());
        machine.architecture = "riscv64";
        assert!(machine.check().is_err());
        machine = target();
        machine.os = "windows";
        assert!(machine.check().is_err());
        assert!(verify_source(SOURCE).is_ok());
        assert_eq!(verify_source(b"changed"), Err(PACKAGE));
    }

    #[test]
    fn failed_install_can_retry_and_removal_preserves_owner_files() {
        let config = Stage::new(&std::env::temp_dir()).unwrap();
        let store = Store::new(&config.0).unwrap();
        let machine = target();
        assert!(matches!(
            store.install(&machine, |stage| {
                write_private(&stage.join(ID), b"partial")?;
                Err(PROBE)
            }),
            Err(PROBE)
        ));
        assert!(store.record(&machine).unwrap().is_none());
        assert!(fs::read_dir(&store.root)
            .unwrap()
            .all(|item| item.unwrap().file_name() == "lock"));

        let expected = b"\x7fELFfixture";
        let installed = store
            .install(&machine, |stage| write_private(&stage.join(ID), expected))
            .unwrap();
        assert_eq!(
            checked_runner(&store.install_dir(), &installed).unwrap(),
            expected
        );
        let retry = store
            .install(&machine, |_| panic!("installed artifact was rebuilt"))
            .unwrap();
        assert_eq!(installed.id, retry.id);

        let user_file = store.install_dir().join("owner-model");
        write_private(&user_file, b"owner data").unwrap();
        assert!(store.remove(&machine, installed.id).is_err());
        assert_eq!(fs::read(&user_file).unwrap(), b"owner data");
        let data = config.0.join("models.bin");
        fs::rename(user_file, &data).unwrap();
        assert!(store.remove(&machine, Uuid::new_v4()).is_err());
        store.remove(&machine, installed.id).unwrap();
        assert_eq!(fs::read(data).unwrap(), b"owner data");
        assert!(store.record(&machine).unwrap().is_none());
    }
}
