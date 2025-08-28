use clap::{Parser};
use rand::{rngs::SmallRng, RngCore, SeedableRng};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{PathBuf};
use std::time::Instant;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "windows")]
use std::os::windows::fs::OpenOptionsExt as WinOpenOptionsExt;
use sysinfo::Disks;
use chrono::Local;

#[cfg(target_os = "macos")]
static MAC_MEDIA_DIRS: Lazy<Vec<&'static str>> = Lazy::new(|| vec!["/Volumes"]);

fn clear_screen() {
    #[cfg(target_os = "windows")]
    { let _ = std::process::Command::new("cmd").args(["/C", "cls"]).status(); }
    #[cfg(not(target_os = "windows"))]
    { let _ = std::process::Command::new("clear").status(); }
}

fn mbps(bytes: u128, dur_s: f64) -> f64 { (bytes as f64 * 8.0) / 1_000_000f64 / dur_s }
fn mbs(bytes: u128, dur_s: f64) -> f64 { (bytes as f64) / 1_000_000f64 / dur_s } // MB/s (decimal)

fn print_progress(prefix: &str, done: u64, total: u64, start: Instant) {
    let pct = (done as f64 / total as f64) * 100.0;
    let elapsed = start.elapsed().as_secs_f64();
    let speed_mbs = if elapsed > 0.0 {
        (done as f64 / 1_000_000f64) / elapsed
    } else { 0.0 };
    print!("\r{prefix}... {pct:5.1}% ({:.2} MB/s)", speed_mbs);
    let _ = std::io::stdout().flush();
}

fn finish_progress() { println!(); }

fn prompt_yes_no(prompt: &str) -> io::Result<bool> {
    print!("{} [y/N]: ", prompt);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let s = line.trim().to_ascii_lowercase();
    Ok(matches!(s.as_str(), "y" | "yes"))
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{}: ", prompt);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

#[derive(Parser, Debug)]
#[command(name="usbbench", about="USB read/write speed test")]
struct Args {
    /// Directory on the USB device to use (will write a temp file here). If omitted, you'll be prompted to pick a device.
    target_dir: Option<PathBuf>,

    /// Total test size (e.g., 1G, 512M)
    #[arg(short='s', long, default_value="1G")]
    size: String,

    /// Block size (e.g., 4M, 1M, 64K)
    #[arg(short='b', long, default_value="4M")]
    block: String,

    /// Keep the test file (for repeat reads)
    #[arg(long)]
    keep: bool,
}

fn parse_size(s: &str) -> u64 {
    // simple parser: supports K/M/G suffix (base 1024)
    let (num, suf) = s.trim().split_at(s.trim().find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().expect("invalid size");
    let mult = match suf.trim().to_ascii_uppercase().as_str() {
        "" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024*1024,
        "G" | "GB" => 1024*1024*1024,
        _ => panic!("unsupported size suffix"),
    };
    n * mult
}

#[cfg(target_os = "macos")]
fn set_nocache(file: &File) {
    unsafe {
        let fd = file.as_raw_fd();
        let _ = libc::fcntl(fd, libc::F_NOCACHE, 1);
    }
}

#[allow(dead_code)]
#[cfg(not(target_os = "macos"))]
fn set_nocache(_file: &File) {}

fn open_write(path: &std::path::Path, direct: bool) -> std::io::Result<File> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_WRITE_THROUGH};
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        if direct { opts.custom_flags(FILE_FLAG_WRITE_THROUGH as u32); } // Fix type
        let f = opts.open(path)?;
        Ok(f)
    }
    #[cfg(unix)]
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        if direct {
            opts.custom_flags(libc::O_SYNC);
        }
        let f = opts.open(path)?;
        #[cfg(target_os = "macos")]
        if direct { set_nocache(&f); }
        Ok(f)
    }
}

fn open_read(path: &std::path::Path, direct: bool) -> std::io::Result<File> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_WRITE_THROUGH};
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        if direct { opts.custom_flags(FILE_FLAG_WRITE_THROUGH as u32); } // Fix type
        let f = opts.open(path)?;
        Ok(f)
    }
    #[cfg(unix)]
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        if direct { opts.custom_flags(libc::O_SYNC); }
        let f = opts.open(path)?;
        #[cfg(target_os = "macos")]
        if direct { set_nocache(&f); }
        Ok(f)
    }
}

fn choose_target_dir() -> io::Result<PathBuf> {
    let disks = Disks::new_with_refreshed_list();

    // Gather candidates
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for d in disks.list() {
        let mount = d.mount_point().to_path_buf();
        let name = d.name().to_string_lossy().to_string();
        #[cfg(target_os = "windows")]
        {
            // Omit C: drive, include all others
            let letter = mount.display().to_string().chars().next().unwrap_or('C');
            if letter != 'C' {
                candidates.push((format!("{} — {}", name, mount.display()), mount.clone()));
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            if d.is_removable() {
                candidates.push((format!("{} — {}", name, mount.display()), mount.clone()));
            }
        }
    }

    // De-dup (some OSes report multiple entries for same mount point)
    candidates.sort_by(|a,b| a.1.cmp(&b.1));
    candidates.dedup_by(|a,b| a.1 == b.1);

    if candidates.is_empty() {
        eprintln!("No removable/USB mounts detected. Enter a directory path to test:");
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let p = PathBuf::from(line.trim());
        return Ok(p);
    }

    println!("Select a device/path to test:");
    for (i, (label, _)) in candidates.iter().enumerate() {
        println!("  {}. {}", i + 1, label);
    }
    print!("Enter number: ");
    io::stdout().flush()?;

    let mut sel = String::new();
    io::stdin().read_line(&mut sel)?;
    let idx: usize = sel.trim().parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid selection"))?;
    let idx0 = idx.checked_sub(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "selection out of range"))?;
    if let Some((label, path)) = candidates.get(idx0) {
        println!("Testing read/write speed to USB device: {}", label);
        Ok(path.clone())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "selection out of range"))
    }
}

