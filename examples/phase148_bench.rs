//! Phase 14.8 Step 2 — bench gate.
//!
//! One-shot bench harness measuring the three independent phases that the
//! future `vex index --history` builder will execute, so the design's
//! budgets can be validated before any builder code is written.
//!
//! Phases:
//!   [A] `git log --raw --no-renames` enumeration → (commit, path, blob) triples
//!   [B] parse via 14.7 blob cache (cache hit / miss latency split)
//!   [C] section size projection (HistoryEntry + Commit + Blob + FST overhead)
//!
//! Usage:
//!   cargo run --release --example phase148_bench -- \
//!       --repo /path/to/repo \
//!       --cache-dir /tmp/phase148-bench-cache \
//!       [--depth N] [--wipe-cache] [--json]
//!
//! `--cache-dir` is required and exclusive to this run — the harness will
//! `--wipe-cache` only that directory, never the user's main 14.7 cache.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;

use vex::index::parse_cache::BlobCache;
use vex::parse::language::Language;
use vex::parse::parse_file;
use vex::util::config;

#[derive(Parser, Debug)]
#[command(about = "Phase 14.8 Step 2 — bench gate for history-index builder")]
struct Args {
    /// Path to the git repository to bench.
    #[arg(long)]
    repo: PathBuf,
    /// Required isolated cache dir. Harness sets it via set_cache_override.
    #[arg(long)]
    cache_dir: PathBuf,
    /// Cap commits walked (mirrors --history-depth N).
    #[arg(long)]
    depth: Option<usize>,
    /// Wipe `--cache-dir` before sub-bench B (cold-cache measurement).
    #[arg(long)]
    wipe_cache: bool,
    /// Emit a single JSON object instead of human-readable output.
    #[arg(long)]
    json: bool,
}

#[derive(Debug)]
struct Triple {
    commit: String,
    path: String,
    blob: String,
}

#[derive(Debug, Default)]
struct Metrics {
    enum_elapsed_ms: u128,
    enum_triples: usize,
    enum_unique_commits: usize,
    enum_unique_blobs_path_sha: usize,
    enum_unique_blob_shas: usize,

    parse_elapsed_ms: u128,
    parse_cache_hits: usize,
    parse_cache_misses: usize,
    parse_hit_total_us: u128,
    parse_miss_total_us: u128,
    parse_skipped_unknown_lang: usize,
    parse_errors: usize,
    parse_total_symbols: usize,
    parse_total_input_bytes: u64,

    projected_section_bytes: u64,
    existing_index_bytes: u64,
    projected_ratio_pct: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();

    std::fs::create_dir_all(&args.cache_dir)
        .with_context(|| format!("create cache dir {}", args.cache_dir.display()))?;
    let abs_cache = std::fs::canonicalize(&args.cache_dir)?;
    // skip_hash_subdir=true so BlobCache writes directly into <cache_dir>/blobs/
    config::set_cache_override(abs_cache.clone(), true);

    if args.wipe_cache {
        let dir = config::blob_cache_dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
    }

    let mut m = Metrics::default();

    // --- [A] git log --raw enumeration ---
    let t = Instant::now();
    let triples = enumerate_blobs(&args.repo, args.depth)?;
    m.enum_elapsed_ms = t.elapsed().as_millis();
    m.enum_triples = triples.len();

    let unique_commits: HashSet<&str> = triples.iter().map(|t| t.commit.as_str()).collect();
    let unique_pairs: HashSet<(&str, &str)> = triples
        .iter()
        .map(|t| (t.path.as_str(), t.blob.as_str()))
        .collect();
    let unique_shas: HashSet<&str> = triples.iter().map(|t| t.blob.as_str()).collect();
    m.enum_unique_commits = unique_commits.len();
    m.enum_unique_blobs_path_sha = unique_pairs.len();
    m.enum_unique_blob_shas = unique_shas.len();

    // --- [B] parse via 14.7 cache ---
    let cache = BlobCache::new(config::blob_cache_dir());
    let mut batch = CatFileBatch::spawn(&args.repo)?;

