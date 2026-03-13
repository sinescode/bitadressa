use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const URL: &str = "https://gz.blockchair.com/bitcoin/addresses/blockchair_bitcoin_addresses_latest.tsv.gz";
const OUTPUT_FILE: &str = "blockchair_bitcoin_addresses_latest.tsv.gz";
const CHUNK_SIZE: usize = 65536;
const CONNECT_TIMEOUT_SECS: u64 = 30;
const READ_TIMEOUT_SECS: u64 = 60;   // per-read timeout, NOT global
const PROGRESS_UPDATE_INTERVAL_SECS: f64 = 0.5;
const MAX_RETRIES: u32 = 99;         // keep retrying indefinitely
const RETRY_DELAY_MS: u64 = 3000;

#[derive(Debug)]
enum DownloadError {
    Io(io::Error),
    Http(u16, String),
    Transport(String),
    MaxRetriesExceeded,
    Cancelled,
}

impl From<io::Error> for DownloadError {
    fn from(e: io::Error) -> Self {
        DownloadError::Io(e)
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Io(e) => write!(f, "IO error: {}", e),
            DownloadError::Http(code, msg) => write!(f, "HTTP {}: {}", code, msg),
            DownloadError::Transport(e) => write!(f, "Transport: {}", e),
            DownloadError::MaxRetriesExceeded => write!(f, "Max retries exceeded"),
            DownloadError::Cancelled => write!(f, "Download cancelled by user"),
        }
    }
}

impl std::error::Error for DownloadError {}

struct Formatters;

impl Formatters {
    fn bytes(mut bytes: f64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
        for unit in UNITS {
            if bytes.abs() < 1024.0 {
                return format!("{:>6.1} {:2}", bytes, unit);
            }
            bytes /= 1024.0;
        }
        format!("{:>6.1} EB", bytes)
    }

    fn duration(seconds: f64) -> String {
        if seconds < 60.0 {
            format!("{:>4.0}s", seconds)
        } else if seconds < 3600.0 {
            format!("{:>4.1}m", seconds / 60.0)
        } else {
            let h = (seconds / 3600.0) as u64;
            let m = ((seconds % 3600.0) / 60.0) as u64;
            format!("{}h {:02}m", h, m)
        }
    }

    fn speed(bps: f64) -> String {
        format!("{}/s", Self::bytes(bps))
    }
}

struct ProgressBar {
    width: usize,
    last_line_len: usize,
}

impl ProgressBar {
    fn new(width: usize) -> Self {
        Self {
            width,
            last_line_len: 0,
        }
    }

    fn render(&mut self, current: u64, total: Option<u64>, speed: f64, _elapsed: f64) {
        let percentage = total.map(|t| current as f64 / t as f64).unwrap_or(0.0);
        let filled = ((self.width as f64 * percentage) as usize).min(self.width);

        let bar = format!(
            "{}{}",
            "█".repeat(filled),
            "░".repeat(self.width - filled)
        );

        let eta = total.and_then(|t| {
            if speed > 0.0 && current < t {
                Some((t - current) as f64 / speed)
            } else {
                None
            }
        });

        let line = if total.is_some() {
            format!(
                "│{}│ {:>5.1}% │ {} │ {} │ ETA: {} ",
                bar,
                percentage * 100.0,
                Formatters::bytes(current as f64),
                Formatters::speed(speed),
                eta.map(Formatters::duration).unwrap_or_else(|| "?".to_string())
            )
        } else {
            format!(
                "│{}│ {} │ {} │ ? ",
                bar,
                Formatters::bytes(current as f64),
                Formatters::speed(speed)
            )
        };

        let padding = if line.len() < self.last_line_len {
            " ".repeat(self.last_line_len - line.len())
        } else {
            String::new()
        };

        print!("\r{}{}", line, padding);
        let _ = io::stdout().flush();
        self.last_line_len = line.len();
    }

    fn finish(&self) {
        println!();
    }

