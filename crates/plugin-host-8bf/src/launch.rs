//! Deciding how to run a plug-in.
//!
//! A plug-in is a native binary for some architecture, and the machine
//! running Schist may not be that architecture — or even that operating
//! system. This module holds the policy: given what the host is and what
//! the plug-in is, either a plan for running it or a reason it cannot
//! be run, in terms a user can act on.
//!
//! The plan always ends in a **helper process**, never an in-process
//! load. Three things fall out of that which cannot be had otherwise:
//!
//! * A plug-in fault takes the helper, not the document.
//! * A plug-in built for another architecture runs in a helper built for
//!   *its* architecture, so an x86-64 filter works on an Apple Silicon
//!   Mac and a Windows filter works on Linux.
//! * The emulator, where one is needed, wraps the helper's command line
//!   and nothing else has to know about it.

use std::fmt;
use std::path::{Path, PathBuf};

/// The architecture a plug-in binary was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginAbi {
    /// A Windows DLL — `.8bf` as shipped on Windows.
    WindowsX86,
    WindowsX86_64,
    /// A Mach-O bundle — `.plugin` as shipped on macOS.
    MacX86_64,
    MacArm64,
}

impl PluginAbi {
    fn is_windows(self) -> bool {
        matches!(self, PluginAbi::WindowsX86 | PluginAbi::WindowsX86_64)
    }
}

impl fmt::Display for PluginAbi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PluginAbi::WindowsX86 => "32-bit Windows",
            PluginAbi::WindowsX86_64 => "64-bit Windows",
            PluginAbi::MacX86_64 => "Intel macOS",
            PluginAbi::MacArm64 => "Apple Silicon macOS",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Linux,
    MacOs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86,
    X86_64,
    Arm64,
}

/// What Schist itself is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub os: Os,
    pub arch: Arch,
}

impl Host {
    /// The machine this build is running on.
    pub fn current() -> Option<Host> {
        let os = if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            return None;
        };
        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Arm64
        } else if cfg!(target_arch = "x86") {
            Arch::X86
        } else {
            return None;
        };
        Some(Host { os, arch })
    }
}

/// Which build of the helper a plan needs. These are Rust target
/// triples because that is what actually has to be built and shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    WindowsX86,
    WindowsX86_64,
    MacX86_64,
    MacArm64,
}

impl Helper {
    /// The file name the packaging installs it under.
    pub fn file_name(self) -> &'static str {
        match self {
            Helper::WindowsX86 => "schist-8bf-helper-x86.exe",
            Helper::WindowsX86_64 => "schist-8bf-helper-x86_64.exe",
            Helper::MacX86_64 => "schist-8bf-helper-x86_64",
            Helper::MacArm64 => "schist-8bf-helper-arm64",
        }
    }
}

/// An external program the plan needs, and which the user installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Wine, to run a Windows helper anywhere that is not Windows.
    Wine,
    /// FEX-Emu, to run an x86 or x86-64 Wine on 64-bit Arm Linux.
    /// <https://github.com/FEX-Emu/FEX>
    Fex,
    /// Rosetta 2, to run an Intel helper on Apple Silicon.
    Rosetta,
}

impl Requirement {
    pub fn name(self) -> &'static str {
        match self {
            Requirement::Wine => "Wine",
            Requirement::Fex => "FEX-Emu",
            Requirement::Rosetta => "Rosetta 2",
        }
    }

    /// Where a user gets it. Rosetta has no URL: it is an Apple
    /// component installed with `softwareupdate`.
    pub fn url(self) -> Option<&'static str> {
        match self {
            Requirement::Wine => Some("https://www.winehq.org/"),
            Requirement::Fex => Some("https://github.com/FEX-Emu/FEX"),
            Requirement::Rosetta => None,
        }
    }

    /// How to check it is there.
    pub fn probe_binary(self) -> &'static str {
        match self {
            Requirement::Wine => "wine",
            // FEX installs a binfmt handler as well, so an x86-64 binary
            // may simply run; the interpreter is what proves it is
            // present either way.
            Requirement::Fex => "FEXInterpreter",
            Requirement::Rosetta => "arch",
        }
    }
}

