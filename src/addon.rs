// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

//! Mount cold-plugged composable VM image addons before kata-agent starts.
//!
//! NVRC stands in for the systemd `kata-addon-mount@.service` unit: for each
//! `kata.addon.<name>.verity_params=...` on the kernel command line, find the
//! cold-plugged virtio-blk device (serial `addon-<name>`), open it with
//! dm-verity, and mount the EROFS read-only at `/run/kata-addons/<name>/`. Any
//! failure panics (powering off the VM): a confidential VM must not run with a
//! missing or tampered addon.
//!
//! Proposal: <https://github.com/kata-containers/kata-containers/pull/13029>

use log::info;
use nix::mount::MsFlags;
use std::fs;

use crate::execute::foreground;
use crate::macros::ResultExt;

const CMDLINE: &str = "/proc/cmdline";
const SYS_BLOCK: &str = "/sys/block";
const MOUNT_BASE: &str = "/run/kata-addons";
const VERITYSETUP: &str = "/usr/sbin/veritysetup";

/// Prefix for the virtio-blk serial and the dm-verity device-mapper target.
const ADDON_PREFIX: &str = "addon-";

/// dm-verity parameters from `kata.addon.<name>.verity_params`, matching the
/// comma-separated list emitted by the Kata image builder. Hash is sha256.
struct VerityParams {
    root_hash: String,
    salt: String,
    data_blocks: u64,
    data_block_size: u64,
    hash_block_size: u64,
}

/// Mount every addon declared on the kernel command line. No-op when no
/// `kata.addon.*.verity_params` entries are present (non-composable images).
pub fn mount_all() {
    let cmdline = fs::read_to_string(CMDLINE).or_panic(format_args!("read {CMDLINE}"));
    for (name, params) in parse_addons(&cmdline) {
        mount_addon(&name, &params);
    }
}

/// Discover, verity-open and mount a single addon.
fn mount_addon(name: &str, params: &VerityParams) {
    let serial = format!("{ADDON_PREFIX}{name}");
    let dev = find_device_by_serial(SYS_BLOCK, &serial)
        .unwrap_or_else(|| panic!("addon {name}: no block device with serial {serial}"));

    let (data, hash) = find_partitions(SYS_BLOCK, &dev);

    let dm_name = format!("{ADDON_PREFIX}{name}");
    let args = verity_args(&dm_name, &data, &hash, params);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    foreground(VERITYSETUP, &arg_refs);

    let mapper = format!("/dev/mapper/{dm_name}");
    let target = format!("{MOUNT_BASE}/{name}");
    fs::create_dir_all(&target).or_panic(format_args!("create_dir_all {target}"));

    // NOEXEC omitted: addons ship executables (e.g. attestation-agent).
    let flags = MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    nix::mount::mount(
        Some(mapper.as_str()),
        target.as_str(),
        Some("erofs"),
        flags,
        None::<&str>,
    )
    .or_panic(format_args!("mount addon {name} ({mapper}) on {target}"));

    info!("mounted addon {name} at {target}");
}

/// Parse every `kata.addon.<name>.verity_params=...` entry from the kernel
/// command line into `(name, params)` pairs. Other parameters are ignored.
fn parse_addons(cmdline: &str) -> Vec<(String, VerityParams)> {
    cmdline
        .split_whitespace()
        .filter_map(|param| param.split_once('='))
        .filter_map(|(key, value)| {
            let name = key
                .strip_prefix("kata.addon.")?
                .strip_suffix(".verity_params")?;
            Some((name.to_owned(), parse_verity_params(name, value)))
        })
        .collect()
}

/// Parse the comma-separated verity parameter list. All five fields are
/// required; anything missing or malformed is fatal (fail-fast).
fn parse_verity_params(name: &str, value: &str) -> VerityParams {
    let mut root_hash = None;
    let mut salt = None;
    let mut data_blocks = None;
    let mut data_block_size = None;
    let mut hash_block_size = None;

    for (key, val) in value.split(',').filter_map(|kv| kv.split_once('=')) {
        match key {
            "root_hash" => root_hash = Some(val.to_owned()),
            "salt" => salt = Some(val.to_owned()),
            "data_blocks" => data_blocks = Some(parse_u64(name, "data_blocks", val)),
            "data_block_size" => data_block_size = Some(parse_u64(name, "data_block_size", val)),
            "hash_block_size" => hash_block_size = Some(parse_u64(name, "hash_block_size", val)),
            _ => {}
        }
    }

    let require = |field: &str, v: Option<String>| {
        v.unwrap_or_else(|| panic!("addon {name}: verity_params missing {field}"))
    };
    let require_num = |field: &str, v: Option<u64>| {
        v.filter(|n| *n != 0)
            .unwrap_or_else(|| panic!("addon {name}: verity_params missing or zero {field}"))
    };

    VerityParams {
        root_hash: require("root_hash", root_hash),
        salt: require("salt", salt),
        data_blocks: require_num("data_blocks", data_blocks),
        data_block_size: require_num("data_block_size", data_block_size),
        hash_block_size: require_num("hash_block_size", hash_block_size),
    }
}

