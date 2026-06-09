//! Embedding compute-device selection.
//!
//! CPU is always available and is the library default; GPU execution providers
//! are compiled in only behind the `gpu-*` Cargo features and are selected for
//! the index path via `--device` / `--gpu` / `.vex.toml` / `VEX_DEVICE` (see
//! [`Device::resolve`]). The low-level embedder constructor stays CPU-neutral
//! so callers that bypass `resolve` — notably `vex search` query embedding —
//! never spin up a GPU. See `docs/GPU_SUPPORT.md`.

use anyhow::{bail, Result};

/// Compute device for embedding generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Device {
    /// Force CPU. Produces an empty execution-provider list — byte-for-byte
    /// today's behaviour. The library default (see [`Device::resolve`] for the
    /// index-path default, which differs on GPU-compiled binaries).
    #[default]
    Cpu,
    /// Highest-priority compiled-in execution provider, with silent CPU
    /// fallback if none can register. The index-path default on a GPU build.
    Auto,
    Cuda,
    DirectMl,
    CoreMl,
}

/// Default device for the index path when no `--device`/`--gpu` flag,
/// `.vex.toml`, or `VEX_DEVICE` is set.
///
/// A GPU-compiled binary defaults to [`Device::Auto`] (silent CPU fallback if
/// no GPU is usable); a CPU-only build defaults to [`Device::Cpu`] — today's
/// behaviour. This is the single source of truth shared by [`Device::resolve`]
/// and `vex status`, so the two can never drift.
#[cfg(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda"))]
pub fn default_device() -> Device {
    Device::Auto
}
#[cfg(not(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda")))]
pub fn default_device() -> Device {
    Device::Cpu
}

/// Human-readable summary of GPU support compiled into this binary, for
/// `vex status`. Lists the execution provider(s) baked in, or notes none.
pub const fn gpu_support_str() -> &'static str {
    // cfg! is const-evaluable. If multiple EPs co-compile, the first match is
    // shown; today release builds enable exactly one per target.
    if cfg!(feature = "gpu-directml") {
        "yes (DirectML)"
    } else if cfg!(feature = "gpu-coreml") {
        "yes (CoreML)"
    } else if cfg!(feature = "gpu-cuda") {
        "yes (CUDA)"
    } else {
        "no (none compiled)"
    }
}

/// GPU devices whose execution provider is compiled into THIS binary, in the
/// same priority order [`Device::Auto`] would try them. Empty on a CPU-only
/// build. Used by `vex gpu` to probe each compiled EP individually (a shipped
/// binary has exactly one; the dev `gpu-cuda,gpu-directml` build has two).
// The `#[cfg]`-gated pushes can't collapse into a `vec![]` literal (their
// presence is per-feature), and the CPU-only build leaves `v` unmutated — both
// lints are expected for this conditional-assembly pattern.
#[allow(clippy::vec_init_then_push, unused_mut)]
pub fn compiled_devices() -> Vec<Device> {
    let mut v = Vec::new();
    #[cfg(feature = "gpu-cuda")]
    v.push(Device::Cuda);
    #[cfg(feature = "gpu-directml")]
    v.push(Device::DirectMl);
    #[cfg(feature = "gpu-coreml")]
    v.push(Device::CoreMl);
    v
}

