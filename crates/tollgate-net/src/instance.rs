//! Named instances, and where each keeps its sockets.
//!
//! One machine may run `tollgated` once per task — selling LAN access, selling
//! FIPS peering, selling a FIPS exit — and each run is an **instance** with a
//! name of its own (`docs/design/core/tollgate-configuration.md`, Instance).
//! An instance with no name is called [`DEFAULT`].
//!
//! Each instance has its own runtime directory, `/run/tollgate-<instance>/`,
//! holding its sockets under fixed names: [`CONTROL_SOCKET`], which
//! `tollgated` serves, and [`ENFORCER_SOCKET`], where an external enforcer
//! listens. A client therefore finds an instance from its name alone, and
//! `tolltop` finds every instance by listing `/run/tollgate-*/control.sock`.
//!
//! `/run` is the Linux and OpenWrt place. The macOS package uses
//! `/usr/local/var/run`, a node a person runs by hand `$XDG_RUNTIME_DIR`, and
//! the temporary directory is the last resort that exists everywhere. Only that
//! first part changes; see [`bases`].

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The name of an instance nobody named.
pub const DEFAULT: &str = "default";

/// The control socket's name inside the runtime directory.
pub const CONTROL_SOCKET: &str = "control.sock";

/// The external enforcer's socket's name inside the runtime directory.
pub const ENFORCER_SOCKET: &str = "enforcer.sock";

/// Check an instance name: letters, digits and `-` only, because it becomes
/// part of a path and of a service name.
pub fn validate(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("an instance name may not be empty");
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
    {
        bail!("instance name {name:?} has {c:?} in it; only letters, digits and '-' are allowed");
    }
    Ok(())
}

/// The directories a runtime directory may live in, best first.
///
/// `/run` is where a Linux service manager and the OpenWrt init script put
/// it. `/usr/local/var/run` is the macOS package's equivalent, since `/run`
/// there is neither writable nor something launchd populates.
/// `$XDG_RUNTIME_DIR` is where a node a person runs on their own machine
/// belongs, and the temporary directory is the fallback that exists
/// everywhere.
pub fn bases() -> Vec<PathBuf> {
    let mut bases = vec![PathBuf::from("/run"), PathBuf::from("/usr/local/var/run")];
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        bases.push(PathBuf::from(xdg));
    }
    bases.push(std::env::temp_dir());
    bases
}

/// The runtime directory `instance` has under `base`.
pub fn dir_in(base: &Path, instance: &str) -> PathBuf {
    base.join(format!("tollgate-{instance}"))
}

/// The runtime directory this instance uses, created if it is missing and can
/// be.
///
/// The first base that exists and in which the instance's directory exists or
/// can be made, and is writable: a daemon that picked a directory it could not
/// write to would fail at startup over something an operator never asked for.
/// A base that does not exist is skipped rather than made.
pub fn runtime_dir(instance: &str) -> PathBuf {
    let bases = bases();
    for base in &bases {
        if !base.is_dir() {
            continue;
        }
        let dir = dir_in(base, instance);
        if usable(&dir) {
            return dir;
        }
    }
    // The temporary directory exists everywhere; if even that fails, binding
    // the socket will say why.
    dir_in(bases.last().expect("never empty"), instance)
}

/// Whether `dir` exists or can be made, and this process can write in it.
fn usable(dir: &Path) -> bool {
    if !dir.is_dir() && create(dir).is_err() {
        return false;
    }
    // Writability is asked of the directory rather than assumed from the
    // user id: a container runs as root and a laptop does not, and both are
    // ordinary ways to run this.
    let probe = dir.join(".tollgate-write-test");
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&probe)
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Make a runtime directory: only its owner may create files in it, anyone may
/// reach a socket in it that the socket's own permissions admit.
fn create(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o755).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(dir)
    }
}

/// Where a running instance's control socket already is: the first base that
/// has one.
pub fn find_control_socket(instance: &str) -> Option<PathBuf> {
    bases()
        .iter()
        .map(|base| dir_in(base, instance).join(CONTROL_SOCKET))
        .find(|path| path.exists())
}

/// Every running instance's control socket, as `(instance, path)`, found by
/// listing `tollgate-*/control.sock` in every base.
///
/// An instance found in more than one base is listed once, at the first.
pub fn control_sockets() -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for base in bases() {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        let mut here: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let instance = name.strip_prefix("tollgate-")?.to_owned();
                validate(&instance).ok()?;
                let socket = entry.path().join(CONTROL_SOCKET);
                socket.exists().then_some((instance, socket))
            })
            .collect();
        here.sort();
        for (instance, socket) in here {
            if !found.iter().any(|(i, _)| *i == instance) {
                found.push((instance, socket));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_letters_digits_and_dashes() {
        for ok in ["default", "ip", "fips", "fips-exit", "lan2", "A-1"] {
            assert!(validate(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "fips_exit",
            "a/b",
            "..",
            "a.b",
            "a b",
            "tollgate/ip",
            "é",
        ] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_runtime_directory_is_named_for_the_instance() {
        assert_eq!(
            dir_in(Path::new("/run"), "fips-exit"),
            PathBuf::from("/run/tollgate-fips-exit")
        );
        assert_eq!(
            dir_in(Path::new("/run"), DEFAULT).join(CONTROL_SOCKET),
            PathBuf::from("/run/tollgate-default/control.sock")
        );
        assert_eq!(
            dir_in(Path::new("/run"), "ip").join(ENFORCER_SOCKET),
            PathBuf::from("/run/tollgate-ip/enforcer.sock")
        );
    }

    #[test]
    fn run_is_looked_in_first_and_the_temporary_directory_last() {
        let bases = bases();
        assert_eq!(bases[0], PathBuf::from("/run"));
        assert_eq!(bases[1], PathBuf::from("/usr/local/var/run"));
        assert_eq!(bases.last(), Some(&std::env::temp_dir()));
    }

    #[test]
    fn the_runtime_directory_is_one_this_process_can_write_in() {
        let name = format!("unit-test-{}", std::process::id());
        let dir = runtime_dir(&name);
        assert!(
            dir.ends_with(format!("tollgate-{name}")),
            "{}",
            dir.display()
        );
        assert!(dir.is_dir(), "{} was not made", dir.display());
        let _ = std::fs::remove_dir(&dir);
    }
}
