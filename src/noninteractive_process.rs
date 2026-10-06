use std::{ffi::OsStr, process::Command};

/// Builds a subprocess whose stdio is controlled by the caller and which never opens a Windows console.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    crate::platform::configure_background_command(&mut command);
    command
}

pub(crate) fn curl_command() -> Command {
    #[cfg(test)]
    {
        if let Some(program) = CURL_PROGRAM_OVERRIDE.with(|cell| cell.borrow().clone()) {
            return command(program);
        }
    }
    command("curl")
}

#[cfg(test)]
thread_local! {
    /// Test-only redirect for `curl_command()`, per thread. A test intercepts
    /// its own fetches without touching the process environment: every
    /// concurrently running test reads PATH and TMPDIR, so a process-wide
    /// redirect would make one test's fixture delete another test's temp dir.
    static CURL_PROGRAM_OVERRIDE: std::cell::RefCell<Option<std::ffi::OsString>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only: route `curl_command()` to `program` on THIS thread until the
/// returned guard drops (which restores whatever was set before).
#[cfg(test)]
pub(crate) struct CurlProgramOverride(Option<std::ffi::OsString>);

#[cfg(test)]
impl CurlProgramOverride {
    pub(crate) fn new(program: impl AsRef<OsStr>) -> Self {
        Self(
            CURL_PROGRAM_OVERRIDE
                .with(|cell| cell.borrow_mut().replace(program.as_ref().to_os_string())),
        )
    }
}

#[cfg(test)]
impl Drop for CurlProgramOverride {
    fn drop(&mut self) {
        CURL_PROGRAM_OVERRIDE.with(|cell| *cell.borrow_mut() = self.0.take());
    }
}
