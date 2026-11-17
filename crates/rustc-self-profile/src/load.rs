//! Reading `.mm_profdata` files with analyzeme.

use crate::Profile;
use alloc::string::{String, ToString};
use core::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A profile that couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoadError {
    /// The path given to [`Profile::load`].
    pub path: PathBuf,
    /// What analyzeme said.
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reading {}: {}", self.path.display(), self.message)
    }
}

impl core::error::Error for LoadError {}

impl Profile {
    /// Load a profile written by `-Zself-profile`. `path` is the file
    /// without its `.mm_profdata` extension (`target/prof/app-12345`), as
    /// analyzeme takes it.
    ///
    /// Every interval event is kept; the first argument only for
    /// [`Profile::keeps_arg`] labels. Times are nanoseconds since the
    /// profile started.
    ///
    /// # Errors
    ///
    /// When the file is missing or not a profile analyzeme can read.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let data = analyzeme::ProfilingData::new(path).map_err(|e| LoadError {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let start_time = data.metadata().start_time;
        let mut profile = Self {
            cmd: data.metadata().cmd.clone(),
            ..Self::default()
        };
        let ns = |t: SystemTime| {
            t.duration_since(start_time)
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        };
        for event in data.iter_full() {
            let analyzeme::EventPayload::Timestamp(analyzeme::Timestamp::Interval { start, end }) =
                event.payload
            else {
                continue;
            };
            let arg = if Self::keeps_arg(&event.label) {
                event.additional_data.first().map(|a| &**a)
            } else {
                None
            };
            profile.push(&event.label, arg, ns(start), ns(end), event.thread_id);
        }
        Ok(profile)
    }
}