    let t_b = Instant::now();
    let total_pairs = unique_pairs.len();
    let mut processed = 0usize;
    let mut last_log = Instant::now();
    for (path, blob) in &unique_pairs {
        processed += 1;
        if processed.is_multiple_of(250) || last_log.elapsed().as_secs() >= 5 {
            eprintln!(
                "  [B] {}/{}  hits={}  misses={}  elapsed={}ms",
                processed,
                total_pairs,
                m.parse_cache_hits,
                m.parse_cache_misses,
                t_b.elapsed().as_millis(),
            );
            last_log = Instant::now();
        }
        let lang_opt = Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .and_then(Language::from_extension);
        let Some(lang) = lang_opt else {
            m.parse_skipped_unknown_lang += 1;
            continue;
        };

        let t1 = Instant::now();
        if let Some(pf) = cache.lookup(blob, lang) {
            m.parse_cache_hits += 1;
            m.parse_hit_total_us += t1.elapsed().as_micros();
            m.parse_total_symbols += pf.symbols.len();
            continue;
        }

        // Cache miss: fetch blob + parse + insert.
        let bytes = match batch.read(blob) {
            Ok(b) => b,
            Err(_) => {
                m.parse_errors += 1;
                m.parse_cache_misses += 1;
                m.parse_miss_total_us += t1.elapsed().as_micros();
                continue;
            }
        };
        m.parse_total_input_bytes += bytes.len() as u64;
        let content_str = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                m.parse_errors += 1;
                m.parse_cache_misses += 1;
                m.parse_miss_total_us += t1.elapsed().as_micros();
                continue;
            }
        };
        match parse_file(path, content_str, lang) {
            Ok(pf) => {
                m.parse_total_symbols += pf.symbols.len();
                let _ = cache.insert(blob, lang, &pf);
            }
            Err(_) => {
                m.parse_errors += 1;
            }
        }
        m.parse_cache_misses += 1;
        m.parse_miss_total_us += t1.elapsed().as_micros();
    }
    m.parse_elapsed_ms = t_b.elapsed().as_millis();
    drop(batch); // close stdin → child exits

    // --- [C] section size projection ---
    let history_entries_bytes = (m.parse_total_symbols as u64) * 28;
    let commits_bytes = (m.enum_unique_commits as u64) * 32;
    let blobs_bytes = (m.enum_unique_blob_shas as u64) * 24;
    // FST overhead estimate: per existing symbol_fst infra, ~4 bytes per
    // unique key + 4 bytes per posting + ~10% FST header/transition overhead.
    // Postings count == HistoryEntry rows (one posting per entry).
    let unique_keys = estimate_unique_symbol_keys(m.parse_total_symbols);
    let fst_bytes = unique_keys * 4 + (m.parse_total_symbols as u64) * 4;
    let fst_bytes = (fst_bytes as f64 * 1.10) as u64;
    m.projected_section_bytes = history_entries_bytes + commits_bytes + blobs_bytes + fst_bytes;

    // Compare to existing index.vex if present at conventional location.
    let candidate = args.repo.join(".vex/index.vex");
    if let Ok(meta) = std::fs::metadata(&candidate) {
        m.existing_index_bytes = meta.len();
    }
    m.projected_ratio_pct = if m.existing_index_bytes > 0 {
        (m.projected_section_bytes as f64) / (m.existing_index_bytes as f64) * 100.0
    } else {
        0.0
    };

    if args.json {
        emit_json(&args, &m);
    } else {
        emit_human(&args, &m);
    }

    Ok(())
}

/// Best-guess unique-symbol-name count for FST sizing.
///
/// Real number requires building a `HashSet<String>` of all symbol names —
/// too memory-heavy for the harness. Empirically, on Rust repos
/// `unique_names / total_entries ≈ 0.3–0.5`. Use 0.4 as a central estimate.
fn estimate_unique_symbol_keys(total_entries: usize) -> u64 {
    ((total_entries as f64) * 0.4) as u64
}

