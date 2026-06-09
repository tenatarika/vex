//! `vex gpu` — GPU execution-provider diagnostics ("doctor").
//!
//! Reports which GPU EP is compiled into this binary, then *actively probes*
//! each compiled EP — forcing strict registration (`VEX_GPU_STRICT`) so a
//! silent CPU fallback is reported as a failure rather than hidden — and prints
//! targeted remediation for the common setup problems (a stale/missing
//! DirectML.dll, a missing CUDA/cuDNN runtime, etc.). With `--enable`, persists
//! the working device to the `VEX_DEVICE` environment variable for future runs.
//!
//! This exists because the index path falls back to CPU *silently* when an EP
//! cannot register (so a misconfigured GPU just looks "slow"). `vex gpu` is the
//! supported way to find out whether the GPU is actually being used.
//! See `docs/GPU_SUPPORT.md` §6.

use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use crate::embed::device::{compiled_devices, default_device, gpu_support_str, Device};
use crate::embed::{make_embedder_with_device, MINILM_ID};

pub(crate) fn gpu(enable: bool) -> Result<()> {
    println!("vex GPU diagnostics");
    println!("  build:          {}", gpu_support_str());
    println!("  default device: {}", default_device().as_str());

    let devices = compiled_devices();
    if devices.is_empty() {
        println!();
        println!("This build has no GPU execution provider compiled in — embedding runs on CPU.");
        println!("To get GPU acceleration:");
        println!(
            "  • NVIDIA (CUDA):    cargo install vex --features gpu-cuda  \
             (needs CUDA Toolkit 12 + cuDNN 9 on PATH)"
        );
        println!("  • Any Windows GPU:  use the prebuilt Windows binary (DirectML, driver-only)");
        println!("  • Apple Silicon:    cargo install vex --features gpu-coreml");
        return Ok(());
    }

    // Force strict EP registration so the probe distinguishes "engaged" from
    // "silently fell back to CPU". Safe: this is a one-shot CLI process.
    std::env::set_var("VEX_GPU_STRICT", "1");

    println!();
    let mut first_ok: Option<Device> = None;
    for device in &devices {
        print!("  probing {:<9} ... ", device.as_str());
        let _ = std::io::stdout().flush();
        match probe(*device) {
            Ok(warmup_ms) => {
                println!("OK — engaged ({warmup_ms} ms warm-up)");
                if first_ok.is_none() {
                    first_ok = Some(*device);
                }
            }
            Err(e) => {
                println!("FAILED");
                remediation(*device, &e.to_string());
            }
        }
    }

    println!();
    match first_ok {
        Some(device) => {
            println!("\u{2714} GPU available via {}.", device.as_str());
            if enable {
                persist_device(device);
            } else {
                println!(
                    "  It engages automatically (default device is `{}`). \
                     Run `vex gpu --enable` to pin VEX_DEVICE={} for all projects.",
                    default_device().as_str(),
                    device.as_str()
                );
            }
        }
        None => {
            println!("\u{2718} No GPU execution provider engaged — vex will embed on CPU.");
            println!(
                "  Resolve the issue(s) above, or set VEX_DEVICE=cpu / pass --no-gpu \
                 to stop attempting GPU."
            );
        }
    }
    Ok(())
}

/// Build a MiniLM embedder on `device` (strict, via `VEX_GPU_STRICT`) and run
/// one inference. Returns the model-load warm-up in ms. Any EP-registration
/// failure surfaces as `Err` because strict mode is on. MiniLM is used because
/// it is the smallest model — the probe tests the *provider*, not throughput;
/// it downloads ~86 MB on first use if the model isn't cached.
fn probe(device: Device) -> Result<u128> {
    let start = Instant::now();
    let mut embedder = make_embedder_with_device(MINILM_ID, device)?;
    let warmup_ms = start.elapsed().as_millis();
    embedder.embed("fn probe() { let _ = 0; }")?;
    Ok(warmup_ms)
}

/// Print EP-specific setup guidance for a failed probe.
fn remediation(device: Device, err: &str) {
    let low = err.to_ascii_lowercase();
    println!("    └ {err}");
    match device {
        Device::Cuda => {
            println!("    → CUDA EP could not initialize — usually a missing runtime dependency:");
            println!("      • CUDA Toolkit 12.x  (cudart64_12 / cublas64_12 / cufft64_11)");
            println!(
                "      • cuDNN 9            (cudnn64_9.dll) — e.g. `pip install nvidia-cudnn-cu12`"
            );
            println!("      • put both bin dirs on PATH; ensure the NVIDIA driver is current");
        }
        Device::DirectMl => {
            if low.contains("887a0004")
                || low.contains("feature level")
                || low.contains("not supported")
            {
                println!("    → DirectML device creation failed (feature level unsupported):");
                println!(
                    "      • ensure the redistributable DirectML.dll sits next to vex.exe — the"
                );
                println!(
                    "        in-box C:\\Windows\\System32\\DirectML.dll is too old for current ORT"
                );
                println!(
                    "      • update your GPU driver (plain Remote Desktop without a GPU-backed"
                );
                println!("        session has no Direct3D 12 device and cannot use DirectML)");
            } else {
                println!(
                    "    → DirectML EP could not initialize — ensure a DX12 GPU + current driver,"
                );
                println!("      and that the redistributable DirectML.dll is beside vex.exe");
            }
        }
        Device::CoreMl => {
            println!("    → CoreML EP could not initialize (expected off Apple hardware).");
        }
        Device::Cpu | Device::Auto => {}
    }
}

/// Persist `VEX_DEVICE` at the user level so all future runs pick it up.
fn persist_device(device: Device) {
    let value = device.as_str();
    #[cfg(windows)]
    {
        // setx writes HKCU\Environment; it affects NEW shells, not this one.
        match std::process::Command::new("setx")
            .args(["VEX_DEVICE", value])
            .status()
        {
            Ok(status) if status.success() => println!(
                "  Pinned VEX_DEVICE={value} (user environment). \
                 Open a NEW terminal for it to take effect."
            ),
            _ => println!("  Could not run setx — set it manually: setx VEX_DEVICE {value}"),
        }
    }
    #[cfg(not(windows))]
    {
        println!("  Add to your shell profile to set it globally:");
        println!("      export VEX_DEVICE={value}");
    }
}
