//! A look at Vulkan before GPUI needs it, so a machine with no driver
//! gets a sentence instead of a panic.
//!
//! GPUI draws through Blade, and on Linux Blade is Vulkan or nothing.
//! When it finds nothing it returns `NoSupportedDeviceFound`, which the
//! Wayland and X11 backends `.expect()`: what the user sees is a panic
//! and a file path into somebody else's cargo checkout.
//!
//! The cause is almost never a GPU that cannot run Vulkan. It is an
//! install with no Vulkan *driver*. The loader — `vulkan-icd-loader`,
//! `libvulkan1` — arrives as a dependency of half the desktop, while the
//! driver is a separate package (`vulkan-radeon`, `mesa-vulkan-drivers`,
//! ...) that minimal installs and virtual machines routinely lack. With
//! no driver registered, the loader does not even offer `VK_KHR_surface`,
//! since it only exposes the surface extensions on an ICD's behalf, and
//! Blade stops on that.
//!
//! So look first, and if the answer is hopeless, name the missing package
//! and exit. Only hopeless answers are caught here: a driver that merely
//! lacks something Schist wants is Blade's call, not ours.

use ash::vk;

/// What the probe found. Everything but [`Verdict::Usable`] is fatal —
/// Blade fails on the same machine for the same reason moments later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A driver is installed and offers at least one device.
    Usable,
    /// No `libvulkan.so.1` to load: the loader itself is missing.
    NoLoader,
    /// The loader is installed and has no driver to talk to.
    NoDriver,
    /// A driver answered, but no physical device came back.
    NoDevice,
}

/// Ask the loader whether anything can draw.
fn probe() -> Verdict {
    // SAFETY: `Entry::load` dlopens the loader, and the instance created
    // below is destroyed before returning. Nothing else holds either.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return Verdict::NoLoader;
        };
        let Ok(extensions) = entry.enumerate_instance_extension_properties(None) else {
            return Verdict::NoDriver;
        };
        // `VK_KHR_surface` is the tell. The loader implements it, but only
        // advertises it when some ICD is there to present through, so its
        // absence means the driver list is empty.
        let has_surface = extensions
            .iter()
            .any(|ext| ext.extension_name_as_c_str() == Ok(vk::KHR_SURFACE_NAME));
        if !has_surface {
            return Verdict::NoDriver;
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

/// What to tell someone whose system cannot render, and what to do about it.
fn advice(verdict: Verdict) -> String {
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
        Verdict::NoDriver => format!(
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
"
        ),
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

/// Where the loader looks for driver manifests, quoted in the advice
/// because two empty directories are the whole diagnosis.
const ICD_DIRS: &str = "    /usr/share/vulkan/icd.d\n    /etc/vulkan/icd.d";

/// Escape hatch, in case this probe is ever wrong about a system Blade
/// would in fact have run on.
const SKIP_VAR: &str = "SCHIST_SKIP_VULKAN_CHECK";

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
    eprint!("{}", advice(verdict));
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::{advice, probe, Verdict, SKIP_VAR};

    /// Every fatal verdict has to name a package to install; advice that
    /// only says "no driver" leaves the reader exactly where they were.
    #[test]
    fn every_failure_names_something_to_install() {
        for verdict in [Verdict::NoLoader, Verdict::NoDriver, Verdict::NoDevice] {
            let text = advice(verdict);
            assert!(text.starts_with("schist: "), "{verdict:?}: {text}");
            assert!(text.contains("mesa-vulkan-drivers"), "{verdict:?}: {text}");
            assert!(text.contains(SKIP_VAR), "{verdict:?}: {text}");
        }
    }

    /// The loader's own directories are the first thing to look in, so the
    /// advice for an empty driver list says where it looked.
    #[test]
    fn a_missing_driver_says_where_drivers_live() {
        let text = advice(Verdict::NoDriver);
        assert!(text.contains("/usr/share/vulkan/icd.d"), "{text}");
        assert!(text.contains("vulkan-driver"), "{text}");
    }

    /// Not an assertion about this machine -- CI runners have no GPU --
    /// only that probing one is safe to do and returns.
    #[test]
    fn probing_is_harmless() {
        let _ = probe();
    }
}
