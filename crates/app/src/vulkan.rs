//! A look at Vulkan before GPUI needs it, so a machine that cannot render
//! gets a sentence instead of a panic.
//!
//! GPUI draws through Blade, and on Linux Blade is Vulkan or nothing.
//! When it finds nothing it returns `NoSupportedDeviceFound`, which the
//! Wayland and X11 backends `.expect()`: what the user sees is a panic
//! and a file path into somebody else's cargo checkout.
//!
//! Two things put a system there, and neither is a GPU too old for
//! Vulkan.
//!
//! The first is an install with no Vulkan *driver*. The loader —
//! `vulkan-icd-loader`, `libvulkan1` — arrives as a dependency of half
//! the desktop, while the driver is a separate package (`vulkan-radeon`,
//! `mesa-vulkan-drivers`, ...) that minimal installs and virtual machines
//! routinely lack.
//!
//! The second is `VK_DRIVER_FILES` or `VK_ICD_FILENAMES` naming a
//! manifest that is not there. Those variables *replace* the loader's
//! search rather than adding to it, so one stale path in a session's
//! environment leaves every Vulkan program on the machine with no driver
//! at all — with the driver package installed and sitting in
//! `/usr/share/vulkan/icd.d`, untouched. Told "install a driver" the
//! reader would install the one they already have, so it is worth
//! telling the two apart.
//!
//! Either way the loader ends up advertising no `VK_KHR_surface`, since
//! it only exposes the surface extensions on an ICD's behalf, and Blade
//! stops on that. So look first, and name what is actually wrong. Only
//! hopeless answers are caught here: a driver that merely lacks something
//! Schist wants is Blade's call, not ours.

use ash::vk;

/// What the probe found. Everything but [`Verdict::Usable`] is fatal —
/// Blade fails on the same machine for the same reason moments later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A driver is installed and offers at least one device.
    Usable,
    /// No `libvulkan.so.1` to load: the loader itself is missing.
    NoLoader,
    /// The loader is installed and has no driver to talk to, with
    /// whatever the environment had to say about where drivers are.
    NoDriver(Option<DriverOverride>),
    /// A driver answered, but no physical device came back.
    NoDevice,
}

/// A `VK_DRIVER_FILES` / `VK_ICD_FILENAMES` setting found in the
/// environment, and which of the paths it names are not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverOverride {
    var: &'static str,
    value: String,
    missing: Vec<String>,
}

/// The variables that replace the loader's own search for drivers.
/// `VK_DRIVER_FILES` is the current name and `VK_ICD_FILENAMES` the
/// deprecated one the loader still honours — and still the one most
/// config files in the wild set. First one set wins, as in the loader.
const DRIVER_FILE_VARS: [&str; 2] = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"];

/// Where the loader looks when nothing overrides it, quoted in the advice
/// because two empty directories are the whole diagnosis.
const ICD_DIRS: &str = "    /usr/share/vulkan/icd.d\n    /etc/vulkan/icd.d";

/// Escape hatch, in case this probe is ever wrong about a system Blade
/// would in fact have run on.
const SKIP_VAR: &str = "SCHIST_SKIP_VULKAN_CHECK";

/// Ask the loader whether anything can draw.
fn probe() -> Verdict {
    // SAFETY: `Entry::load` dlopens the loader, and the instance created
    // below is destroyed before returning. Nothing else holds either.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return Verdict::NoLoader;
        };
        let Ok(extensions) = entry.enumerate_instance_extension_properties(None) else {
            return Verdict::NoDriver(driver_override());
        };
        // `VK_KHR_surface` is the tell. The loader implements it, but only
        // advertises it when some ICD is there to present through, so its
        // absence means the driver list is empty.
        let has_surface = extensions
            .iter()
            .any(|ext| ext.extension_name_as_c_str() == Ok(vk::KHR_SURFACE_NAME));
        if !has_surface {
            return Verdict::NoDriver(driver_override());
        }
        // Asking for 1.0 keeps this probe about *existence*: a driver too
        // old for Blade is still a driver, and saying "install one" would
        // be the wrong advice. Nothing is enabled, so the only realistic
        // failures are a broken ICD or no memory, and Blade's richer
        // instance would not survive either.
        let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_0);
        let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let Ok(instance) = entry.create_instance(&create_info, None) else {
            return Verdict::NoDevice;
        };
        let devices = instance.enumerate_physical_devices();
        instance.destroy_instance(None);
        match devices {
            Ok(devices) if !devices.is_empty() => Verdict::Usable,
            _ => Verdict::NoDevice,
        }
    }
}

