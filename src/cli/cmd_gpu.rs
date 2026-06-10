//! `vex gpu` — GPU execution-provider diagnostics ("doctor").
//!
//! Reports which GPU EP is compiled into this binary, then *actively probes*
//! each compiled EP — requesting strict registration (threaded as a parameter
//! to the embedder constructor) so a silent CPU fallback is reported as a
//! failure rather than hidden — and prints targeted remediation for the common
//! setup problems (a stale/missing DirectML.dll, a missing CUDA/cuDNN runtime,
//! etc.). `vex gpu <device>` narrows the probe to a single EP (and reports
//! clearly if it isn't in this build). With `--enable`, persists the working
//! device to the `VEX_DEVICE` environment variable for future runs — and when
//! nothing engages, says so instead of silently doing nothing. Under
//! `--format json` the same diagnosis is emitted as a standard
//! `MetaEnvelope`-wrapped payload so MCP agents can gate on it.
//!
//! This exists because the index path falls back to CPU *silently* when an EP
//! cannot register (so a misconfigured GPU just looks "slow"). `vex gpu` is the
//! supported way to find out whether the GPU is actually being used.
//! See `docs/GPU_SUPPORT.md` §6.

use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use super::args::OutputFormat;
use super::common::CmdCtx;
use super::output::print_envelope;
use crate::embed::device::{compiled_devices, gpu_support_str, Device, DEFAULT_DEVICE};
use crate::embed::{make_embedder_with_device, MINILM_ID};
use crate::protocol::{capabilities, MetaEnvelope};

/// One probed execution provider: either a warm-up time (engaged) or the
/// registration error (failed — strict mode turns the fallback into an `Err`).
struct ProbeOutcome {
    device: Device,
    warmup_ms: Option<u128>,
    error: Option<String>,
}