fn parse_u64(name: &str, field: &str, value: &str) -> u64 {
    value
        .parse()
        .unwrap_or_else(|_| panic!("addon {name}: invalid verity_params {field}={value}"))
}

/// Build the `veritysetup open` arguments. Params are passed explicitly because
/// the image is built with `veritysetup format --no-superblock`.
fn verity_args(dm_name: &str, data: &str, hash: &str, p: &VerityParams) -> Vec<String> {
    vec![
        "open".to_owned(),
        "--no-superblock".to_owned(),
        "--hash".to_owned(),
        "sha256".to_owned(),
        "--data-block-size".to_owned(),
        p.data_block_size.to_string(),
        "--hash-block-size".to_owned(),
        p.hash_block_size.to_string(),
        "--data-blocks".to_owned(),
        p.data_blocks.to_string(),
        "--salt".to_owned(),
        p.salt.clone(),
        data.to_owned(),
        dm_name.to_owned(),
        hash.to_owned(),
        p.root_hash.clone(),
    ]
}

/// Find the block device with the given `serial` (e.g. `vdb`). Serial-based
/// discovery is order-independent and needs no udev in the minimal guest.
fn find_device_by_serial(sys_block: &str, serial: &str) -> Option<String> {
    fs::read_dir(sys_block).ok()?.flatten().find_map(|entry| {
        let found = fs::read_to_string(entry.path().join("serial")).ok()?;
        (found.trim() == serial).then(|| entry.file_name().to_string_lossy().into_owned())
    })
}