    fn clear(&self) {
        print!("\r{}\r", " ".repeat(self.last_line_len));
        let _ = io::stdout().flush();
    }
}

struct DownloadStats {
    start_time: Instant,
    bytes_downloaded: u64,
    last_update: Instant,
    bytes_since_update: u64,
}

impl DownloadStats {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            start_time: now,
            bytes_downloaded: 0,
            last_update: now,
            bytes_since_update: 0,
        }
    }

    fn update(&mut self, bytes: usize) {
        self.bytes_downloaded += bytes as u64;
        self.bytes_since_update += bytes as u64;
    }

    fn should_update(&self) -> bool {
        self.last_update.elapsed().as_secs_f64() >= PROGRESS_UPDATE_INTERVAL_SECS
    }

    fn reset_interval(&mut self) {
        self.last_update = Instant::now();
        self.bytes_since_update = 0;
    }

    fn current_speed(&self) -> f64 {
        let elapsed = self.last_update.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.bytes_since_update as f64 / elapsed
        } else {
            0.0
        }
    }

    fn average_speed(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.bytes_downloaded as f64 / elapsed
        } else {
            0.0
        }
    }
}

fn setup_ctrlc_handler() -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");
    running
}

fn create_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // How long to wait for TCP connection to be established
        .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECS)))
        // How long to wait for each individual body chunk — NOT a global timeout
        // This allows multi-hour downloads while still detecting stalled connections
        .timeout_recv_body(Some(Duration::from_secs(READ_TIMEOUT_SECS)))
        // timeout_global is intentionally OMITTED — it kills the whole download
        .build()
        .into()
}

fn get_existing_file_size(path: &Path) -> u64 {
    path.metadata().map(|m| m.len()).unwrap_or(0)
}

fn open_output_file(path: &Path, append: bool) -> Result<std::fs::File, DownloadError> {
    let mut opts = OpenOptions::new();
    opts.write(true);

    if append {
        opts.append(true);
    } else {
        opts.create(true).truncate(true);
    }

    opts.open(path).map_err(DownloadError::from)
}

fn make_request(
    agent: &ureq::Agent,
    url: &str,
    resume_from: u64,
) -> Result<ureq::http::Response<ureq::Body>, DownloadError> {
    let mut req = agent
        .get(url)
        .header("User-Agent", "rust-downloader/1.0");

    if resume_from > 0 {
        req = req.header("Range", format!("bytes={}-", resume_from));
    }

    req.call().map_err(|e| match e {
        ureq::Error::StatusCode(code) => {
            let msg = match code {
                402 => "Payment Required: Range requests blocked on free tier".to_string(),
                416 => "Range Not Satisfiable: File may already be complete".to_string(),
                _ => format!("HTTP error {}", code),
            };
            DownloadError::Http(code, msg)
        }
        e => DownloadError::Transport(e.to_string()),
    })
}

fn download_chunk(
    reader: &mut impl Read,
    file: &mut std::fs::File,
    buf: &mut [u8],
    running: &Arc<AtomicBool>,
) -> Result<usize, DownloadError> {
    match reader.read(buf) {
        Ok(0) => Ok(0),
        Ok(n) => {
            if !running.load(Ordering::SeqCst) {
                return Err(DownloadError::Cancelled);
            }
            file.write_all(&buf[..n])?;
            Ok(n)
        }
        Err(e) if e.kind() == io::ErrorKind::TimedOut
            || e.kind() == io::ErrorKind::WouldBlock =>
        {
            Err(DownloadError::Transport(format!("Read timeout: {}", e)))
        }
        Err(e) => Err(DownloadError::Io(e)),
    }
}