fn emit_human(args: &Args, m: &Metrics) {
    println!("=== Phase 14.8 Step 2 bench gate ===");
    println!("Repo:        {}", args.repo.display());
    println!("Cache dir:   {}", args.cache_dir.display());
    println!("Wipe cache:  {}", args.wipe_cache);
    if let Some(d) = args.depth {
        println!("Depth cap:   {}", d);
    }

    println!("\n[A] git log --raw enumeration");
    println!("    elapsed:        {} ms", m.enum_elapsed_ms);
    println!("    triples:        {}", m.enum_triples);
    println!("    unique commits: {}", m.enum_unique_commits);
    println!("    unique (path,sha): {}", m.enum_unique_blobs_path_sha);
    println!("    unique blob shas:  {}", m.enum_unique_blob_shas);

    println!("\n[B] parse via 14.7 cache");
    println!("    elapsed total:     {} ms", m.parse_elapsed_ms);
    println!(
        "    cache hits:        {} (avg {:.1} µs/hit)",
        m.parse_cache_hits,
        avg_us(m.parse_hit_total_us, m.parse_cache_hits),
    );
    println!(
        "    cache misses:      {} (avg {:.1} µs/miss)",
        m.parse_cache_misses,
        avg_us(m.parse_miss_total_us, m.parse_cache_misses),
    );
    println!("    skipped (lang):    {}", m.parse_skipped_unknown_lang);
    println!("    parse errors:      {}", m.parse_errors);
    println!("    total symbols:     {}", m.parse_total_symbols);
    println!(
        "    total input bytes: {} ({:.1} MiB)",
        m.parse_total_input_bytes,
        (m.parse_total_input_bytes as f64) / (1024.0 * 1024.0),
    );

    println!("\n[C] section size projection");
    println!(
        "    HistoryEntry rows: {} × 28B  = {} KiB",
        m.parse_total_symbols,
        (m.parse_total_symbols * 28) / 1024,
    );
    println!(
        "    Commit rows:       {} × 32B  = {} KiB",
        m.enum_unique_commits,
        (m.enum_unique_commits * 32) / 1024,
    );
    println!(
        "    Blob rows:         {} × 24B  = {} KiB",
        m.enum_unique_blob_shas,
        (m.enum_unique_blob_shas * 24) / 1024,
    );
    println!(
        "    FST + postings:    ~{} KiB",
        (m.projected_section_bytes
            - (m.parse_total_symbols * 28) as u64
            - (m.enum_unique_commits * 32) as u64
            - (m.enum_unique_blob_shas * 24) as u64)
            / 1024,
    );
    println!(
        "    section total:     {} KiB ({:.2} MiB)",
        m.projected_section_bytes / 1024,
        (m.projected_section_bytes as f64) / (1024.0 * 1024.0),
    );
    if m.existing_index_bytes > 0 {
        println!(
            "    vs existing idx:   {} KiB → projected {:.1}% of index.vex",
            m.existing_index_bytes / 1024,
            m.projected_ratio_pct,
        );
    } else {
        println!("    vs existing idx:   (no .vex/index.vex found)");
    }

    println!("\n=== Summary ===");
    println!("    [A] git enum:   {:>7} ms", m.enum_elapsed_ms);
    println!("    [B] parse path: {:>7} ms", m.parse_elapsed_ms);
    println!(
        "    total wall:     {:>7} ms",
        m.enum_elapsed_ms + m.parse_elapsed_ms
    );
}

fn emit_json(args: &Args, m: &Metrics) {
    println!(
        "{{\"repo\":\"{}\",\"cache_dir\":\"{}\",\"wipe_cache\":{},\"depth\":{},\
        \"enum_ms\":{},\"enum_triples\":{},\"unique_commits\":{},\
        \"unique_path_sha\":{},\"unique_shas\":{},\
        \"parse_ms\":{},\"hits\":{},\"misses\":{},\
        \"hit_avg_us\":{:.1},\"miss_avg_us\":{:.1},\
        \"skipped_lang\":{},\"parse_errors\":{},\"total_symbols\":{},\
        \"input_bytes\":{},\"section_bytes\":{},\
        \"existing_index_bytes\":{},\"ratio_pct\":{:.2}}}",
        args.repo.display(),
        args.cache_dir.display(),
        args.wipe_cache,
        args.depth.map_or("null".into(), |d| d.to_string()),
        m.enum_elapsed_ms,
        m.enum_triples,
        m.enum_unique_commits,
        m.enum_unique_blobs_path_sha,
        m.enum_unique_blob_shas,
        m.parse_elapsed_ms,
        m.parse_cache_hits,
        m.parse_cache_misses,
        avg_us(m.parse_hit_total_us, m.parse_cache_hits),
        avg_us(m.parse_miss_total_us, m.parse_cache_misses),
        m.parse_skipped_unknown_lang,
        m.parse_errors,
        m.parse_total_symbols,
        m.parse_total_input_bytes,
        m.projected_section_bytes,
        m.existing_index_bytes,
        m.projected_ratio_pct,
    );
}

