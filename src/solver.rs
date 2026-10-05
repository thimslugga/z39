use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use anyhow::Result;

const Z3_VERSION: &str = "4.16.0";

/// Find or download the z3 binary.
/// Search order:
/// 1. Z3_BIN env var
/// 2. Same directory as the z39 binary (distribution layout)
/// 3. ~/.local/share/z39/z3 (auto-downloaded)
/// 4. PATH lookup
/// If not found anywhere, auto-download from GitHub releases (macOS/Windows)
/// or build from source (Linux, since official binaries require glibc 2.38+).
pub async fn find_or_download_z3() -> anyhow::Result<PathBuf> {
    // 1. Z3_BIN env var
    if let Ok(p) = std::env::var("Z3_BIN") {
        let pb = PathBuf::from(&p);
        if pb.exists() { return Ok(pb); }
    }

    // 2. Same directory as the z39 binary (distribution layout)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["z3", "z3.exe"] {
                let candidate = dir.join(name);
                if candidate.exists() { return Ok(candidate); }
            }
        }
    }

    // 3. ~/.local/share/z39/z3 (auto-downloaded)
    let cache_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("z39");
    let cached = cache_dir.join("z3");
    if cfg!(target_os = "windows") {
        let cached_exe = cache_dir.join("z3.exe");
        if cached_exe.exists() { return Ok(cached_exe); }
    }
    if cached.exists() { return Ok(cached); }

    // 4. PATH lookup
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let p = PathBuf::from(dir).join("z3");
        if p.exists() { return Ok(p); }
    }

    // Not found — auto-download
    eprintln!("z39: z3 not found, auto-provisioning v{}...", Z3_VERSION);
    provision_z3(&cache_dir).await?;
    if cfg!(target_os = "windows") {
        Ok(cache_dir.join("z3.exe"))
    } else {
        Ok(cache_dir.join("z3"))
    }
}

/// Download Z3 binary or build from source depending on platform.
async fn provision_z3(cache_dir: &PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(cache_dir)?;

    #[cfg(target_os = "macos")]
    {
        let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
        let filename = format!("z3-{Z3_VERSION}-{arch}-osx-15.7.3.zip");
        let url = format!("https://github.com/Z3Prover/z3/releases/download/z3-{Z3_VERSION}/{filename}");
        download_and_extract(&url, cache_dir).await
    }

    #[cfg(target_os = "windows")]
    {
        let filename = format!("z3-{Z3_VERSION}-x64-win.zip");
        let url = format!("https://github.com/Z3Prover/z3/releases/download/z3-{Z3_VERSION}/{filename}");
        download_and_extract(&url, cache_dir).await
    }

    #[cfg(target_os = "linux")]
    {
        // Official Z3 binaries require glibc 2.38+, many Linux distros ship older.
        // Build from source for maximum compatibility.
        build_z3_from_source(cache_dir).await
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        anyhow::bail!("z39: unsupported platform for auto-provisioning; install z3 manually and set Z3_BIN")
    }
}

async fn download_and_extract(url: &str, cache_dir: &PathBuf) -> anyhow::Result<()> {
    eprintln!("z39: downloading {url}");
    let response = reqwest::get(url).await?;
    if !response.status().is_success() {
        anyhow::bail!("download failed: {}", response.status());
    }
    let bytes = response.bytes().await?;
    let tmpdir = tempfile::tempdir()?;

    let zip_path = tmpdir.path().join("z3.zip");
    std::fs::write(&zip_path, &bytes)?;

    let archive = std::io::BufReader::new(std::fs::File::open(&zip_path)?);
    let mut zip = zip::ZipArchive::new(archive)?;

    let dest_name = if cfg!(target_os = "windows") { "z3.exe" } else { "z3" };
    let dest = cache_dir.join(dest_name);

    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let name = file.name().to_string();
        // Match z3 binary inside the archive (any path ending in /bin/z3 or /z3)
        if name.ends_with("/bin/z3") || name.ends_with("/bin/z3.exe") || name == "z3" || name == "z3.exe" {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut buf)?;
            std::fs::write(&dest, &buf)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
            }
            eprintln!("z39: z3 installed to {}", dest.display());
            return Ok(());
        }
    }
    anyhow::bail!("z3 binary not found in archive")
}