/// Whatever the environment says about where the drivers are.
fn driver_override() -> Option<DriverOverride> {
    let (var, value) = DRIVER_FILE_VARS
        .iter()
        .find_map(|var| Some((*var, std::env::var(var).ok()?)))?;
    let missing = missing_paths(&value);
    Some(DriverOverride {
        var,
        value,
        missing,
    })
}

/// The entries of a driver-files variable that do not exist. Each is a
/// path to a manifest or to a directory of them, separated as `PATH` is;
/// an empty entry is what a trailing separator leaves behind and means
/// nothing.
fn missing_paths(value: &str) -> Vec<String> {
    value
        .split(':')
        .filter(|entry| !entry.is_empty())
        .filter(|entry| !std::path::Path::new(entry).exists())
        .map(str::to_string)
        .collect()
}

/// What to tell someone whose system cannot render, and what to do about it.
fn advice(verdict: &Verdict) -> String {
    let body = match verdict {
        // Unreachable through `check`, and cheaper to answer than to prove
        // unreachable.
        Verdict::Usable => String::new(),
        Verdict::NoLoader => "\
schist: no Vulkan loader on this system, so there is nothing to draw with.

Schist renders through Vulkan, and `libvulkan.so.1` is not installed.
Both the loader and a driver are needed:

    Arch, CachyOS, Omarchy    sudo pacman -S vulkan-icd-loader vulkan-driver
    Debian, Ubuntu, Mint      sudo apt install libvulkan1 mesa-vulkan-drivers
    Fedora, RHEL              sudo dnf install vulkan-loader mesa-vulkan-drivers
"
        .to_string(),
        // The driver list was named by the environment, and some of it is
        // not there. Almost certainly the whole problem, and installing a
        // package would not fix it -- so say this instead, not as well.
        Verdict::NoDriver(Some(over)) if !over.missing.is_empty() => {
            let missing = over
                .missing
                .iter()
                .map(|path| format!("    {path}\n"))
                .collect::<String>();
            format!(
                "\
schist: the Vulkan driver this session points at is not there.

{var} is set, and setting it *replaces* the loader's search for
drivers rather than adding to it. So these files, which do not exist,
are the whole driver list -- and every Vulkan program in this session
sees no driver at all, however many are installed:

{missing}
    {var}={value}

Point it at a manifest that exists -- installed drivers put theirs in
/usr/share/vulkan/icd.d -- or unset it and let the loader find its own.
A session that sets this from a config file (Hyprland's `env =`, a
systemd environment.d drop-in, a shell profile) has to be told there,
and the change takes a fresh login to reach anything already running.
",
                var = over.var,
                value = over.value,
            )
        }
        Verdict::NoDriver(over) => {
            // The paths all exist, so what is in them is for some other
            // machine -- still worth naming, since unsetting it is a
            // faster thing to try than a package install.
            let overridden = match over {
                Some(over) => format!(
                    "\n{var} is set to {value}, and that replaces the loader's own
search: if none of those manifests is for this machine, unsetting it is
the first thing to try.\n",
                    var = over.var,
                    value = over.value,
                ),
                None => String::new(),
            };
            format!(
                "\
schist: no Vulkan driver installed, so there is nothing to draw on.

Schist renders through Vulkan. The loader is installed and reports no
driver -- nothing has registered one in either place they are looked for:

{ICD_DIRS}

The loader ships separately from the drivers, so this is usually a single
missing package.

Install the one for this machine's GPU:

    Arch, CachyOS, Omarchy    sudo pacman -S vulkan-driver
    Debian, Ubuntu, Mint      sudo apt install mesa-vulkan-drivers
    Fedora, RHEL              sudo dnf install mesa-vulkan-drivers

NVIDIA's proprietary driver carries its own (`nvidia-utils` on Arch,
`nvidia-driver` on Debian). In a virtual machine, or anywhere with no
GPU driver to install, the software rasteriser is the one that works:
`vulkan-swrast` on Arch, part of `mesa-vulkan-drivers` elsewhere. It is
slow, but it starts.
{overridden}"
            )
        }
        Verdict::NoDevice => "\
schist: a Vulkan driver is installed but offers no device to render on.

The driver may not cover this GPU, or may not be able to reach it --
over SSH, or inside a container, /dev/dri is a common thing to be
missing. `vulkaninfo --summary` reports what the loader sees.

The software rasteriser renders without a GPU at all, if that is what
this machine has: `vulkan-swrast` on Arch, part of `mesa-vulkan-drivers`
on Debian and Fedora.
"
        .to_string(),
    };
    format!("{body}\nTo start Schist anyway and let it fail its own way, set {SKIP_VAR}=1.\n")
}