fn avg_us(total_us: u128, n: usize) -> f64 {
    if n == 0 {
        0.0
    } else {
        (total_us as f64) / (n as f64)
    }
}

// ---------------------------------------------------------------------------
// git enumeration
// ---------------------------------------------------------------------------

fn enumerate_blobs(repo: &Path, depth: Option<usize>) -> Result<Vec<Triple>> {
    let mut argv: Vec<String> = vec![
        "log".into(),
        "--raw".into(),
        "--no-renames".into(),
        "--no-merges".into(),
        "--pretty=format:COMMIT %H".into(),
    ];
    if let Some(n) = depth {
        argv.push(format!("-n{}", n));
    }
    argv.push("HEAD".into());

    let out = Command::new("git")
        .current_dir(repo)
        .args(&argv)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn git log in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let mut triples = Vec::with_capacity(4096);
    let mut current = String::new();
    for line in out.stdout.split(|&b| b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("");
        if let Some(rest) = line.strip_prefix("COMMIT ") {
            current = rest.trim().to_string();
            continue;
        }
        if !line.starts_with(':') {
            continue;
        }
        // ":old_mode new_mode old_sha new_sha status\tpath"
        let mut tab = line.splitn(2, '\t');
        let meta = tab.next().unwrap_or("");
        let path = tab.next().unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let toks: Vec<&str> = meta.split_whitespace().collect();
        if toks.len() < 5 {
            continue;
        }
        let status = toks[4];
        if status == "D" {
            continue; // deletions: new_sha is zeros, nothing to parse
        }
        let new_sha = toks[3];
        if new_sha.bytes().all(|b| b == b'0') {
            continue;
        }
        triples.push(Triple {
            commit: current.clone(),
            path: path.to_string(),
            blob: new_sha.to_string(),
        });
    }
    Ok(triples)
}

// ---------------------------------------------------------------------------
// Long-lived `git cat-file --batch` for efficient blob streaming.
// ---------------------------------------------------------------------------

struct CatFileBatch {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl CatFileBatch {
    fn spawn(repo: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .current_dir(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| "spawn git cat-file --batch")?;
        let stdin = child.stdin.take().context("take stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("take stdout")?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn read(&mut self, sha: &str) -> Result<Vec<u8>> {
        writeln!(self.stdin, "{}", sha)?;
        self.stdin.flush()?;

        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let header = header.trim_end();
        // header: "<sha> <type> <size>"  OR  "<input> missing"
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() == 2 && parts[1] == "missing" {
            bail!("blob missing: {}", sha);
        }
        if parts.len() != 3 {
            bail!("unexpected cat-file header: {}", header);
        }
        let size: usize = parts[2].parse().context("parse blob size")?;
        // Read THROUGH BufReader, not around it — otherwise bytes already
        // buffered after the header line get skipped and the next read
        // deadlocks waiting for data that's already in our buffer.
        let mut buf = vec![0u8; size];
        self.stdout.read_exact(&mut buf)?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl)?; // trailing newline
        Ok(buf)
    }
}

impl Drop for CatFileBatch {
    fn drop(&mut self) {
        // We hold `self.stdin` here; it doesn't drop until after this
        // function returns. Calling `child.wait()` first would block
        // forever waiting for cat-file to EOF on its still-open stdin.
        // Kill the child outright — bench doesn't care about clean exit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(dead_code)]
fn _wall_ms(d: Duration) -> u128 {
    d.as_millis()
}