fn download_with_resume(
    url: &str,
    output_file: &str,
    running: &Arc<AtomicBool>,
) -> Result<(), DownloadError> {
    let path = Path::new(output_file);
    let mut downloaded = get_existing_file_size(path);
    let mut retries = 0u32;

    println!("{}", "━".repeat(60));
    println!("  📥  Bitcoin Addresses Dataset Downloader");
    println!("{}", "━".repeat(60));
    println!("  URL:    {}", url);
    println!("  Output: {}", output_file);

    if downloaded > 0 {
        println!(
            "  📦 Resuming from: {}",
            Formatters::bytes(downloaded as f64)
        );
    }
    println!("{}", "─".repeat(60));

    loop {
        if !running.load(Ordering::SeqCst) {
            return Err(DownloadError::Cancelled);
        }

        let agent = create_agent();
        let response = match make_request(&agent, url, downloaded) {
            Ok(r) => r,
            Err(DownloadError::Http(416, _)) => {
                println!("  ⚠️  File appears to be complete already");
                return Ok(());
            }
            Err(e) => {
                retries += 1;
                if retries >= MAX_RETRIES {
                    return Err(DownloadError::MaxRetriesExceeded);
                }
                let delay = RETRY_DELAY_MS * retries as u64;
                eprintln!(
                    "  ⚠️  Attempt {}/{} failed: {} — retrying in {}ms...",
                    retries, MAX_RETRIES, e, delay
                );
                std::thread::sleep(Duration::from_millis(delay));
                continue;
            }
        };

        // Reset retry counter on successful connection
        retries = 0;

        let status = response.status();
        let total_size: Option<u64> = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(|cl| if status == 206 { cl + downloaded } else { cl });

        match total_size {
            Some(t) => println!("  📊 Total size: {}", Formatters::bytes(t as f64)),
            None => println!("  📊 Total size: unknown (streaming)"),
        }
        println!("{}", "─".repeat(60));

        let mut file = open_output_file(path, downloaded > 0 && status == 206)?;
        let mut reader = response.into_body().into_reader();
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut stats = DownloadStats::new();
        let mut progress = ProgressBar::new(20);
        let mut success = false;

        loop {
            match download_chunk(&mut reader, &mut file, &mut buf, running) {
                Ok(0) => {
                    success = true;
                    break;
                }
                Ok(n) => {
                    downloaded += n as u64;
                    stats.update(n);

                    if stats.should_update() {
                        progress.render(
                            downloaded,
                            total_size,
                            stats.current_speed(),
                            stats.start_time.elapsed().as_secs_f64(),
                        );
                        stats.reset_interval();
                    }
                }
                Err(DownloadError::Transport(_)) | Err(DownloadError::Io(_)) => {
                    let _ = file.flush();
                    progress.clear();
                    println!("  ⚠️  Connection stalled, reconnecting in {}ms...", RETRY_DELAY_MS);
                    std::thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
                    break; // break inner loop → reconnect in outer loop
                }
                Err(e) => {
                    let _ = file.flush();
                    return Err(e);
                }
            }
        }

        progress.finish();

        if success {
            let elapsed = stats.start_time.elapsed().as_secs_f64();
            let avg_speed = stats.average_speed();

            println!("{}", "─".repeat(60));
            println!("  ✅ Download complete!");
            println!("  📦 Size:    {}", Formatters::bytes(downloaded as f64));
            println!("  ⏱️  Time:    {}", Formatters::duration(elapsed));
            println!("  🚀 Average: {}", Formatters::speed(avg_speed));
            println!("{}", "━".repeat(60));
            return Ok(());
        }

        if !running.load(Ordering::SeqCst) {
            return Err(DownloadError::Cancelled);
        }
    }
}

fn main() {
    let running = setup_ctrlc_handler();

    match download_with_resume(URL, OUTPUT_FILE, &running) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("\n  ❌ Error: {}", e);
            match e {
                DownloadError::Http(402, _) => {
                    eprintln!("     Tip: Delete the partial file and retry without resume");
                }
                DownloadError::Cancelled => {
                    eprintln!("     Partial file saved. Run again to resume.");
                }
                _ => {
                    eprintln!("     Progress saved. Run again to resume.");
                }
            }
            std::process::exit(1);
        }
    }
}