pub(crate) fn gpu(ctx: &CmdCtx<'_>, device: Option<String>, enable: bool) -> Result<()> {
    let json = matches!(ctx.format, OutputFormat::Json);
    let compiled = compiled_devices();

    if !json {
        println!("vex GPU diagnostics");
        println!("  build:          {}", gpu_support_str());
        println!("  default device: {}", DEFAULT_DEVICE.as_str());
    }

    // Decide which EP(s) to probe. `vex gpu <device>` narrows to one (and
    // reports clearly when that EP isn't in this build); otherwise probe every
    // compiled-in EP, in the same priority order `Auto` would try them.
    let targets: Vec<Device> = match device.as_deref() {
        None => compiled.clone(),
        Some(name) => match Device::parse(name)? {
            Device::Auto => compiled.clone(),
            Device::Cpu => {
                let note = "`cpu` has no execution provider to probe.";
                if json {
                    print_gpu_envelope(&compiled, &[], None, false, Some(note));
                } else {
                    println!();
                    println!("{note}");
                }
                return Ok(());
            }
            requested if compiled.contains(&requested) => vec![requested],
            requested => {
                // A specific GPU EP this binary wasn't built with — the index
                // path would hard-error on `--device {requested}`; say so here.
                let note = format!(
                    "This binary was not built with {} support.",
                    requested.as_str()
                );
                if json {
                    // The JSON note carries the explicit --enable
                    // acknowledgement too — `pinned: false` alone doesn't say
                    // WHY nothing was pinned.
                    let note = if enable {
                        format!(
                            "{note} --enable: nothing to pin — {} isn't compiled into this build.",
                            requested.as_str()
                        )
                    } else {
                        note
                    };
                    print_gpu_envelope(&compiled, &[], None, false, Some(&note));
                } else {
                    println!();
                    println!("{note}");
                    print_install_help();
                    if enable {
                        println!();
                        println!(
                            "  --enable: nothing to pin — {} isn't compiled into this build.",
                            requested.as_str()
                        );
                    }
                }
                return Ok(());
            }
        },
    };

    if targets.is_empty() {
        let note = "This build has no GPU execution provider compiled in — embedding runs on CPU.";
        if json {
            let note = if enable {
                format!(
                    "{note} --enable: no GPU support in this build to pin — \
                     install a gpu-* build first."
                )
            } else {
                note.to_string()
            };
            print_gpu_envelope(&compiled, &[], None, false, Some(&note));
        } else {
            println!();
            println!("{note}");
            print_install_help();
            if enable {
                println!();
                println!(
                    "  --enable: no GPU support in this build to pin — install a gpu-* build first."
                );
            }
        }
        return Ok(());
    }

    if !json {
        println!();
    }
    let mut probes: Vec<ProbeOutcome> = Vec::with_capacity(targets.len());
    let mut first_ok: Option<Device> = None;
    for d in &targets {
        if !json {
            print!("  probing {:<9} ... ", d.as_str());
            let _ = std::io::stdout().flush();
        }
        match probe(*d) {
            Ok(warmup_ms) => {
                if !json {
                    println!("OK — engaged ({warmup_ms} ms warm-up)");
                }
                if first_ok.is_none() {
                    first_ok = Some(*d);
                }
                probes.push(ProbeOutcome {
                    device: *d,
                    warmup_ms: Some(warmup_ms),
                    error: None,
                });
            }
            Err(e) => {
                if !json {
                    println!("FAILED");
                    remediation(*d, &e.to_string());
                }
                probes.push(ProbeOutcome {
                    device: *d,
                    warmup_ms: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    if !json {
        println!();
    }
    let mut pinned = false;
    let mut note: Option<String> = None;
    match first_ok {
        Some(d) => {
            if !json {
                println!("\u{2714} GPU available via {}.", d.as_str());
            }
            if enable {
                pinned = persist_device(d, json);
            } else if !json {
                println!(
                    "  It engages automatically (default device is `{}`). \
                     Run `vex gpu --enable` to pin VEX_DEVICE={} for all projects.",
                    DEFAULT_DEVICE.as_str(),
                    d.as_str()
                );
            }
        }
        None => {
            if enable {
                // The user asked to pin a GPU but none engaged — say so
                // explicitly instead of silently dropping --enable (pinning a
                // dead device would just force a CPU fallback on every run).
                note = Some(
                    "--enable: not pinning VEX_DEVICE — no GPU engaged. Fix the issue(s) \
                     reported per probe, then re-run `vex gpu --enable`."
                        .to_string(),
                );
            } else {
                note = Some(
                    "No GPU execution provider engaged — vex will embed on CPU. Resolve \
                     the per-probe error(s), or set VEX_DEVICE=cpu / pass --no-gpu to \
                     stop attempting GPU."
                        .to_string(),
                );
            }
            if !json {
                println!("\u{2718} No GPU execution provider engaged — vex will embed on CPU.");
                if enable {
                    println!(
                        "  --enable: not pinning VEX_DEVICE — no GPU engaged. Fix the issue(s) \
                         above, then re-run `vex gpu --enable`."
                    );
                } else {
                    println!(
                        "  Resolve the issue(s) above, or set VEX_DEVICE=cpu / pass --no-gpu \
                         to stop attempting GPU."
                    );
                }
            }
        }
    }
    if json {
        print_gpu_envelope(&compiled, &probes, first_ok, pinned, note.as_deref());
    }
    Ok(())
}

/// Emit the standard `MetaEnvelope`-wrapped JSON payload for `--format json`.
/// `vex gpu` needs no index, so the default meta (no `index_age_ms`) is
/// correct — same shape `vex status` uses for its no-index branch.
fn print_gpu_envelope(
    compiled: &[Device],
    probes: &[ProbeOutcome],
    engaged: Option<Device>,
    pinned: bool,
    note: Option<&str>,
) {
    let payload = serde_json::json!({
        "build": gpu_support_str(),
        "default_device": DEFAULT_DEVICE.as_str(),
        "compiled": compiled.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
        "probes": probes
            .iter()
            .map(|p| {
                serde_json::json!({
                    "device": p.device.as_str(),
                    "engaged": p.error.is_none(),
                    // Warm-up is wall-clock ms; far below u64::MAX in practice.
                    "warmup_ms": p.warmup_ms.map(|ms| ms as u64),
                    "error": p.error,
                })
            })
            .collect::<Vec<_>>(),
        "engaged": engaged.map(|d| d.as_str()),
        "pinned": pinned,
        "note": note,
    });
    print_envelope(&payload, capabilities::current(), MetaEnvelope::default());
}

/// Shared install guidance for builds/devices without a usable GPU EP.
fn print_install_help() {
    println!("To get GPU acceleration:");
    println!(
        "  • NVIDIA (CUDA):    cargo install vex --features gpu-cuda  \
         (needs CUDA Toolkit 12 + cuDNN 9 on PATH)"
    );
    println!("  • Any Windows GPU:  use the prebuilt Windows binary (DirectML, driver-only)");
    println!("  • Apple Silicon:    cargo install vex --features gpu-coreml");
}

/// Build a MiniLM embedder on `device` with strict EP registration and run one
/// inference. Returns the model-load warm-up in ms. Any EP-registration
/// failure surfaces as `Err` because strict mode is on. MiniLM is used because
/// it is the smallest model — the probe tests the *provider*, not throughput;
/// it downloads ~86 MB on first use if the model isn't cached.
fn probe(device: Device) -> Result<u128> {
    let start = Instant::now();
    let mut embedder = make_embedder_with_device(MINILM_ID, device, /* strict */ true)?;
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
/// Returns whether the value was actually persisted (`setx` succeeded) —
/// on unix nothing is written automatically, so this is always `false` there.
/// `quiet` suppresses the human-readable messages (`--format json`).
fn persist_device(device: Device, quiet: bool) -> bool {
    let value = device.as_str();
    #[cfg(windows)]
    {
        // setx writes HKCU\Environment; it affects NEW shells, not this one.
        // `.output()` (not `.status()`) so the child cannot write to OUR
        // stdout — under `--format json` an inherited "SUCCESS: ..." line
        // from setx would corrupt the single-JSON-envelope contract that
        // the MCP wrapper parses.
        match std::process::Command::new("setx")
            .args(["VEX_DEVICE", value])
            .output()
        {
            Ok(out) if out.status.success() => {
                if !quiet {
                    println!(
                        "  Pinned VEX_DEVICE={value} (user environment). \
                         Open a NEW terminal for it to take effect."
                    );
                }
                true
            }
            _ => {
                if !quiet {
                    println!("  Could not run setx — set it manually: setx VEX_DEVICE {value}");
                }
                false
            }
        }
    }
    #[cfg(not(windows))]
    {
        if !quiet {
            println!("  Add to your shell profile to set it globally:");
            println!("      export VEX_DEVICE={value}");
        }
        false
    }
}