impl Device {
    /// Lowercase canonical name, the inverse of [`Device::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Device::Cpu => "cpu",
            Device::Auto => "auto",
            Device::Cuda => "cuda",
            Device::DirectMl => "directml",
            Device::CoreMl => "coreml",
        }
    }

    /// Parse a device string (case-insensitive, trimmed). `""`/`"auto"` map to
    /// [`Device::Auto`]; `"gpu"` is an alias for `"cuda"`; `"dml"` for
    /// `"directml"`. Errors on anything else.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Device::Auto,
            "cpu" => Device::Cpu,
            "cuda" | "gpu" => Device::Cuda,
            "directml" | "dml" => Device::DirectMl,
            "coreml" => Device::CoreMl,
            other => bail!("unknown device `{other}` (cpu|auto|cuda|directml|coreml)"),
        })
    }

    /// Resolve the device for the **index path**. Precedence (first present
    /// wins):
    ///   1. CLI `--device <x>`        (explicit EP override)
    ///   2. CLI `--gpu` / `--no-gpu`  (boolean: Auto / Cpu)
    ///   3. `.vex.toml` `device = "<x>"`
    ///   4. `.vex.toml` `gpu = true|false`
    ///   5. `VEX_DEVICE` env
    ///   6. default: [`default_device`] — `Auto` in a GPU-compiled binary,
    ///      `Cpu` otherwise.
    ///
    /// The MCP server forwards its `gpu`/`device` args as the equivalent CLI
    /// flags, so it reuses this exact path — there is no separate resolution.
    pub fn resolve(
        cli_device: Option<&str>,
        cli_gpu: Option<bool>,
        cfg_device: Option<&str>,
        cfg_gpu: Option<bool>,
    ) -> Result<Self> {
        if let Some(v) = cli_device {
            return Device::parse(v);
        }
        if let Some(g) = cli_gpu {
            return Ok(if g { Device::Auto } else { Device::Cpu });
        }
        if let Some(v) = cfg_device {
            return Device::parse(v);
        }
        if let Some(g) = cfg_gpu {
            return Ok(if g { Device::Auto } else { Device::Cpu });
        }
        match std::env::var("VEX_DEVICE") {
            Ok(v) => Device::parse(&v),
            Err(_) => Ok(default_device()),
        }
    }
}

/// Build the CUDA execution-provider dispatch.
///
/// VRAM note (see `docs/GPU_SUPPORT.md`): ORT's CUDA arena reserves close to all
/// free device memory on a large cold index, regardless of batch size, and a
/// hard `gpu_mem_limit` below that natural peak fragments and OOMs on
/// long-context batches — so **no cap is imposed by default**. The speed fix is
/// [`crate::embed::batching`] (length-aware micro-batching, ~14× on a large
/// repo). The reserved VRAM is transient (released after indexing); incremental
/// `vex update` embeds few symbols and stays light. `VEX_GPU_MEM_LIMIT` (bytes)
/// is an **advanced opt-in** hard cap for shared GPUs — set it generously
/// (≥ the working set) or it will OOM; `--no-gpu` is the escape hatch for cold
/// builds on a contended card.
#[cfg(feature = "gpu-cuda")]
fn cuda_ep() -> fastembed::ExecutionProviderDispatch {
    use ort::ep::CUDA;
    let cuda = CUDA::default();
    let cuda = match std::env::var("VEX_GPU_MEM_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(limit) if limit > 0 => cuda.with_memory_limit(limit),
        _ => cuda,
    };
    cuda.build()
}

/// Build the ort execution-provider list for `device`.
///
/// Returns an empty `Vec` (== today's CPU path) on a non-GPU build, for
/// [`Device::Cpu`], or for [`Device::Auto`] when no EP is compiled in. An
/// **explicit** non-auto request for an EP that wasn't compiled in errors, so
/// the user isn't silently downgraded to CPU.
#[cfg(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda"))]
pub fn execution_providers(device: Device) -> Result<Vec<fastembed::ExecutionProviderDispatch>> {
    let mut eps = Vec::new();
    match device {
        Device::Cpu => {}
        Device::Auto => {
            // Priority order. Each push is a runtime no-op if the EP can't
            // register (ort warns + falls back to CPU). cfg-gated to the EPs
            // actually compiled in.
            #[cfg(feature = "gpu-cuda")]
            eps.push(cuda_ep());
            #[cfg(feature = "gpu-directml")]
            eps.push(ort::ep::DirectML::default().build());
            #[cfg(feature = "gpu-coreml")]
            eps.push(ort::ep::CoreML::default().build());
        }
        Device::Cuda => {
            #[cfg(feature = "gpu-cuda")]
            eps.push(cuda_ep());
            #[cfg(not(feature = "gpu-cuda"))]
            bail!("vex was not built with CUDA support (rebuild: cargo install vex --features gpu-cuda)");
        }
        Device::DirectMl => {
            #[cfg(feature = "gpu-directml")]
            eps.push(ort::ep::DirectML::default().build());
            #[cfg(not(feature = "gpu-directml"))]
            bail!("vex was not built with DirectML support (rebuild with --features gpu-directml)");
        }
        Device::CoreMl => {
            #[cfg(feature = "gpu-coreml")]
            eps.push(ort::ep::CoreML::default().build());
            #[cfg(not(feature = "gpu-coreml"))]
            bail!("vex was not built with CoreML support (rebuild with --features gpu-coreml)");
        }
    }
    // Diagnostic / strict mode: `VEX_GPU_STRICT` turns ORT's default silent
    // EP-registration fallback into a hard error, so a benchmark (or a user
    // who insists on GPU) can tell whether the provider actually engaged
    // instead of quietly running on CPU. Off by default — normal runs keep the
    // graceful CPU fallback (`error_on_failure = false`).
    if !eps.is_empty() && std::env::var_os("VEX_GPU_STRICT").is_some() {
        eps = eps.into_iter().map(|d| d.error_on_failure()).collect();
    }
    Ok(eps)
}

