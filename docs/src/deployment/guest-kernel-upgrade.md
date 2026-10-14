# Guest Kernel Upgrade

Each sandbox boots a Firecracker microVM with the kernel that setup places on the host. This page covers where that kernel comes from, how this fork's kernel differs from the GitHub-hosted runner kernel, how to move to the 6.18 LTS, and how to rebuild and republish the golden snapshot so that new sandboxes boot the new kernel.

> [!WARNING]
> A kernel change applies to new cold boots. Snapshots and paused sandboxes restore the kernel that was running when they were captured (Firecracker semantics; see [Why a cold boot is required](#why-a-cold-boot-is-required)). Keep the previous kernel, Firecracker binary, and golden until the new ones pass every check in the [Verification checklist](#verification-checklist).

## What the guest kernel is

### Where it comes from

Setup provides one uncompressed `vmlinux.bin` per kernel version. The packaged default is the `[kernel.kvm]` entry in `config/deps_manifest.toml`. PVM mode uses `[kernel.pvm]` instead (see [PVM Deployment](./pvm.md)).

```toml
[kernel.kvm]
version = "vmlinux-6.1.175"
url = "https://github.com/kvcache-ai/firecracker/releases/download/aenv-deps/{version}-{arch}"
```

Setup resolves the file in this order:

1. `[kernel] image_path`, if set. Setup skips the download and requires a readable, non-empty regular file.
2. `<deps_path>/kernel/<version>/vmlinux.bin`, where `<version>` is `[kernel] version` when set and the manifest version otherwise.
3. Setup downloads from the `url` template only when that path is missing or empty. A non-empty file already in place is used as is.

With the packaged manifest, the kernel is `<deps_path>/kernel/vmlinux-6.1.175/vmlinux.bin`. `deps_path` defaults to `$AENV_HOME/deps`, and `AENV_DEPS_PATH` overrides it. See the [`[kernel]` reference](../configuration/reference.md#kernel) and the [environment variables](../configuration/env-vars.md). Each sandbox passes this path to Firecracker as its boot source, and sandbox creation fails if the file is missing (`src/cfg.rs` `resolved_kernel_image_path`, `src/setup/deps.rs` `ensure_kernel`, `src/sandbox/firecracker/config.rs`).

### Why the rootfs's own kernel is never booted

A sandbox's root filesystem is an OCI image. A rootfs copied from the GitHub-hosted runner image contains `linux-azure` 6.17 packages and `/lib/modules/6.17.0-1020-azure`. AgentENV never boots them: no code under `src/` extracts a kernel from a rootfs, so the boot source is always the host's `vmlinux.bin`. As a result:

- Kernel packages and modules inside an image do not affect the running kernel. A module loads only when its version matches the running kernel, and the runner's modules are for a different kernel.
- Anything a workload needs from the kernel must be built in (`=y`). The parity delta below does that.
- `uname -r` reports the build's version string, including its local version suffix.

## Parity with GitHub-hosted runners

The target is the `linux-azure 6.17.0-1020.20~24.04.1` kernel from the `ubuntu-24.04` runner image `20260720.247.2`. Its config enables AppArmor, Yama, and Landlock, builds MINIX as a module (`CONFIG_MINIX_FS=m`), and sets `CONFIG_LSM="landlock,lockdown,yama,integrity,apparmor"`.

### Build inputs

These files live under `kernel/` and come from [preloopdev/AgentENV#3](https://github.com/preloopdev/AgentENV/pull/3). This page names them without linking to the files themselves.

| Path | Role |
|---|---|
| `kernel/configs/preloop-x86_64-6.1.175.defconfig` | `make savedefconfig` output of the final preloop3 config. Reproduce it with `cp <defconfig> .config && make olddefconfig`. |
| `kernel/configs/preloop-x86_64-6.1.fragment` | Parity delta: 45 options, the tiers below, `CONFIG_LSM`, and local version `-preloop3`. It is series-independent. |
| `kernel/configs/preloop-base-x86_64.fragment` | Settings the preloop2 build changed relative to the 6.1.175 config it was derived from (provenance unrecorded): nested KVM, netfilter/nftables, bridge, device-mapper, btrfs, DAMON reclaim/LRU sort, virtio-fs/vsock, and others. Merged only when porting to a new base. |
| `kernel/patches/6.1/0001-apparmor-restrict-unprivileged-userns-sysctl-shim.patch` | Adds the opt-in `CONFIG_USERNS_APPARMOR_SYSCTL_COMPAT` option and `kernel/userns_apparmor_compat.c`, the sysctl shim. |
| `kernel/build.sh` | Takes one argument, a full version (default `6.1.175`). It derives the series, applies `kernel/patches/<series>/*.patch`, runs `olddefconfig` from `configs/preloop-x86_64-<version>.defconfig`, checks every line of `configs/preloop-x86_64-<series>.fragment` against the final `.config`, builds `vmlinux`, and writes `vmlinux.bin` and `vmlinux.bin.sha256`. |
| `kernel/smoke-test.sh` | Boots a built kernel under a Firecracker binary with a busybox initramfs and runs the parity checks listed in the [verification checklist](#verification-checklist). |

Build prerequisites are `curl`, `xz`, `gcc`, `make`, `flex`, `bison`, `bc`, `libelf-dev`, `libssl-dev`, and `pahole` 1.22 or newer. `build.sh` writes `vmlinux.bin` with `objcopy --strip-debug`, which keeps the `.BTF` section, and it fails if the result has no `.BTF` section. After that step, `readelf -S vmlinux.bin` shows `.BTF` and `.BTF_ids`.

The provenance of the 6.1.175 config that the preloop2 build was derived from is not recorded. That starting config is neither Firecracker's microvm-kernel-ci 6.1 config nor the config of kvcache-ai's published `vmlinux-6.1.175`. The defconfig and fragments in this repository are therefore the record of what was built. The preloop3 defconfig enables `CONFIG_IKCONFIG` and `CONFIG_IKCONFIG_PROC`, so a booted build can expose its own `.config` at `/proc/config.gz`. Do not treat any other file as the upstream baseline.

### Reference build

The current preloop3 artifact:

- Kernel release: `6.1.175-preloop3`.
- `vmlinux.bin`: 86,523,640 bytes, sha256 `1a36f2bb308ed689d109641a27534c55fc3dd6b8ecd726b6f34642e7114da91d`.
- Source: the kernel.org 6.1.175 tarball, sha256 `37e984a9…beca85`, which matches kernel.org's `sha256sums.asc`.
- Boot: about 1.7 s under Firecracker v1.15.1-patch-v1 with 2 vCPUs and 2 GiB of memory.

### What the parity delta changes

| Tier | Options | Why |
|---|---|---|
| 1, evidence | `kernel.apparmor_restrict_unprivileged_userns` sysctl (shim); `CONFIG_MINIX_FS=y` | The runner's `/etc/sysctl.d/10-apparmor.conf` sets the sysctl to 1, and this kernel lacks it. A `uv` CI workflow (`test.yml`) loop-mounts a MINIX image. linux-azure builds MINIX as a module, and this guest has no matching module tree, so it is built in. |
| 2 | `CONFIG_SECURITY_YAMA=y` | The runner's `/etc/sysctl.d/10-ptrace.conf` sets `kernel.yama.ptrace_scope=1`. |
| 3, insurance | FAT, VFAT, and MSDOS with NLS 437, ISO8859-1, UTF8, and ASCII; EXFAT, NTFS3, UDF, EROFS, F2FS, HFS, HFSPLUS, JFS, NILFS2, and ZONEFS; UNIX_DIAG, PACKET_DIAG, and NETLINK_DIAG; CRYPTO_USER_API_HASH, AEAD, and RNG; SECURITY_LANDLOCK and SECURITY_SAFESETID; INPUT_UINPUT; NFSD; NET_9P and 9P_FS; BLK_DEV_RAM | Not evidence-based. Built in so they work without a matching module tree. |

Building an LSM is not enough. An LSM takes effect only when it is built in and named in `CONFIG_LSM`, the boot-time order list. The fragment sets:

```text
CONFIG_LSM="landlock,lockdown,yama,integrity,apparmor,bpf"
```

Without `yama` and `landlock` in that list, `SECURITY_YAMA` and `SECURITY_LANDLOCK` are inert. `SECURITY_SAFESETID` is built but not listed, so it is inert too. The kernel skips listed names that are not built in (`security/security.c`). The `apparmor` entry has no effect in these builds, because the shim requires `SECURITY_APPARMOR` to be off. The list is linux-azure's list plus `bpf`. On the preloop3 build, `/sys/kernel/security/lsm` reads `capability,landlock,yama,bpf`, so `apparmor`, `lockdown`, and `integrity` are not active.

#### The sysctl shim

`kernel.apparmor_restrict_unprivileged_userns` comes from an Ubuntu patch. Mainline 6.18 does not define it: `security/apparmor/lsm.c` defines `unprivileged_userns_apparmor_policy` instead, and enabling AppArmor on a mainline-based kernel does not create the Ubuntu knob. The runner image sets the knob to 1 at boot, so the guest needs the knob to exist. The shim provides it:

- `CONFIG_USERNS_APPARMOR_SYSCTL_COMPAT` is opt-in. It depends on `USER_NS && SYSCTL && !SECURITY_APPARMOR`.
- `kernel/userns_apparmor_compat.c` registers the knob with `register_sysctl_init("kernel", …)` from a `late_initcall`.
- The knob accepts 0 and 1 and enforces nothing. Its default is 0, because user namespaces are unrestricted without AppArmor.

### Verifying the option set

Kconfig does not error when a requested option's dependencies are unmet. The option is simply absent or unset in the final `.config`. On 6.1, all 45 requested options were honoured, and Kconfig auto-enabled 11 dependencies that the fragment does not list. `scripts/kconfig/merge_config.sh` runs its comparison only when it runs `make` itself. With `-m`, which the 6.18 port uses, run the check in the [Verification checklist](#verification-checklist) after `make olddefconfig`.

### Known limitation: ntfs3 writes

On 6.1.175 the `ntfs3` driver mounts NTFS volumes, but it cannot create files on a freshly `mkntfs`-formatted volume. The kernel logs `ntfs3: loop0: no free space to extend mft`, for both quick and full formats. This is a limitation of the 6.1 driver, not of the configuration. Whether linux-azure 6.17 can write to such a volume is unverified. The smoke test checks only that `ntfs3` is registered. This is one reason to do the 6.18 port.

## Upgrading to the 6.18 LTS

Firecracker's [kernel policy](https://github.com/firecracker-microvm/firecracker/blob/main/docs/kernel-policy.md) sets the minimum Firecracker version and the minimum end-of-support date for each guest kernel:

| Guest kernel | Minimum Firecracker | Minimum end of support |
|---|---|---|
| 6.1 | v1.9.0 | 2026-09-02, passed |
| 6.18 | v1.16.1 | 2028-06-01 |

### Prerequisites

1. **Firecracker v1.16.1 or later.** The pinned `1.15.1-patch-v1` is below the 6.18 minimum. The kvcache-ai/firecracker releases are `v1.15.1-patch-v2` and `aenv-deps`, with no 1.16+ build. Upstream Firecracker has v1.16.1 (2026-07-02), v1.16.2, and v1.17.0 (both 2026-09-10). Package the binary and update `config/deps_manifest.toml` as described in the [Firecracker Client and Runtime Upgrade Checklist](../internals/sandbox-testing.md#41-firecracker-client-and-runtime-upgrade-checklist). To test without changing the manifest, set `[firecracker] binary_path`.
2. **Base config.** Start from the kernel.org 6.18.z tarball and Firecracker's `resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config`. That file is generated from an Amazon Linux microVM kernel (`6.18.39-…amzn2023.x86_64.microvm`). Firecracker's policy says the configs are for its Amazon Linux microVM kernels and that upstream builds are not guaranteed to work with them. Expect fixups, and run the option check. The CI config leaves `CONFIG_BTRFS_FS` and `CONFIG_VIRTIO_FS` unset, both settings the preloop2 build changed, so merge the base fragment or those settings are lost.
3. **Host kernel (issue #20).** The ublk daemon runs on the host, so the host kernel matters even though the guest does not run ublk. [kvcache-ai/AgentENV#20](https://github.com/kvcache-ai/AgentENV/issues/20) reports that the first `UBLK_U_CMD_ADD_DEV` can trigger a NULL-pointer dereference on a host kernel that has upstream commit `529d4d632788` (ublk NUMA-aware allocation) without its prerequisite `011af85ccd87` (tag_set reorder). The command then never completes, and AgentENV cannot create sandboxes. Reported affected kernels: upstream 6.18.4 and 6.18.5, Arch `6.18.4-arch1-1`, and Ubuntu `6.17.0-1015-azure` and `6.17.0-1018-azure`. Upstream 6.18.6 and later 6.18.y releases contain the fix. The 6.18 guest config leaves `CONFIG_BLK_DEV_UBLK` unset.

### Port the series

Run these steps once. Afterward, `kernel/build.sh 6.18.<z>` works unchanged.

1. **Port the shim.** Write the 6.18 version as `kernel/patches/6.18/0001-apparmor-restrict-unprivileged-userns-sysctl-shim.patch`, and keep the knob in its own file, as the 6.1 patch does. Two 6.18 details apply:
   - Register the table with `register_sysctl_init("kernel", table)`. On 6.18 that macro expands to `__register_sysctl_init(path, table, #table, ARRAY_SIZE(table))` (`include/linux/sysctl.h`), and the table is iterated by its size (`fs/proc/proc_sysctl.c`). The table therefore needs no empty `{ }` sentinel entry.
   - `__register_sysctl_init` takes a `const struct ctl_table *`, so declare the table `static const`.

   Also confirm that its dependencies still hold: `USER_NS` and `SYSCTL` enabled, and `SECURITY_APPARMOR` disabled, as in the CI config.
2. **Extract and patch.** Extract the 6.18.z tarball and apply `kernel/patches/6.18/*.patch` in order.
3. **Set the base config.** Copy the Firecracker CI config to `.config`.
4. **Merge both fragments.** The base fragment carries the settings the preloop2 build changed, and the parity fragment carries the runner-parity delta:

   ```bash
   scripts/kconfig/merge_config.sh -m .config \
     /path/to/AgentENV/kernel/configs/preloop-base-x86_64.fragment \
     /path/to/AgentENV/kernel/configs/preloop-x86_64-6.1.fragment
   make olddefconfig
   ```

   Then run the option check from the [Verification checklist](#verification-checklist) against both fragments. Confirm `CONFIG_DAMON_RECLAIM=y` in the final `.config`. The shipped boot args set `damon_reclaim.*` parameters (`config/default.toml`, and `DEFAULT_BOOT_ARGS` in `src/sandbox/firecracker/config.rs`); without the option, no `damon_reclaim` module receives them. DAMON reclaim is compiled into the upstream 6.1.175 binary too, so the option is not specific to the preloop build. Under Firecracker v1.15.1-patch-v1, both that binary and preloop3 read `Y` from `/sys/module/damon_reclaim/parameters/enabled` when `damon_reclaim.enabled=Y` is on the command line. The shipped `config/default.toml` line also passes `mitigations=off`, which `DEFAULT_BOOT_ARGS` does not.
5. **Build and check BTF.** Build `vmlinux` and write `vmlinux.bin` with `objcopy --strip-debug`, the same steps `kernel/build.sh` runs. `readelf -S vmlinux.bin` shows `.BTF` and `.BTF_ids`.
6. **Commit the inputs.** Run `make savedefconfig` and commit the result as `kernel/configs/preloop-x86_64-6.18.<z>.defconfig`. Commit the 6.18 patches and a copy of the parity fragment as `kernel/configs/preloop-x86_64-6.18.fragment`, which `kernel/build.sh` checks for that series.
7. **Install and switch.** Continue with [Install the new kernel next to the old one](#install-the-new-kernel-next-to-the-old-one).

### Verification checklist

Check 1 needs a host with `/dev/kvm` and no sandbox. Check 2 runs in the kernel tree. Checks 8 and 10 run on the host. Checks 3, 4, 6, and 7 run in the golden, which is read-only. Check 5 writes a sysctl, so run it in a throwaway sandbox, because anything changed in the golden is captured in its snapshot. Check 9 runs in a clone of the new golden.

1. **Automated smoke test.** Run `kernel/smoke-test.sh <vmlinux.bin> <firecracker>`. It boots the kernel under the given Firecracker binary with a busybox initramfs and runs 24 checks:
   - `kernel.apparmor_restrict_unprivileged_userns` defaults to 0, accepts writes of 1 and 0, and rejects 2 without changing its value.
   - `kernel.yama.ptrace_scope` is 1.
   - A process running as uid 1001 can create a user namespace.
   - Loop-mount and write succeed on minix, vfat, exfat, and btrfs.
   - ntfs3, udf, erofs, f2fs, hfs, hfsplus, jfs, nilfs2, zonefs, 9p, nfsd, overlay, and fuse are registered as filesystems.
   - `/proc/net/unix` exists (the script names this check `unix_diag`).

   The script also prints `uname -r`, the active LSM list, and the `damon_reclaim` parameter as information lines. It exits non-zero if any check fails.

   The host needs `/dev/kvm`, a static busybox (given by `BUSYBOX`, or found on `PATH`), gzip, and `mkfs.minix`, `mkfs.vfat`, `mkfs.exfat`, and `mkfs.btrfs`. Reported results: preloop3 passes 24 and fails 0; the upstream 6.1.175 binary passes 4 and fails 20. These results come from the kernel build runs, not from this page's checks.
2. **Options.** Every requested fragment line appears unchanged in the final `.config`:

   ```bash
   AGENTENV=/path/to/AgentENV
   for fragment in "$AGENTENV"/kernel/configs/preloop-base-x86_64.fragment \
                   "$AGENTENV"/kernel/configs/preloop-x86_64-6.18.fragment; do
     grep -E '^(CONFIG_[A-Za-z0-9_]+=|# CONFIG_[A-Za-z0-9_]+ is not set)' "$fragment" |
       while IFS= read -r line; do
         grep -qxF -- "$line" .config || echo "not honoured: $line"
       done
   done
   ```

   For a 6.1 build, check only `preloop-x86_64-6.1.fragment`, because the base fragment is for ports to a new base. Any `not honoured` line blocks the build. Dependencies that Kconfig enables on its own do not appear in the fragments and are expected.
3. **Local version.** `uname -r` ends with the fragment's local version (`-preloop3` on the 6.1 build). Use this, not the snapshot record, to identify the running kernel.
4. **LSMs.** With securityfs mounted at `/sys/kernel/security`, `cat /sys/kernel/security/lsm` reads `capability,landlock,yama,bpf` on preloop3. The upstream 6.1.175 binary reads `capability,selinux`, because upstream activates SELinux and Ubuntu does not. If an expected LSM is missing, add `lsm.debug` to the boot arguments, boot once, and run `dmesg | grep 'not built'`. The kernel logs listed names that are not built in.
5. **Sysctl.** `sysctl kernel.apparmor_restrict_unprivileged_userns` exists. `sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` succeeds and reads back `0`. Writing `1` also succeeds, and writing `2` is rejected.
6. **Yama.** `sysctl kernel.yama.ptrace_scope` exists.
7. **DAMON reclaim.** `/proc/cmdline` contains `damon_reclaim.enabled=Y`, and `/sys/module/damon_reclaim/parameters/enabled` reads `Y`. Under Firecracker v1.15.1-patch-v1, both preloop3 and the upstream 6.1.175 binary read `Y`.
8. **Cold start.** `aenv start --cold <base image>` reaches the ready state.
9. **Known-failing workflow.** In a clone of the new golden, run the workflow that motivated the parity delta, such as the `uv` `test.yml` job that loop-mounts a MINIX image. It must pass.
10. **Sandbox checklist.** Run the [end-to-end execution checklist](../internals/sandbox-testing.md#4-end-to-end-execution-checklist).

## Rebuilding and publishing the golden

In this page, the golden is a cold-booted sandbox that you snapshot and then clone from.

### Install the new kernel next to the old one

Choose one way to make the server use the new build. In every case, keep the old kernel in place for rollback.

- **Versioned directory (recommended for a local build).** Place the build at `<deps_path>/kernel/<new-version>/vmlinux.bin`, for example `vmlinux-6.18.<z>`, which matches the manifest's naming. Leave `<deps_path>/kernel/vmlinux-6.1.175/` in place. Set `[kernel] version = "<new-version>"`, and leave `[kernel] url` unset. The file is present, so setup does not download.
- **Explicit file.** Set `[kernel] image_path` to the build.
- **Manifest.** After the artifact is published, update the `[kernel.kvm]` entry in `config/deps_manifest.toml` (`version` and `url`).

Then:

1. Point `[firecracker]` at the 1.16.1-or-later binary, through `binary_path` or the updated manifest pin. The Firecracker binary is part of the switch.
2. Run `server --config <file> --setup-only` and confirm that every asset resolves.
3. Restart the server. Configuration is read once per process (`src/cfg.rs`, `ConfigManager`).

To roll back, revert the kernel override (or set `[kernel] version` back to `vmlinux-6.1.175`) and revert `[firecracker]` to the previous binary in the same change. Firecracker checks the snapshot data-format version when it loads a snapshot, and it documents that snapshots must be resumed on an identical software configuration, with limited exceptions ([Firecracker snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md#snapshot-versioning)). AgentENV records `firecracker_version` in snapshot metadata but does not compare it, so pair each golden with the binary that captured it. Old kernel directories stay on disk until you delete them ([Persistence Artifact Inventory](../internals/persistence-artifact-inventory.md)).

### Why a cold boot is required

- A snapshot stores the guest's memory and device state, and restoring it resumes the original guest workload ([Firecracker snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)). A restored golden keeps the kernel that was in memory at capture. Only a cold boot loads the new `vmlinux.bin`. [INFERENCE from Firecracker semantics; AgentENV does not test kernel swaps under restore.]
- Firecracker advises taking snapshots after the guest kernel has booted, because a snapshot taken during early boot can crash on resume. Snapshot once the sandbox is ready.
- The snapshot record stores `kernel_version`, but no code compares it. Its value depends on how the snapshot was made. Template builds store a `uname -r` probe (`src/template/runner.rs`, `src/snapshot/types/version.rs`). Sandboxes started with `aenv start --cold` store `[kernel] version`, or `unknown` when it is unset (`src/orchestrator/service.rs`, `configured_runtime_versions`), and their snapshots carry that value. Check the kernel with `uname -r` in the guest.

### Cold-start, snapshot, and publish

Publishing goes to the backend selected by `[snapshot] repository_backend`. For R2, set it to `oss` and configure [`[backend.oss]`](../configuration/reference.md#backendoss) with the R2 `endpoint`, `bucket`, `region`, and credentials. R2 is S3-compatible, and some R2 deployments need `addressing_style = "virtual"`. [INFERENCE: R2's region value is `auto`, from Cloudflare's documentation; not checked here.] Keep `[snapshot.publish_compression]` unchanged from the previous publish, because it changes uploaded digests (see the table below). Leave `[snapshot.image_publish]` disabled. This page does not cover source-registry publishing.

1. Cold-start from the same base image reference the old golden used, for example `aenv start --cold ubuntu:24.04`. `--cold` takes an OCI image, not a template or snapshot ID ([`aenv start`](../getting-started/aenv-cli/sandboxes.md)). A moved tag changes the base layers, which then upload again.
2. Wait until the sandbox is ready. Run checks 3, 4, 6, and 7 from the [Verification checklist](#verification-checklist) in the golden. Run check 5 in a throwaway sandbox.
3. Capture and publish: `aenv snapshot create <golden-sandbox-id> --name <new-alias>`. The golden keeps running. The command publishes the snapshot to the configured repository before it returns ([Create a Snapshot](../concepts/snapshots/create.md)).
4. Confirm that the alias resolves: `curl -H 'X-API-Key: <key>' http://127.0.0.1:8000/snapshots/<new-alias>` ([Manage Snapshots](../concepts/snapshots/manage.md)).
5. Start a clone with `aenv start <new-alias>`, and run check 9 there.

### What is uploaded and what is reused

The OSS backend keys every managed layer by digest and uploads it only when that key is absent (`upload_managed_layer_if_missing` in `src/snapshot/repository/backends/oss/repository.rs`). Digests cover the bytes actually uploaded, so the publish compression settings affect them (`prepare_layer_upload` in `src/snapshot/repository/backends/common/recontainerize.rs`). For a kernel-only rebake from the same base image:

| Object (OSS key) | Rebake | Why |
|---|---|---|
| Base rootfs layers with a local file and a descriptor, under `managed-layers/{digest}` | **Reused** when an object with the same digest exists; otherwise uploaded | The digest covers the recompressed bytes. The layer is reused only if the raw lower is unchanged and `[snapshot.publish_compression]` is unchanged. [INFERENCE: the same inputs and settings give the same digest; not tested.] |
| Base rootfs lowers that have only a `repoBlobUrl` | **Not copied**; referenced | Recorded as an external layer. |
| Sealed writable rootfs upper (`snapshot.commit`, or its zfile form), under `managed-layers/{digest}` | **New**, uploaded | Runtime-generated delta. [INFERENCE: writes from the new boot differ.] |
| Memory layers, under `managed-layers/{digest}` | **New**, uploaded | Every local memory layer goes through the same existence check. [INFERENCE: a fresh boot produces new memory contents.] |
| `artifacts/{id}/vm_state.bin` | **New** | One per snapshot ID. |
| `artifacts/{id}/firecracker-manifest.json` | **New** | One per snapshot ID. |
| `catalog/records/{id}.json` | **New** | One per snapshot ID. |
| `catalog/aliases/{name}.json` | Created for a new name. For an existing name, see [Repoint an alias](#repoint-an-alias). | `bind_alias` refuses a live target. |

### Repoint an alias

The alias object `catalog/aliases/{name}.json` holds the snapshot ID as a JSON string. `bind_alias` writes it, and `resolve_alias` reads it. Three facts shape the procedure:

- `bind_alias` refuses to rebind a name whose current target still exists (`AliasConflict`).
- No AgentENV command unbinds a name. Deleting the old snapshot removes its alias, but it also removes the rollback target.
- `resolve_alias` deletes an alias whose target record is missing, so point names only at committed snapshots.

Recommended: give each golden its own alias, such as `<new-alias>` above, and switch the consumer to it. Rollback is switching the consumer back, and no object edits are needed.

If the consumer must keep a fixed name, replace the alias object by hand. This is not an AgentENV command, and it has not been tested. [INFERENCE from the object format in `bind_alias` and `resolve_alias`.]

1. Confirm that the new snapshot resolves (step 4 of the publish procedure).
2. Overwrite `catalog/aliases/<name>.json` with the new snapshot ID as a quoted JSON string, `"<new-snapshot-id>"`.
3. Confirm that `GET /snapshots/<name>` returns the new ID.

To roll back, write the old ID back the same way. Keep both snapshots until the switch is verified.

### Retention

`aenv template delete <id-or-alias>` removes the snapshot's catalog record, its alias if the alias still points to it, and `artifacts/{id}/`. It leaves `managed-layers/` in place because those objects are shared by digest. The code defers that cleanup to a separate GC, and this repository has no OSS GC. Each rebake therefore adds storage that outlives the snapshot that created it. Before removing any `managed-layers/` object by hand, check that no remaining record under `catalog/records/` names its digest. [INFERENCE: check a sample record first.]

## Operator caveat for preloop

preloop's AgentENV provider (`crates/preloop-vm/src/agentenv.rs` in `preloopdev/preloop`, function `fork`) runs `aenv snapshot create <golden-sandbox> --name <golden>-snap-<8 hex>` once, on the first fork. It stores that name as `fork_snapshot` in the golden's entry in `agentenv-machines.json`, and later forks reuse it. The cache is keyed on the golden record, not the kernel, so a kernel change is not picked up.

To pick up a new kernel, delete the golden's record in `agentenv-machines.json`, recreate the golden so that it cold-boots on the new kernel, and let the next fork snapshot it. This applies to preloop deployments only.

## Related pages

- [Configuration Reference](../configuration/reference.md)
- [Persistence Artifact Inventory](../internals/persistence-artifact-inventory.md)
- [Sandbox Internals and Testing](../internals/sandbox-testing.md)
- [Snapshots](../concepts/snapshots/index.md)
- [PVM Deployment](./pvm.md)
