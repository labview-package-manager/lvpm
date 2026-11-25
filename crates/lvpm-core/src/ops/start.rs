//! `lvpm start`: make the selected LabVIEW answer VI Server.

use super::env::OpEnv;
use crate::target::LvTarget;
use crate::viserver;
use anyhow::{Result, bail};
use std::time::Duration;

/// A LabVIEW that answers VI Server.
#[derive(Debug, Clone)]
pub struct Started {
    pub target: LvTarget,
    pub port: u16,
    /// It was already answering before the command ran, and nothing was started.
    pub was_running: bool,
}

/// Start the LabVIEW the destination flags select, if none is answering, and
/// wait up to `wait` seconds for its VI Server.
pub fn start(env: &OpEnv, wait: u64) -> Result<Started> {
    let Some(target) = env.flagged_roots()?.target else {
        bail!("start needs a real LabVIEW target, not --prefix");
    };
    let was_running = viserver::is_listening(&target);
    let port =
        viserver::ensure_vi_server(&target, Duration::from_secs(wait), env.headless, env.sink)?;
    Ok(Started {
        target,
        port,
        was_running,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_scratch_prefix_has_no_labview_to_start() {
        let mut env = testing::env(Path::new("."));
        env.flags.prefix = Some(PathBuf::from("dest"));
        let why = start(&env, 1).unwrap_err().to_string();
        assert_eq!(why, "start needs a real LabVIEW target, not --prefix");
    }

    #[test]
    fn a_global_alone_settles_as_it_does_for_a_write_and_says_which_labview_when_several() {
        let mut env = testing::env(Path::new("."));
        let mut older = testing::labview_2026();
        older.version = 25.3;
        older.path = PathBuf::from("LabVIEW 2025");
        env.targets.push(older);
        env.flags.global = true;
        let why = start(&env, 1).map(|_| ()).unwrap_err().to_string();
        assert!(
            why.starts_with("which LabVIEW? pass --labview-version <YYYY>"),
            "{why}"
        );
    }
}
