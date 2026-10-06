pub mod config;
pub mod error;
pub mod generated;
pub mod git_diff;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc_compat;
pub mod ident;
pub mod paths;
pub mod sidecar;
pub mod test_paths;
pub mod walk;