fn main() -> std::io::Result<()> {
    clear_screen();
    println!(r#" ██████╗██████╗  █████╗ ██████╗ ██╗    ██╗██╗███████╗███████╗
██╔════╝██╔══██╗██╔══██╗██╔══██╗██║    ██║██║██╔════╝██╔════╝
██║     ██████╔╝███████║██████╔╝██║ █╗ ██║██║███████╗█████╗  
██║     ██╔══██╗██╔══██║██╔══██╗██║███╗██║██║╚════██║██╔══╝  
╚██████╗██║  ██║██║  ██║██████╔╝╚███╔███╔╝██║███████║███████╗
 ╚═════╝╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝  ╚══╝╚══╝ ╚═╝╚══════╝╚══════╝
"#);
    println!("USB Device Benchmark Utility\n");
    let args = Args::parse();
    let total = parse_size(&args.size);
    let block = parse_size(&args.block);
    assert!(block > 0 && total >= block, "block must be >0 and <= total size");

    let target_dir = match args.target_dir {
        Some(p) => p,
        None => choose_target_dir()?,
    };

    std::fs::create_dir_all(&target_dir)?;
    let test_path = target_dir.join(".usbbench.tmp");

    // -------- WRITE --------
    let f = open_write(&test_path, true)?;
    let mut writer = BufWriter::with_capacity(block as usize, f);

    // precreate a block of pseudo-random bytes
    let mut rng = SmallRng::seed_from_u64(0x5EED_CAFE);
    let mut buf = vec![0u8; block as usize];
    rng.fill_bytes(&mut buf);

    let mut written: u64 = 0;
    let t0 = Instant::now();
    while written < total {
        let to_write = std::cmp::min(block, total - written) as usize;
        writer.write_all(&buf[..to_write])?;
        written += to_write as u64;
        if total >= 100 { print_progress("Writing", written, total, t0); }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?; // ensure data + metadata on disk
    if total >= 100 { finish_progress(); }
    let write_secs = t0.elapsed().as_secs_f64();

    // -------- READ --------
    let f = open_read(&test_path, true)?;
    let mut reader = BufReader::with_capacity(block as usize, f);
    let mut read_buf = vec![0u8; block as usize];
    let mut read_total: u64 = 0;
    let t1 = Instant::now();
    loop {
        let n = reader.read(&mut read_buf)?;
        if n == 0 { break; }
        read_total += n as u64;
        if total >= 100 { print_progress("Reading", read_total, total, t1); }
    }
    if total >= 100 { finish_progress(); }
    let read_secs = t1.elapsed().as_secs_f64();

    let size_gib = (total as f64)/(1024.0*1024.0*1024.0);
    let block_mib = (block as f64)/(1024.0*1024.0);
    let w_mbs = mbs(written as u128, write_secs);
    let w_mbps = mbps(written as u128, write_secs);
    let r_mbs = mbs(read_total as u128, read_secs);
    let r_mbps = mbps(read_total as u128, read_secs);

    let top = "╔".to_string() + &"═".repeat(46) + "╗";
    let mid = "╚".to_string() + &"═".repeat(46) + "╝";
    println!("\n{}", top);
    println!("║{:^46}║", "USB Benchmark Results");
    println!("{}", mid);

    println!("{:<8} {} — {}", "Device:", target_dir.display(), test_path.parent().unwrap_or(&target_dir).display());
    println!("{:<8} {}", "Test:", test_path.display());
    println!("{:<8} {:>6.2} GiB", "Size:", size_gib);
    println!("{:<8} {:>6.2} MiB", "Block:", block_mib);

    println!("\n{:<6} {:>9.2} MB/s ({:>8.2} Mbps) in {:>6.2}s", "WRITE:", w_mbs, w_mbps, write_secs);
    println!("{:<6} {:>9.2} MB/s ({:>8.2} Mbps) in {:>6.2}s\n", "READ:",  r_mbs, r_mbps, read_secs);

    println!("{}", "═".repeat(48));

    // --- Optional logging ---
    if prompt_yes_no("Save results to USB root?")? {
        let mut session = prompt_line("Enter session name")?;
        if session.is_empty() {
            session = Local::now().format("session-%Y%m%d-%H%M%S").to_string();
        }
        let log_path = target_dir.join("crabwise.log");
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!(
            "{:<30} | {:>7.2} Mbps | {:>7.2} Mbps | {}\n",
            session, r_mbps, w_mbps, ts
        );
        let mut f = OpenOptions::new().create(true).append(true).open(&log_path)?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
        println!("Saved log entry to {}", log_path.display());
        if let Ok(contents) = std::fs::read_to_string(&log_path) {
            println!("\n=== crabwise.log ===\n{}", contents);
        }
    }

    if !args.keep {
        let _ = std::fs::remove_file(&test_path);
    }
    Ok(())
}