/// CPU-only build: any GPU request that isn't `Auto`/`Cpu` errors; otherwise an
/// empty list (legacy CPU path). This is the impl `cargo test --workspace`
/// compiles in CI.
#[cfg(not(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda")))]
pub fn execution_providers(device: Device) -> Result<Vec<fastembed::ExecutionProviderDispatch>> {
    match device {
        Device::Auto | Device::Cpu => Ok(Vec::new()),
        _ => bail!("this vex build has no GPU support compiled in (rebuild with a gpu-* feature)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_variants() {
        assert_eq!(Device::parse("auto").unwrap(), Device::Auto);
        assert_eq!(Device::parse("").unwrap(), Device::Auto);
        assert_eq!(Device::parse("  AUTO ").unwrap(), Device::Auto);
        assert_eq!(Device::parse("cpu").unwrap(), Device::Cpu);
        assert_eq!(Device::parse("cuda").unwrap(), Device::Cuda);
        assert_eq!(Device::parse("gpu").unwrap(), Device::Cuda);
        assert_eq!(Device::parse("directml").unwrap(), Device::DirectMl);
        assert_eq!(Device::parse("dml").unwrap(), Device::DirectMl);
        assert_eq!(Device::parse("coreml").unwrap(), Device::CoreMl);
    }

    #[test]
    fn parse_invalid_lists_options() {
        let err = Device::parse("metal").unwrap_err().to_string();
        assert!(err.contains("metal"));
        assert!(err.contains("cpu|auto|cuda|directml|coreml"));
    }

    #[test]
    fn resolve_precedence_cli_device_wins() {
        // CLI device beats a conflicting CLI gpu bool.
        let d = Device::resolve(Some("cpu"), Some(true), Some("cuda"), Some(true)).unwrap();
        assert_eq!(d, Device::Cpu);
    }

    #[test]
    fn resolve_cli_gpu_bool() {
        assert_eq!(
            Device::resolve(None, Some(true), None, None).unwrap(),
            Device::Auto
        );
        assert_eq!(
            Device::resolve(None, Some(false), Some("cuda"), Some(true)).unwrap(),
            Device::Cpu
        );
    }

    #[test]
    fn resolve_falls_through_to_config() {
        assert_eq!(
            Device::resolve(None, None, Some("directml"), None).unwrap(),
            Device::DirectMl
        );
        assert_eq!(
            Device::resolve(None, None, None, Some(true)).unwrap(),
            Device::Auto
        );
        assert_eq!(
            Device::resolve(None, None, None, Some(false)).unwrap(),
            Device::Cpu
        );
    }

    #[test]
    fn execution_providers_cpu_is_empty() {
        assert!(execution_providers(Device::Cpu).unwrap().is_empty());
        // On the default CPU build, Auto also yields an empty list.
        assert!(execution_providers(Device::Auto).unwrap().is_empty());
    }

    #[cfg(not(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda")))]
    #[test]
    fn execution_providers_explicit_gpu_errors_on_cpu_build() {
        assert!(execution_providers(Device::Cuda).is_err());
        assert!(execution_providers(Device::DirectMl).is_err());
        assert!(execution_providers(Device::CoreMl).is_err());
    }
}
