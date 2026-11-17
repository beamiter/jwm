//! Where frames are shrunk or enlarged: auto, gpu or cpu.

use x11rb::connection::RequestConnection;
use x11rb::rust_connection::RustConnection;

/// Where frames are scaled to the target dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScalingPolicy {
    /// Use XRender downscaling/upscaling, except on NVIDIA's proprietary driver
    /// where operations on composited or offscreen pixmaps can return unfinished frames.
    #[default]
    Auto,
    /// Always use XRender scaling in the X server.
    Gpu,
    /// Always scale on the CPU.
    Cpu,
}

impl ScalingPolicy {
    /// Host compatibility readback uses core GetImage, not MIT-SHM. This
    /// preserves the tested core path; it does not diagnose SHM as faulty.
    #[must_use]
    pub(crate) fn requires_core_readback(self, nvidia_driver: bool) -> bool {
        matches!(self, Self::Cpu) || (self == Self::Auto && nvidia_driver)
    }

    #[must_use]
    pub fn allows_gpu(self, nvidia_driver: bool) -> bool {
        match self {
            Self::Gpu => true,
            Self::Cpu => false,
            Self::Auto => !nvidia_driver,
        }
    }
}

/// Whether the X server runs NVIDIA's proprietary driver, by the extensions
/// only that driver registers.
#[must_use]
pub fn nvidia_driver(conn: &RustConnection) -> bool {
    ["NV-GLX", "NV-CONTROL"]
        .iter()
        .any(|name| conn.extension_information(name).ok().flatten().is_some())
}

impl std::str::FromStr for ScalingPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "gpu" | "xrender" => Ok(Self::Gpu),
            "cpu" => Ok(Self::Cpu),
            _ => Err(format!(
                "invalid scaling policy {value:?}; choose auto, gpu or cpu"
            )),
        }
    }
}

impl std::fmt::Display for ScalingPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Gpu => write!(f, "gpu"),
            Self::Cpu => write!(f, "cpu"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scaling_avoids_xrender_only_on_the_nvidia_driver() {
        assert!(ScalingPolicy::Auto.allows_gpu(false));
        assert!(!ScalingPolicy::Auto.allows_gpu(true));
        // An explicit choice wins either way.
        assert!(ScalingPolicy::Gpu.allows_gpu(true));
        assert!(!ScalingPolicy::Cpu.allows_gpu(false));
        assert_eq!("XRender".parse(), Ok(ScalingPolicy::Gpu));
        assert_eq!("gpu".parse(), Ok(ScalingPolicy::Gpu));
        assert_eq!("cpu".parse(), Ok(ScalingPolicy::Cpu));
        assert_eq!("auto".parse(), Ok(ScalingPolicy::Auto));
        assert!("fast".parse::<ScalingPolicy>().is_err());
    }

    #[test]
    fn host_readback_compatibility_policy_matrix() {
        for (policy, nvidia, core) in [
            (ScalingPolicy::Cpu, false, true),
            (ScalingPolicy::Cpu, true, true),
            (ScalingPolicy::Auto, false, false),
            (ScalingPolicy::Auto, true, true),
            (ScalingPolicy::Gpu, false, false),
            (ScalingPolicy::Gpu, true, false),
        ] {
            assert_eq!(policy.requires_core_readback(nvidia), core);
            if core {
                assert!(!policy.allows_gpu(nvidia));
            }
        }
    }

    #[test]
    fn scaling_policy_formats_expected_string() {
        assert_eq!(ScalingPolicy::Auto.to_string(), "auto");
        assert_eq!(ScalingPolicy::Gpu.to_string(), "gpu");
        assert_eq!(ScalingPolicy::Cpu.to_string(), "cpu");
    }
}