/// How to run a plug-in: which helper, wrapped in what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub helper: Helper,
    /// Programs that must be installed, in the order they wrap the
    /// helper's command line — outermost first.
    pub needs: Vec<Requirement>,
}

impl Plan {
    /// True when the helper is a Windows binary running under Wine, and
    /// so sees the filesystem through a drive letter.
    pub fn under_wine(&self) -> bool {
        self.needs.contains(&Requirement::Wine)
    }

    /// A path as the *helper* will see it.
    ///
    /// Wine maps the whole Unix filesystem at `Z:`, so an absolute path
    /// becomes a Windows one. Everything else — a native helper, or an
    /// Intel helper under Rosetta — shares the host's view already.
    pub fn helper_path(&self, path: &Path) -> String {
        let text = path.to_string_lossy();
        if !self.under_wine() || !text.starts_with('/') {
            return text.into_owned();
        }
        format!("Z:{}", text.replace('/', "\\"))
    }

    /// Build the command line, given where the helper lives and what
    /// arguments it takes. The wrappers go on the front in order, so
    /// FEX wraps Wine wraps the helper.
    pub fn command(&self, helper_dir: &Path, args: &[String]) -> Vec<String> {
        let mut cmd: Vec<String> = Vec::new();
        for need in &self.needs {
            match need {
                Requirement::Fex => cmd.push("FEXInterpreter".into()),
                Requirement::Wine => cmd.push("wine".into()),
                Requirement::Rosetta => {
                    cmd.push("arch".into());
                    cmd.push("-x86_64".into());
                }
            }
        }
        cmd.push(
            helper_dir
                .join(self.helper.file_name())
                .to_string_lossy()
                .into_owned(),
        );
        cmd.extend(args.iter().cloned());
        cmd
    }
}

/// Why a plug-in cannot be run here at all — as distinct from needing
/// something installed, which is a [`Plan`] with unmet [`Requirement`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    /// An Apple Silicon plug-in on an Intel Mac. Rosetta runs Intel code
    /// on Arm and not the other way round.
    NoArmEmulationOnIntel,
    /// A macOS plug-in anywhere but macOS. There is no Wine for Mach-O.
    MacPluginOffMac,
    /// A Windows plug-in on a machine Schist cannot emulate x86 on.
    NoX86Emulation,
    /// Schist is running somewhere this module has no policy for.
    UnknownHost,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unsupported::NoArmEmulationOnIntel => write!(
                f,
                "this is an Apple Silicon plug-in and this Mac is Intel; \
                 Rosetta translates Intel code to Arm, not the reverse"
            ),
            Unsupported::MacPluginOffMac => {
                write!(f, "macOS plug-ins only run on macOS")
            }
            Unsupported::NoX86Emulation => write!(f, "no way to run x86 code on this machine"),
            Unsupported::UnknownHost => {
                write!(
                    f,
                    "Schist does not know how to run plug-ins on this platform"
                )
            }
        }
    }
}

impl std::error::Error for Unsupported {}

