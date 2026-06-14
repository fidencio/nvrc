// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

//! NVIDIA Container Toolkit (nvidia-ctk) integration.
//!
//! Generates CDI (Container Device Interface) specs so container runtimes
//! can discover and mount GPU devices without needing the legacy hook.

use crate::execute::foreground;
use crate::gpu;

const NVIDIA_CTK: &str = "/bin/nvidia-ctk";

/// Run nvidia-ctk with given arguments.
fn ctk(args: &[&str]) {
    foreground(&gpu::resolve(NVIDIA_CTK), args);
}

/// Generate CDI spec for GPU device discovery.
/// CDI allows container runtimes (containerd, CRI-O) to inject GPU devices
/// without nvidia-docker. The spec is written to /var/run/cdi/nvidia.yaml
/// where runtimes expect to find it.
///
/// With composable images the GPU userspace lives in the addon at
/// `/run/kata-addons/gpu`, which several flags teach nvidia-ctk about (all
/// no-ops for the monolithic image, where the helpers return nothing and the
/// canonical roots already apply):
///
/// - `--driver-root=<addon>`: nvidia-ctk records each driver library's
///   in-container mount path as its host path with the driver root stripped, so
///   libraries at `<addon>/usr/lib` land at the canonical `/usr/lib` in the
///   container. Without it they would mount at the addon path and apps that scan
///   `/usr` (e.g. NVIDIA NIM) would not find the driver. See [`gpu::driver_root`].
/// - `--dev-root=/`: the driver root also defaults the device-node root, but the
///   `/dev/nvidia*` nodes are real guest nodes, not in the addon; pin it to `/`
///   ([`gpu::DEV_ROOT`]) so the GPU nodes are not dropped from the spec.
/// - `--library-search-path=<addon>/usr/lib`: nvidia-ctk does not honour
///   `LD_LIBRARY_PATH` and the loader-less addon ships no ldcache, so point its
///   discovery at the addon lib dir explicitly (see [`gpu::library_search_paths`]).
/// - `--nvidia-cdi-hook-path=<addon>/bin/nvidia-cdi-hook`: the spec records the
///   path the kata-agent execs to create the libcuda.so.1 symlink and refresh
///   the container ldcache. nvidia-ctk defaults it to `/usr/bin`; the binary
///   lives in the addon, so pass its real (un-stripped) guest location, else the
///   hooks silently no-op and CUDA falls back to the image's cuda-compat driver.
///   See [`gpu::cdi_hook_path`].
pub fn nvidia_ctk_cdi() {
    let mut args: Vec<String> = vec![
        "-d".to_owned(),
        "cdi".to_owned(),
        "generate".to_owned(),
        "--output=/var/run/cdi/nvidia.yaml".to_owned(),
    ];
    if let Some(root) = gpu::driver_root() {
        args.push(format!("--driver-root={root}"));
        args.push(format!("--dev-root={}", gpu::DEV_ROOT));
    }
    for path in gpu::library_search_paths() {
        args.push(format!("--library-search-path={path}"));
    }
    if let Some(path) = gpu::cdi_hook_path() {
        args.push(format!("--nvidia-cdi-hook-path={path}"));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    ctk(&arg_refs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    #[test]
    fn test_ctk_fails_without_binary() {
        let result = panic::catch_unwind(|| {
            ctk(&["--version"]);
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_nvidia_ctk_cdi_fails_without_binary() {
        let result = panic::catch_unwind(|| {
            nvidia_ctk_cdi();
        });
        assert!(result.is_err());
    }
}
