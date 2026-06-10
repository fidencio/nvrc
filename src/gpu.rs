// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

//! Consume the cold-plugged `gpu` addon mounted by [`crate::addon`] at
//! `/run/kata-addons/gpu`.
//!
//! With composable images the base NVIDIA rootfs ships only NVRC and the
//! kata-agent; the GPU userspace (driver libraries, kernel modules, service
//! binaries, configs and firmware) lives in the `gpu` addon. NVRC therefore
//! resolves those components from the addon mount instead of the rootfs:
//!
//! - binaries: exec'd from `<root>/bin` and `<root>/sbin`
//! - libraries: `LD_LIBRARY_PATH=<root>/lib:<root>/usr/lib`
//! - kernel modules: `modprobe --dirname <root>` (the addon ships its own
//!   `lib/modules/<ver>/modules.dep`)
//! - configs: read from `<root>/usr/share/nvidia/...`
//! - firmware: bind-mounted onto `/lib/firmware/nvidia` so the kernel firmware
//!   loader (`request_firmware`, e.g. GSP) finds it at the default search path
//!
//! When the addon is absent (monolithic NVIDIA image) every helper falls back
//! to the canonical rootfs paths, so behaviour is unchanged. No placeholder
//! files are created in the base rootfs; the firmware mountpoint is an empty
//! directory baked into the base image.

use std::path::Path;

use nix::mount::MsFlags;

use crate::macros::ResultExt;

/// Mount point of the GPU addon (see [`crate::addon::mount_all`]).
pub const ROOT: &str = "/run/kata-addons/gpu";

/// Addon firmware tree and the rootfs directory the kernel firmware loader
/// searches by default. The destination is an empty directory provided by the
/// base image (not a binary placeholder).
const FIRMWARE_SRC: &str = "/run/kata-addons/gpu/lib/firmware/nvidia";
const FIRMWARE_DST: &str = "/lib/firmware/nvidia";

/// True when the GPU addon has been mounted.
pub fn present() -> bool {
    Path::new(ROOT).is_dir()
}

/// Resolve a canonical rootfs component path to its addon location when the GPU
/// addon is present, otherwise return it unchanged.
pub fn resolve(path: &str) -> String {
    resolve_in(present(), ROOT, path)
}

fn resolve_in(present: bool, root: &str, path: &str) -> String {
    if present {
        format!("{root}{path}")
    } else {
        path.to_owned()
    }
}

/// `modprobe --dirname` value for `module`: the addon root for NVIDIA modules
/// when the addon is present, otherwise `None` (use the rootfs `/lib/modules`).
/// In-tree modules such as `ib_umad`/`mlx5_ib` stay in the base image.
pub fn modprobe_dirname(module: &str) -> Option<String> {
    modprobe_dirname_in(present(), ROOT, module)
}

fn modprobe_dirname_in(present: bool, root: &str, module: &str) -> Option<String> {
    (present && module.starts_with("nvidia")).then(|| root.to_owned())
}

/// Prepare the environment for GPU components: expose the addon libraries via
/// `LD_LIBRARY_PATH` (inherited by every daemon NVRC spawns) and bind the addon
/// firmware onto the canonical path. No-op without the addon.
pub fn setup() {
    if !present() {
        return;
    }

    let lib_path = format!("{ROOT}/lib:{ROOT}/usr/lib");
    // Safe: NVRC sets this before spawning any GPU daemon and the only other
    // thread (syslog poller) does not touch the environment.
    std::env::set_var("LD_LIBRARY_PATH", &lib_path);
    info!("gpu addon: LD_LIBRARY_PATH={lib_path}");

    bind_firmware();
}

/// Bind the addon firmware tree onto `/lib/firmware/nvidia` so the kernel's
/// `request_firmware` finds GSP (and other) firmware at the default search
/// path. Skipped when the addon ships no firmware.
fn bind_firmware() {
    if !Path::new(FIRMWARE_SRC).is_dir() {
        return;
    }
    bind_dir(FIRMWARE_SRC, FIRMWARE_DST);
    info!("gpu addon: bound firmware {FIRMWARE_SRC} -> {FIRMWARE_DST}");
}

/// Bind-mount directory `src` onto `dst`. `dst` must already exist (the base
/// rootfs is read-only, so the mountpoint is baked in at image build time).
fn bind_dir(src: &str, dst: &str) {
    nix::mount::mount(
        Some(src),
        dst,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .or_panic(format_args!("bind {src} -> {dst}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::require_root;
    use std::fs;
    use tempfile::TempDir;

    // === resolve ===

    #[test]
    fn test_resolve_with_addon() {
        assert_eq!(
            resolve_in(true, "/run/kata-addons/gpu", "/bin/nvidia-smi"),
            "/run/kata-addons/gpu/bin/nvidia-smi"
        );
    }

    #[test]
    fn test_resolve_without_addon() {
        assert_eq!(
            resolve_in(false, "/run/kata-addons/gpu", "/bin/nvidia-smi"),
            "/bin/nvidia-smi"
        );
    }

    #[test]
    fn test_resolve_config_path() {
        assert_eq!(
            resolve_in(true, "/run/kata-addons/gpu", "/usr/share/nvidia/nvlsm/nvlsm.conf"),
            "/run/kata-addons/gpu/usr/share/nvidia/nvlsm/nvlsm.conf"
        );
    }

    // === modprobe_dirname ===

    #[test]
    fn test_modprobe_dirname_nvidia_with_addon() {
        assert_eq!(
            modprobe_dirname_in(true, "/run/kata-addons/gpu", "nvidia"),
            Some("/run/kata-addons/gpu".to_owned())
        );
        assert_eq!(
            modprobe_dirname_in(true, "/run/kata-addons/gpu", "nvidia-uvm"),
            Some("/run/kata-addons/gpu".to_owned())
        );
    }

    #[test]
    fn test_modprobe_dirname_nvidia_without_addon() {
        assert_eq!(modprobe_dirname_in(false, "/run/kata-addons/gpu", "nvidia"), None);
    }

    #[test]
    fn test_modprobe_dirname_base_module_with_addon() {
        // In-tree modules ship in the base image, not the addon.
        assert_eq!(modprobe_dirname_in(true, "/run/kata-addons/gpu", "ib_umad"), None);
        assert_eq!(modprobe_dirname_in(true, "/run/kata-addons/gpu", "mlx5_ib"), None);
    }

    // === bind_dir (needs root for mount(2)) ===

    #[test]
    fn test_bind_dir_makes_source_visible() {
        require_root();
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        // Plain text marker, not a binary placeholder.
        fs::write(src.path().join("gsp.bin.txt"), "firmware").unwrap();

        let src_str = src.path().to_str().unwrap();
        let dst_str = dst.path().to_str().unwrap();
        bind_dir(src_str, dst_str);

        let visible = dst.path().join("gsp.bin.txt");
        assert!(visible.exists());
        assert_eq!(fs::read_to_string(visible).unwrap(), "firmware");

        nix::mount::umount(dst.path()).unwrap();
    }
}