/// Decide how to run `plugin` on `host`.
///
/// The matrix, in full:
///
/// | host | plug-in | plan |
/// |---|---|---|
/// | Windows x86-64 | Windows x86-64 | native helper |
/// | Windows x86-64 | Windows x86 | 32-bit helper, on WOW64 |
/// | Windows arm64 | either Windows | matching helper, on Windows' own x86 emulation |
/// | Linux x86-64 | either Windows | Windows helper under Wine |
/// | Linux arm64 | either Windows | Windows helper under Wine under FEX |
/// | macOS arm64 | Apple Silicon | native helper |
/// | macOS arm64 | Intel | Intel helper under Rosetta |
/// | macOS x86-64 | Intel | native helper |
/// | macOS x86-64 | Apple Silicon | impossible |
/// | anywhere else | macOS plug-in | impossible |
pub fn plan(host: Host, plugin: PluginAbi) -> Result<Plan, Unsupported> {
    let windows_helper = |abi: PluginAbi| match abi {
        PluginAbi::WindowsX86 => Helper::WindowsX86,
        _ => Helper::WindowsX86_64,
    };

    match (host.os, host.arch, plugin) {
        // macOS plug-ins are Mach-O; nothing else loads them.
        (os, _, PluginAbi::MacX86_64 | PluginAbi::MacArm64) if os != Os::MacOs => {
            Err(Unsupported::MacPluginOffMac)
        }
        (Os::MacOs, Arch::Arm64, PluginAbi::MacArm64) => Ok(Plan {
            helper: Helper::MacArm64,
            needs: vec![],
        }),
        (Os::MacOs, Arch::Arm64, PluginAbi::MacX86_64) => Ok(Plan {
            helper: Helper::MacX86_64,
            needs: vec![Requirement::Rosetta],
        }),
        (Os::MacOs, Arch::X86_64, PluginAbi::MacX86_64) => Ok(Plan {
            helper: Helper::MacX86_64,
            needs: vec![],
        }),
        (Os::MacOs, Arch::X86_64, PluginAbi::MacArm64) => Err(Unsupported::NoArmEmulationOnIntel),
        // Windows plug-ins on macOS would want Wine, which Schist does
        // not ask Mac users to install.
        (Os::MacOs, _, p) if p.is_windows() => Err(Unsupported::NoX86Emulation),

        // Windows runs its own binaries; a 32-bit plug-in goes in a
        // 32-bit helper, which WOW64 — or Windows-on-Arm's emulation —
        // takes care of.
        (Os::Windows, _, p) if p.is_windows() => Ok(Plan {
            helper: windows_helper(p),
            needs: vec![],
        }),

        // Linux needs Wine for the PE, and on Arm needs FEX under that
        // to run Wine's x86 code at all.
        (Os::Linux, Arch::X86_64 | Arch::X86, p) if p.is_windows() => Ok(Plan {
            helper: windows_helper(p),
            needs: vec![Requirement::Wine],
        }),
        (Os::Linux, Arch::Arm64, p) if p.is_windows() => Ok(Plan {
            helper: windows_helper(p),
            needs: vec![Requirement::Fex, Requirement::Wine],
        }),

        _ => Err(Unsupported::UnknownHost),
    }
}

/// Which of a plan's requirements are not installed.
///
/// The check is a `PATH` lookup, which is what a user can fix. Rosetta
/// is exempt: `arch` is always present on macOS and whether Rosetta
/// itself is installed only shows when something is run.
pub fn missing(plan: &Plan) -> Vec<Requirement> {
    plan.needs
        .iter()
        .copied()
        .filter(|r| *r != Requirement::Rosetta && !on_path(r.probe_binary()))
        .collect()
}

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

