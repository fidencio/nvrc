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
/// nvidia-ctk does not honour `LD_LIBRARY_PATH`; it locates the driver
/// libraries to mount via the ldcache and standard paths. With composable
/// images the GPU libraries live in the addon, so pass the addon lib dirs via
/// `--library-search-path` (a no-op for the monolithic image, where
/// [`gpu::library_search_paths`] is empty and the ldcache already covers them).
///
/// The generated spec also records the path to `nvidia-cdi-hook`, which the
/// kata-agent executes to create the libcuda.so.1 symlink and refresh the
/// ldcache in the container. nvidia-ctk defaults that to `/usr/bin`, but with
/// composable images the binary lives in the addon, so pass its real location
/// via `--nvidia-cdi-hook-path` (see [`gpu::cdi_hook_path`]); otherwise the
/// hooks silently no-op and CUDA falls back to the image's cuda-compat driver.
pub fn nvidia_ctk_cdi() {
    let mut args: Vec<String> = vec![
        "-d".to_owned(),
        "cdi".to_owned(),
        "generate".to_owned(),
        "--output=/var/run/cdi/nvidia.yaml".to_owned(),
    ];
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