/// Resolve the data (partition 1) and hash (partition 2) paths, reading the
/// partition number from sysfs so the device naming convention doesn't matter.
fn find_partitions(sys_block: &str, dev: &str) -> (String, String) {
    let dir = format!("{sys_block}/{dev}");
    let mut data = None;
    let mut hash = None;

    for entry in fs::read_dir(&dir)
        .or_panic(format_args!("read_dir {dir}"))
        .flatten()
    {
        let Ok(number) = fs::read_to_string(entry.path().join("partition")) else {
            continue;
        };
        let part = format!("/dev/{}", entry.file_name().to_string_lossy());
        match number.trim() {
            "1" => data = Some(part),
            "2" => hash = Some(part),
            _ => {}
        }
    }

    (
        data.unwrap_or_else(|| panic!("addon device {dev}: missing data partition (1)")),
        hash.unwrap_or_else(|| panic!("addon device {dev}: missing hash partition (2)")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};
    use tempfile::TempDir;

    const PARAMS: &str = "root_hash=abc123,salt=def456,data_blocks=96512,\
                          data_block_size=4096,hash_block_size=4096";

    // === parse_verity_params ===

    #[test]
    fn test_parse_verity_params_valid() {
        let p = parse_verity_params("coco", PARAMS);
        assert_eq!(p.root_hash, "abc123");
        assert_eq!(p.salt, "def456");
        assert_eq!(p.data_blocks, 96512);
        assert_eq!(p.data_block_size, 4096);
        assert_eq!(p.hash_block_size, 4096);
    }

    #[test]
    fn test_parse_verity_params_ignores_unknown_keys() {
        let p = parse_verity_params("coco", &format!("{PARAMS},extra=ignored"));
        assert_eq!(p.root_hash, "abc123");
    }

    #[rstest]
    #[case::missing_root_hash("salt=def,data_blocks=1,data_block_size=4096,hash_block_size=4096")]
    #[case::zero_data_blocks(
        "root_hash=a,salt=b,data_blocks=0,data_block_size=4096,hash_block_size=4096"
    )]
    #[case::non_numeric(
        "root_hash=a,salt=b,data_blocks=lots,data_block_size=4096,hash_block_size=4096"
    )]
    #[should_panic]
    fn test_parse_verity_params_invalid(#[case] params: &str) {
        parse_verity_params("coco", params);
    }

    // === parse_addons ===

    #[rstest]
    #[case::single(
        format!("ro quiet kata.addon.coco.verity_params={PARAMS} console=ttyS0"),
        vec!["coco"]
    )]
    #[case::multiple(
        format!("kata.addon.coco.verity_params={PARAMS} kata.addon.gpu.verity_params={PARAMS}"),
        vec!["coco", "gpu"]
    )]
    #[case::none("ro quiet console=ttyS0 nvrc.log=debug".to_owned(), vec![])]
    #[case::other_kata_params("kata.addon.coco.other=x kata.something=y".to_owned(), vec![])]
    fn test_parse_addons(#[case] cmdline: String, #[case] expected: Vec<&str>) {
        let addons = parse_addons(&cmdline);
        let names: Vec<&str> = addons.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, expected);
    }

    // === verity_args ===

    #[test]
    fn test_verity_args_order() {
        let p = parse_verity_params("coco", PARAMS);
        let args = verity_args("addon-coco", "/dev/vdb1", "/dev/vdb2", &p);
        assert_eq!(
            args,
            vec![
                "open",
                "--no-superblock",
                "--hash",
                "sha256",
                "--data-block-size",
                "4096",
                "--hash-block-size",
                "4096",
                "--data-blocks",
                "96512",
                "--salt",
                "def456",
                "/dev/vdb1",
                "addon-coco",
                "/dev/vdb2",
                "abc123",
            ]
        );
    }

    // === find_device_by_serial ===

    fn write_serial(sys_block: &TempDir, dev: &str, serial: &str) {
        let dir = sys_block.path().join(dev);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("serial"), format!("{serial}\n")).unwrap();
    }

    /// sysfs with a rootfs device and an addon device exposing serials.
    #[fixture]
    fn sys_block() -> TempDir {
        let dir = TempDir::new().unwrap();
        write_serial(&dir, "vda", "rootfs");
        write_serial(&dir, "vdb", "addon-coco");
        dir
    }

    #[rstest]
    #[case::match_found("addon-coco", Some("vdb".to_owned()))]
    #[case::no_match("addon-missing", None)]
    fn test_find_device_by_serial(
        sys_block: TempDir,
        #[case] serial: &str,
        #[case] expected: Option<String>,
    ) {
        let dev = find_device_by_serial(sys_block.path().to_str().unwrap(), serial);
        assert_eq!(dev, expected);
    }

    #[test]
    fn test_find_device_by_serial_missing_serial_file() {
        let sys_block = TempDir::new().unwrap();
        // device dir without a serial attribute must be skipped, not panic
        fs::create_dir_all(sys_block.path().join("vdb")).unwrap();
        let dev = find_device_by_serial(sys_block.path().to_str().unwrap(), "addon-coco");
        assert_eq!(dev, None);
    }

    #[test]
    fn test_find_device_by_serial_nonexistent_dir() {
        assert_eq!(
            find_device_by_serial("/nonexistent/path", "addon-coco"),
            None
        );
    }

    // === find_partitions ===

    fn write_partition(sys_block: &TempDir, dev: &str, part: &str, number: &str) {
        let dir = sys_block.path().join(dev).join(part);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("partition"), format!("{number}\n")).unwrap();
    }

    #[test]
    fn test_find_partitions() {
        // Addons are cold-plugged as virtio-blk devices (vdX).
        let sys_block = TempDir::new().unwrap();
        write_partition(&sys_block, "vdb", "vdb1", "1");
        write_partition(&sys_block, "vdb", "vdb2", "2");
        // a non-partition sysfs attribute alongside partitions must be ignored
        fs::write(sys_block.path().join("vdb").join("size"), "100\n").unwrap();

        let (data, hash) = find_partitions(sys_block.path().to_str().unwrap(), "vdb");
        assert_eq!(data, "/dev/vdb1");
        assert_eq!(hash, "/dev/vdb2");
    }

    #[test]
    #[should_panic]
    fn test_find_partitions_missing_hash_panics() {
        let sys_block = TempDir::new().unwrap();
        write_partition(&sys_block, "vdb", "vdb1", "1");
        find_partitions(sys_block.path().to_str().unwrap(), "vdb");
    }
}