/// Where the helper binaries are installed. Beside the running
/// executable, which is what every packaging layout here produces.
pub fn helper_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN64: Host = Host {
        os: Os::Windows,
        arch: Arch::X86_64,
    };
    const LINUX64: Host = Host {
        os: Os::Linux,
        arch: Arch::X86_64,
    };
    const LINUX_ARM: Host = Host {
        os: Os::Linux,
        arch: Arch::Arm64,
    };
    const MAC_ARM: Host = Host {
        os: Os::MacOs,
        arch: Arch::Arm64,
    };
    const MAC_INTEL: Host = Host {
        os: Os::MacOs,
        arch: Arch::X86_64,
    };

    #[test]
    fn windows_runs_its_own_binaries_with_no_help() {
        for abi in [PluginAbi::WindowsX86_64, PluginAbi::WindowsX86] {
            let p = plan(WIN64, abi).unwrap();
            assert!(p.needs.is_empty());
        }
        // A 32-bit plug-in needs the 32-bit helper even on a 64-bit
        // Windows: the bitness has to match the plug-in, not the host.
        assert_eq!(
            plan(WIN64, PluginAbi::WindowsX86).unwrap().helper,
            Helper::WindowsX86
        );
        assert_eq!(
            plan(WIN64, PluginAbi::WindowsX86_64).unwrap().helper,
            Helper::WindowsX86_64
        );
    }

    #[test]
    fn x86_64_linux_needs_wine_and_nothing_else() {
        let p = plan(LINUX64, PluginAbi::WindowsX86_64).unwrap();
        assert_eq!(p.needs, vec![Requirement::Wine]);
        assert_eq!(p.helper, Helper::WindowsX86_64);
    }

    #[test]
    fn arm64_linux_needs_fex_as_well_and_fex_goes_outermost() {
        let p = plan(LINUX_ARM, PluginAbi::WindowsX86_64).unwrap();
        assert_eq!(p.needs, vec![Requirement::Fex, Requirement::Wine]);
        let cmd = p.command(
            Path::new("/opt/schist"),
            &["--socket".into(), "9000".into()],
        );
        // Compared piecewise rather than as one string: the helper's
        // path is built with `Path::join`, whose separator is the
        // *running* platform's, and this test runs on all three.
        assert_eq!(cmd[0], "FEXInterpreter");
        assert_eq!(cmd[1], "wine");
        assert!(
            cmd[2].ends_with("schist-8bf-helper-x86_64.exe"),
            "{}",
            cmd[2]
        );
        assert_eq!(&cmd[3..], &["--socket", "9000"]);
    }

    #[test]
    fn apple_silicon_runs_both_kinds_of_mac_plug_in() {
        // Native arm64 needs nothing.
        let arm = plan(MAC_ARM, PluginAbi::MacArm64).unwrap();
        assert_eq!(arm.helper, Helper::MacArm64);
        assert!(arm.needs.is_empty());

        // Intel plug-ins run in an Intel helper under Rosetta — which is
        // the whole reason the helper is a separate process.
        let intel = plan(MAC_ARM, PluginAbi::MacX86_64).unwrap();
        assert_eq!(intel.helper, Helper::MacX86_64);
        assert_eq!(intel.needs, vec![Requirement::Rosetta]);
        let cmd = intel.command(Path::new("/Applications/Schist.app/Contents/MacOS"), &[]);
        assert_eq!(cmd[0], "arch");
        assert_eq!(cmd[1], "-x86_64");
        assert!(cmd[2].ends_with("schist-8bf-helper-x86_64"));
    }

    #[test]
    fn an_intel_mac_cannot_run_an_apple_silicon_plug_in() {
        assert_eq!(plan(MAC_INTEL, PluginAbi::MacX86_64).unwrap().needs, vec![]);
        assert_eq!(
            plan(MAC_INTEL, PluginAbi::MacArm64),
            Err(Unsupported::NoArmEmulationOnIntel)
        );
    }

    #[test]
    fn mac_plug_ins_do_not_run_off_mac() {
        for host in [WIN64, LINUX64, LINUX_ARM] {
            for abi in [PluginAbi::MacArm64, PluginAbi::MacX86_64] {
                assert_eq!(plan(host, abi), Err(Unsupported::MacPluginOffMac));
            }
        }
    }

    #[test]
    fn windows_plug_ins_are_refused_on_mac_rather_than_half_supported() {
        assert_eq!(
            plan(MAC_ARM, PluginAbi::WindowsX86_64),
            Err(Unsupported::NoX86Emulation)
        );
    }

    #[test]
    fn a_wine_helper_sees_paths_through_the_z_drive() {
        let wine = plan(LINUX64, PluginAbi::WindowsX86_64).unwrap();
        assert_eq!(
            wine.helper_path(Path::new("/tmp/schist/Twirl.8bf")),
            r"Z:\tmp\schist\Twirl.8bf"
        );
        // A native helper shares the host's view of the filesystem.
        let native = plan(MAC_ARM, PluginAbi::MacArm64).unwrap();
        assert_eq!(native.helper_path(Path::new("/tmp/x")), "/tmp/x");
        // So does an Intel helper under Rosetta — Rosetta translates
        // instructions, not paths.
        let rosetta = plan(MAC_ARM, PluginAbi::MacX86_64).unwrap();
        assert_eq!(rosetta.helper_path(Path::new("/tmp/x")), "/tmp/x");
    }

    #[test]
    fn every_reason_says_something_a_user_could_act_on() {
        for u in [
            Unsupported::NoArmEmulationOnIntel,
            Unsupported::MacPluginOffMac,
            Unsupported::NoX86Emulation,
            Unsupported::UnknownHost,
        ] {
            let s = u.to_string();
            assert!(s.len() > 20 && !s.contains("Unsupported"), "{s}");
        }
        assert_eq!(
            Requirement::Fex.url(),
            Some("https://github.com/FEX-Emu/FEX")
        );
    }
}
