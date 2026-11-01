//! The bubblewrap backend: Linux namespaces, no root, no overlay. The layout is in
//! docs/sandbox.md; `args` builds it and is tested without running bwrap.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{Backend, Spec};

/// System dirs bound read-only. /mnt (Windows drives on WSL), /init (WSL's interop runs
/// .exe files through it), /var (docker.sock), /home and /root are not among them.
const SYSTEM: [&str; 6] = ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

pub struct Bwrap;

impl Backend for Bwrap {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    fn check(&self) -> Result<()> {
        // A real one, not just `--version`: user namespaces can be off (sysctl, AppArmor).
        let out = Command::new("bwrap")
            .args([
                "--unshare-user",
                "--unshare-pid",
                "--die-with-parent",
                "--ro-bind",
                "/",
                "/",
                "true",
            ])
            .stdin(Stdio::null())
            .output()
            .context("run bwrap (install bubblewrap)")?;
        if !out.status.success() {
            bail!(
                "bwrap cannot make a sandbox here: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn argv(&self, spec: &Spec) -> Vec<OsString> {
        let mut a = vec![OsString::from("bwrap")];
        a.extend(args(spec, &|p| p.exists()));
        a
    }
}

fn bind(out: &mut Vec<OsString>, flag: &str, src: &Path, dst: &Path) {
    out.push(flag.into());
    out.push(src.into());
    out.push(dst.into());
}

/// bwrap's arguments for `spec`, then `--` and the command. `exists` says which host
/// paths are there (missing system dirs are skipped).
pub fn args(spec: &Spec, exists: &dyn Fn(&Path) -> bool) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        // The network stays the host's (owner decision 2026-11-01): no --unshare-net.
        "--die-with-parent",
        "--new-session",
    ]
    .map(OsString::from)
    .to_vec();
    for d in SYSTEM {
        let p = Path::new(d);
        if exists(p) {
            bind(&mut a, "--ro-bind", p, p);
        }
    }
    for d in ["/tmp", "/run"] {
        a.extend(["--tmpfs".into(), d.into()]);
    }
    a.extend(["--proc", "/proc", "--dev", "/dev"].map(OsString::from));
    // Read-write: the private home, then the worktree; read-only binds after them, so
    // those inside the home (the agent's plugins) sit on top.
    for p in [&spec.home, &spec.work] {
        bind(&mut a, "--bind", p, p);
    }
    for (src, dst) in &spec.ro {
        if exists(src) {
            bind(&mut a, "--ro-bind", src, dst);
        }
    }
    a.push("--chdir".into());
    a.push(spec.cwd.clone().into());
    a.push("--clearenv".into());
    for (k, v) in &spec.env {
        a.extend(["--setenv".into(), k.into(), v.into()]);
    }
    a.push("--".into());
    a.extend(spec.argv.iter().map(OsString::from));
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec() -> Spec {
        Spec {
            work: "/data/sb/s1/work".into(),
            home: "/data/sb/s1/home".into(),
            cwd: "/data/sb/s1/work/src".into(),
            ro: vec![
                (PathBuf::from("/h/.cargo"), PathBuf::from("/h/.cargo")),
                (
                    PathBuf::from("/h/.claude/plugins"),
                    PathBuf::from("/data/sb/s1/home/.claude/plugins"),
                ),
                (PathBuf::from("/h/.nvm"), PathBuf::from("/h/.nvm")),
            ],
            env: vec![("HOME".into(), "/data/sb/s1/home".into())],
            argv: vec!["make".into(), "test".into()],
        }
    }

    fn joined(a: &[OsString]) -> String {
        a.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn the_layout_hides_the_host_and_keeps_the_network() {
        let a = args(&spec(), &|p| {
            p != Path::new("/lib64") && p != Path::new("/h/.nvm")
        });
        let s = joined(&a);
        for want in [
            "--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup-try --die-with-parent --new-session",
            "--ro-bind /usr /usr",
            "--ro-bind /etc /etc",
            "--tmpfs /tmp --tmpfs /run --proc /proc --dev /dev",
            "--bind /data/sb/s1/home /data/sb/s1/home --bind /data/sb/s1/work /data/sb/s1/work",
            "--ro-bind /h/.cargo /h/.cargo",
            "--ro-bind /h/.claude/plugins /data/sb/s1/home/.claude/plugins",
            "--chdir /data/sb/s1/work/src --clearenv --setenv HOME /data/sb/s1/home -- make test",
        ] {
            assert!(s.contains(want), "missing `{want}` in {s}");
        }
        for never in [
            "--unshare-net",
            "/lib64",
            "/h/.nvm",
            "/mnt",
            "/init",
            "/var",
            " /home ",
            "--bind / ",
        ] {
            assert!(!s.contains(never), "`{never}` in {s}");
        }
        // The command is last, after `--`: no argument of it reads as a bwrap flag.
        assert_eq!(a[a.len() - 3], "--");
    }
}