#[cfg(target_os = "linux")]
async fn build_z3_from_source(cache_dir: &PathBuf) -> anyhow::Result<()> {
    let src_url = format!("https://github.com/Z3Prover/z3/archive/refs/tags/z3-{Z3_VERSION}.tar.gz");
    let tmpdir = tempfile::tempdir()?;

    eprintln!("z39: downloading Z3 source...");
    let response = reqwest::get(&src_url).await?;
    if !response.status().is_success() {
        anyhow::bail!("download failed: {}", response.status());
    }
    let bytes = response.bytes().await?;
    let archive_path = tmpdir.path().join("z3.tar.gz");
    std::fs::write(&archive_path, &bytes)?;

    // Extract
    let output = Command::new("tar")
        .args(["-xzf", archive_path.to_str().unwrap(), "-C", tmpdir.path().to_str().unwrap()])
        .output().await?;
    if !output.status.success() {
        anyhow::bail!("failed to extract Z3 source: {}", String::from_utf8_lossy(&output.stderr));
    }

    let src_dir = tmpdir.path().join(format!("z3-z3-{Z3_VERSION}"));
    let build_dir = src_dir.join("build");
    std::fs::create_dir_all(&build_dir)?;

    // Configure
    eprintln!("z39: configuring Z3...");
    let output = Command::new("python3")
        .args(["../scripts/mk_make.py"])
        .current_dir(&build_dir)
        .output().await?;
    if !output.status.success() {
        anyhow::bail!("Z3 configure failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Build
    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    eprintln!("z39: building Z3 with {nproc} jobs (this may take a few minutes)...");
    let output = Command::new("make")
        .args(["-j", &nproc.to_string()])
        .current_dir(&build_dir)
        .output().await?;
    if !output.status.success() {
        anyhow::bail!("Z3 build failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Copy to cache
    let built = build_dir.join("z3");
    let dest = cache_dir.join("z3");
    std::fs::copy(&built, &dest)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;

    eprintln!("z39: Z3 v{} built and installed to {}", Z3_VERSION, dest.display());
    Ok(())
}

// === Solver ===

#[derive(Debug, Clone, PartialEq)]
pub enum SolveStatus {
    Sat,
    Unsat,
    Unknown,
    Timeout,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct SolveResult {
    pub status: SolveStatus,
    pub model: Option<String>,
    pub raw_output: String,
    pub duration_ms: u64,
}

impl SolveResult {
    /// Token-optimized compact output
    pub fn to_compact(&self) -> String {
        let t = format!("{:.1}s", self.duration_ms as f64 / 1000.0);
        match &self.status {
            SolveStatus::Sat => {
                let m = self.model.as_deref().unwrap_or("");
                if m.is_empty() { format!("sat {t}") } else { format!("sat {m} {t}") }
            }
            SolveStatus::Unsat => format!("unsat {t}"),
            SolveStatus::Unknown => format!("unknown {t}"),
            SolveStatus::Timeout => format!("timeout {t}"),
            SolveStatus::Error(e) => format!("error {e} {t}"),
        }
    }

    pub fn is_sat(&self) -> bool { self.status == SolveStatus::Sat }
    pub fn is_unsat(&self) -> bool { self.status == SolveStatus::Unsat }
}

/// Run z3 on SMT-LIB2 input
pub async fn solve(z3_bin: &PathBuf, smt_input: &str, timeout_secs: u64) -> SolveResult {
    let start = std::time::Instant::now();
    let dur = if timeout_secs > 0 {
        Duration::from_secs(timeout_secs)
    } else {
        Duration::from_secs(300)
    };

    let result = timeout(dur, run_z3_process(z3_bin, smt_input)).await;
    let elapsed = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(output)) => parse_z3_output(&output, elapsed),
        Ok(Err(e)) => SolveResult {
            status: SolveStatus::Error(e.to_string()),
            model: None,
            raw_output: String::new(),
            duration_ms: elapsed,
        },
        Err(_) => SolveResult {
            status: SolveStatus::Timeout,
            model: None,
            raw_output: String::new(),
            duration_ms: elapsed,
        },
    }
}

async fn run_z3_process(z3_bin: &PathBuf, smt_input: &str) -> Result<String> {
    let mut child = Command::new(z3_bin)
        .arg("-in")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(smt_input.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    let output = child.wait_with_output().await?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_z3_output(raw: &str, elapsed_ms: u64) -> SolveResult {
    let trimmed = raw.trim();
    let first_line = trimmed.lines().next().unwrap_or("").trim();

    let (status, model) = if first_line.starts_with("sat") || first_line == "sat" {
        (SolveStatus::Sat, extract_model(trimmed))
    } else if first_line.starts_with("unsat") {
        (SolveStatus::Unsat, None)
    } else if first_line.starts_with("unknown") {
        (SolveStatus::Unknown, None)
    } else if first_line.starts_with("error") {
        (SolveStatus::Error(first_line.to_string()), None)
    } else {
        (SolveStatus::Error(format!("unexpected: {first_line}")), None)
    };

    SolveResult { status, model, raw_output: trimmed.to_string(), duration_ms: elapsed_ms }
}

fn extract_model(output: &str) -> Option<String> {
    let mut assignments = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    for i in 0..lines.len() {
        let line = lines[i].trim();
        if line.starts_with("(define-fun") {
            if let Some((name, inline)) = parse_define_fun(line) {
                if let Some(val) = inline {
                    assignments.push(format!("{name}={val}"));
                    continue;
                }
                // Value is on the next line; strip the single paren closing define-fun.
                if i + 1 < lines.len() {
                    let next = lines[i + 1].trim();
                    let val = next.strip_suffix(')').unwrap_or(next);
                    if !val.is_empty() && val != "(" {
                        assignments.push(format!("{name}={val}"));
                    }
                }
            }
        }
    }
    if assignments.is_empty() {
        None
    } else {
        Some(assignments.join(" "))
    }
}

/// Parse `(define-fun <name> () <sort> [<value>])` into (name, inline value).
///
/// Only constants (empty arg list) are handled; functions return None.
/// A parenthesized sort such as `(_ BitVec 8)` is skipped by paren matching,
/// so its parameters are never mistaken for the value.
fn parse_define_fun(line: &str) -> Option<(String, Option<String>)> {
    let mut rest = line.trim().strip_prefix("(define-fun")?.trim_start();
    let name_end = rest.find(char::is_whitespace)?;
    let name = rest[..name_end].to_string();
    rest = rest[name_end..].trim_start();
    // Only constants carry a value; skip functions like (define-fun f ((x Int)) ...).
    rest = rest.strip_prefix("()")?.trim_start();
    // Skip the sort: either a plain token (Int, Bool, Real) or a
    // parenthesized parameterized sort ((_ BitVec 8), (Array Int Int)).
    if let Some(after) = rest.strip_prefix('(') {
        let mut depth = 1;
        let mut sort_end = None;
        for (idx, ch) in after.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        sort_end = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = after[sort_end? + 1..].trim_start();
    } else {
        let sort_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = rest[sort_end..].trim_start();
    }
    // Whatever remains (minus the single paren closing define-fun) is the value.
    let val = rest.strip_suffix(')').unwrap_or(rest).trim_end();
    let value = if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    };
    Some((name, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_inline_value() {
        let out = "sat\n(\n  (define-fun x () Int 6)\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("x=6"));
    }

    #[test]
    fn int_next_line_value() {
        let out = "sat\n(\n  (define-fun x () Int\n    6)\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("x=6"));
    }

    #[test]
    fn bool_value() {
        let out = "sat\n(\n  (define-fun p () Bool\n    true)\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("p=true"));
    }

    #[test]
    fn bitvec_next_line_value_not_width() {
        // Regression test: the old parser reported x=8 (the bit width).
        let out = "sat\n(\n  (define-fun x () (_ BitVec 8)\n    #xff)\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("x=#xff"));
    }

    #[test]
    fn bitvec_inline_value() {
        let out = "sat\n(\n  (define-fun x () (_ BitVec 8) #xff)\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("x=#xff"));
    }

    #[test]
    fn negative_int_keeps_parens() {
        let out = "sat\n(\n  (define-fun x () Int\n    (- 3))\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("x=(- 3)"));
    }

    #[test]
    fn real_fraction_keeps_parens() {
        let out = "sat\n(\n  (define-fun r () Real\n    (/ 5.0 2.0))\n)\n";
        assert_eq!(extract_model(out).as_deref(), Some("r=(/ 5.0 2.0)"));
    }

    #[test]
    fn algebraic_number_keeps_parens() {
        let out = "sat\n(\n  (define-fun x () Real\n    (root-obj (+ (^ x 3) (- 2)) 1))\n)\n";
        assert_eq!(
            extract_model(out).as_deref(),
            Some("x=(root-obj (+ (^ x 3) (- 2)) 1)")
        );
    }

    #[test]
    fn function_define_fun_is_skipped() {
        let out = "sat\n(\n  (define-fun f ((x Int)) Int (+ x 1))\n)\n";
        assert_eq!(extract_model(out), None);
    }

    #[test]
    fn multi_sort_model() {
        // Real output from Z3 4.16.0.
        let out = "sat\n(\n  (define-fun r () Real\n    (/ 5.0 2.0))\n  (define-fun b () (_ BitVec 8)\n    #xff)\n  (define-fun x () Int\n    (- 3))\n  (define-fun p () Bool\n    true)\n)\n";
        assert_eq!(
            extract_model(out).as_deref(),
            Some("r=(/ 5.0 2.0) b=#xff x=(- 3) p=true")
        );
    }

    #[test]
    fn unsat_has_no_model() {
        let out = "unsat\n(error \"line 1 column 1: model is not available\")\n";
        assert_eq!(extract_model(out), None);
    }
}