/// Refuse to start, with an explanation, when Vulkan cannot possibly work.
///
/// Called before anything is opened or written, so exiting here costs the
/// user nothing.
pub fn check() {
    if std::env::var(SKIP_VAR).is_ok_and(|v| v == "1") {
        log::warn!("{SKIP_VAR}=1: starting without checking for a Vulkan driver");
        return;
    }
    let verdict = probe();
    if verdict == Verdict::Usable {
        return;
    }
    log::error!("no usable Vulkan setup: {verdict:?}");
    eprint!("{}", advice(&verdict));
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::{advice, missing_paths, probe, DriverOverride, Verdict, SKIP_VAR};

    fn over(value: &str, missing: &[&str]) -> Option<DriverOverride> {
        Some(DriverOverride {
            var: "VK_ICD_FILENAMES",
            value: value.to_string(),
            missing: missing.iter().map(|s| s.to_string()).collect(),
        })
    }

    /// Every fatal verdict has to name something to do; advice that only
    /// says "no driver" leaves the reader exactly where they were.
    #[test]
    fn every_failure_names_a_way_out() {
        for verdict in [
            Verdict::NoLoader,
            Verdict::NoDriver(None),
            Verdict::NoDevice,
        ] {
            let text = advice(&verdict);
            assert!(text.starts_with("schist: "), "{verdict:?}: {text}");
            assert!(text.contains("mesa-vulkan-drivers"), "{verdict:?}: {text}");
            assert!(text.contains(SKIP_VAR), "{verdict:?}: {text}");
        }
    }

    /// The loader's own directories are the first thing to look in, so the
    /// advice for an empty driver list says where it looked.
    #[test]
    fn a_missing_driver_says_where_drivers_live() {
        let text = advice(&Verdict::NoDriver(None));
        assert!(text.contains("/usr/share/vulkan/icd.d"), "{text}");
        assert!(text.contains("vulkan-driver"), "{text}");
    }

    /// A driver named by the environment and not present is a different
    /// bug with a different fix, and "install a driver" is the wrong
    /// advice for it -- the driver is usually already installed.
    #[test]
    fn a_bad_override_is_reported_as_itself() {
        let text = advice(&Verdict::NoDriver(over(
            "/usr/share/vulkan/icd.d/lvp_icd.aarch64.json",
            &["/usr/share/vulkan/icd.d/lvp_icd.aarch64.json"],
        )));
        assert!(text.contains("VK_ICD_FILENAMES"), "{text}");
        assert!(text.contains("lvp_icd.aarch64.json"), "{text}");
        assert!(!text.contains("sudo pacman -S vulkan-driver"), "{text}");
    }

    /// An override whose files all exist is not the diagnosis, but it is
    /// still worth mentioning: it outranks whatever is installed.
    #[test]
    fn an_intact_override_is_mentioned_alongside_the_packages() {
        let text = advice(&Verdict::NoDriver(over("/etc/vulkan/icd.d", &[])));
        assert!(text.contains("mesa-vulkan-drivers"), "{text}");
        assert!(text.contains("unsetting it"), "{text}");
    }

    /// The variable holds a `PATH`-style list, and a trailing separator is
    /// not a missing file.
    #[test]
    fn only_paths_that_are_really_absent_count() {
        assert_eq!(missing_paths(""), Vec::<String>::new());
        assert_eq!(missing_paths("/usr/share:"), Vec::<String>::new());
        assert_eq!(
            missing_paths("/usr/share:/nope/a.json"),
            vec!["/nope/a.json".to_string()]
        );
    }

    /// Not an assertion about this machine -- CI runners have no GPU --
    /// only that probing one is safe to do and returns.
    #[test]
    fn probing_is_harmless() {
        let _ = probe();
    }
}
