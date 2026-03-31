use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    time::{Duration, Instant},
};

use clap::Args;
use espflash::{
    Error,
    cli::{ConnectArgs, config::Config, connect, parse_u32, resolve_connect_args},
    command::{Command, CommandType},
    connection::ResetAfterOperation,
    flasher::Flasher,
    target::ProgressCallbacks,
};
use log::info;
use md5::{Digest, Md5};

use crate::Result;

const FLASH_SECTOR_SIZE: u32 = 0x1000;
const HEADER_LABEL_WIDTH: usize = 16;

#[derive(Debug, Args, Clone)]
pub struct BenchmarkArgs {
    /// Connection configuration
    #[clap(flatten)]
    pub connect_args: ConnectArgs,

    /// Start address of the scratch flash region to benchmark
    ///
    /// This command is destructive: the benchmark overwrites this region.
    /// The address must point at a disposable region and be sector aligned.
    #[arg(long, value_parser = parse_u32)]
    pub address: u32,

    /// Flash region sizes to benchmark
    #[arg(
        long = "size",
        value_name = "BYTES",
        value_delimiter = ',',
        default_values = ["0x100000"],
        value_parser = parse_u32
    )]
    pub sizes: Vec<u32>,

    /// Number of iterations per benchmark
    #[arg(long, default_value_t = 5)]
    pub iterations: usize,

    /// Size of each individual fast-read packet
    #[arg(long, default_value = "0x1000", value_parser = parse_u32)]
    pub block_size: u32,

    /// Maximum number of un-acked fast-read packets
    #[arg(long, default_value_t = 64)]
    pub max_in_flight: u32,
}

#[derive(Debug)]
struct SampleSummary {
    mean: f64,
    stddev: f64,
}

#[derive(Debug)]
struct TransferBenchmark {
    size: u32,
    write: SampleSummary,
    read: SampleSummary,
    skip: SampleSummary,
}

#[derive(Debug, Clone, Copy)]
struct PhaseOptions {
    name: &'static str,
    steps: usize,
    skip_enabled: bool,
}

#[derive(Debug)]
struct TestProgress {
    label: String,
    interactive: bool,
    last_render_at: Option<Instant>,
    latest_sample_bps: Option<f64>,
    op_size: usize,
    op_started_at: Instant,
    phase: &'static str,
    rendered_width: usize,
    total_iterations: usize,
    total_steps: usize,
    verifying: bool,
    current_iteration: usize,
    current_step: usize,
}

pub fn benchmark(_workspace: &Path, args: BenchmarkArgs) -> Result<()> {
    let mut args = args;
    validate_args(&args)?;

    let config = Config::load()?;
    args.connect_args = resolve_connect_args(&args.connect_args, &config)?;
    let use_stub = !args.connect_args.no_stub;

    let (chip, baud) = inspect_device(&args, &config)?;

    print_header_field("Chip", chip.as_str());
    print_header_field("Loader", if use_stub { "stub" } else { "ROM" });
    print_header_field("Baud", baud.to_string());
    print_header_field(
        "Benchmark region",
        format!(
            "0x{:08x} (+ up to {})",
            args.address,
            format_size(max_size(&args) as f64)
        ),
    );
    print_header_field("Iterations", args.iterations.to_string());
    println!();

    info!("Benchmarking connection time");
    let connection = benchmark_connection(&args, &config)?;

    println!("Connection:");
    println!(
        "  connect {:>16} +- {}",
        format_duration(connection.mean),
        format_duration(connection.stddev)
    );
    println!();

    let mut results = Vec::with_capacity(args.sizes.len());

    for size in &args.sizes {
        results.push(benchmark_size(&args, &config, use_stub, *size)?);
    }

    println!("Flash:");
    for result in results {
        println!("  {}", format_size(result.size as f64));
        println!(
            "    write {:>18} +- {}",
            format_rate(result.write.mean),
            format_rate(result.write.stddev)
        );
        println!(
            "    read  {:>18} +- {}",
            format_rate(result.read.mean),
            format_rate(result.read.stddev)
        );
        println!(
            "    skip  {:>18} +- {}",
            format_rate(result.skip.mean),
            format_rate(result.skip.stddev)
        );
    }

    Ok(())
}

fn print_header_field(label: &str, value: impl std::fmt::Display) {
    println!("{label:<HEADER_LABEL_WIDTH$} {value}");
}

fn validate_args(args: &BenchmarkArgs) -> Result<()> {
    if args.iterations == 0 {
        return Err("`--iterations` must be greater than zero".into());
    }

    if args.block_size == 0 {
        return Err("`--block-size` must be greater than zero".into());
    }

    if args.max_in_flight == 0 {
        return Err("`--max-in-flight` must be greater than zero".into());
    }

    if !args.address.is_multiple_of(FLASH_SECTOR_SIZE) {
        return Err(
            format!("benchmark address must be aligned to 0x{FLASH_SECTOR_SIZE:x} bytes").into(),
        );
    }

    for size in &args.sizes {
        if *size == 0 {
            return Err("benchmark sizes must be greater than zero".into());
        }

        if size % FLASH_SECTOR_SIZE != 0 {
            return Err(format!(
                "benchmark sizes must be aligned to 0x{FLASH_SECTOR_SIZE:x} bytes: 0x{size:x}"
            )
            .into());
        }
    }

    Ok(())
}

fn inspect_device(args: &BenchmarkArgs, config: &Config) -> Result<(String, u32)> {
    let mut flasher = connect(&args.connect_args, config, true, true)?;

    if flasher.secure_download_mode() {
        return Err(
            "flash benchmarking is not available in Secure Download Mode because flash reads and skip checks are restricted"
                .into(),
        );
    }

    let chip = flasher.chip().to_string();
    let baud = flasher.connection().baud()?;
    let reset_result = reset_after_benchmark(&args.connect_args, &mut flasher);

    reset_result?;

    Ok((chip, baud))
}

fn benchmark_connection(args: &BenchmarkArgs, config: &Config) -> Result<SampleSummary> {
    let mut samples = Vec::with_capacity(args.iterations);

    for _ in 0..args.iterations {
        let start = Instant::now();
        let mut flasher = connect(&args.connect_args, config, true, true)?;
        samples.push(start.elapsed().as_secs_f64());
        reset_after_benchmark(&args.connect_args, &mut flasher)?;
    }

    summarize(&samples)
}

fn benchmark_size(
    args: &BenchmarkArgs,
    config: &Config,
    use_stub: bool,
    size: u32,
) -> Result<TransferBenchmark> {
    let mut progress = TestProgress::new(format_size(size as f64));
    let (write, pattern) = benchmark_write(args, config, size, &mut progress)?;
    let read = benchmark_read(args, config, use_stub, size, &pattern, &mut progress)?;
    let skip = benchmark_skip(args, config, size, &pattern, &mut progress)?;

    Ok(TransferBenchmark {
        size,
        write,
        read,
        skip,
    })
}

fn benchmark_write(
    args: &BenchmarkArgs,
    config: &Config,
    size: u32,
    progress: &mut TestProgress,
) -> Result<(SampleSummary, Vec<u8>)> {
    let mut final_pattern = Vec::new();
    let summary = run_phase(
        args,
        config,
        progress,
        PhaseOptions {
            name: "write",
            steps: 1,
            skip_enabled: false,
        },
        size as usize,
        |flasher, iteration, progress| {
            let pattern = generate_pattern(size as usize, iteration as u32 + 1);
            let started = Instant::now();
            flasher.write_bin_to_flash(args.address, &pattern, progress)?;
            verify_flash_contents(flasher, args.address, size, &pattern)?;
            final_pattern = pattern;
            Ok(throughput(size, started.elapsed()))
        },
    )?;

    Ok((summary, final_pattern))
}

fn benchmark_read(
    args: &BenchmarkArgs,
    config: &Config,
    use_stub: bool,
    size: u32,
    pattern: &[u8],
    progress: &mut TestProgress,
) -> Result<SampleSummary> {
    run_phase(
        args,
        config,
        progress,
        PhaseOptions {
            name: "read",
            steps: read_step_count(size, args.block_size, use_stub),
            skip_enabled: false,
        },
        size as usize,
        |flasher, _iteration, progress| {
            let started = Instant::now();
            let readback = read_flash_region(
                flasher,
                args.address,
                size,
                args.block_size,
                args.max_in_flight,
                use_stub,
                progress,
            )?;
            verify_readback(args.address, pattern, &readback)?;
            Ok(throughput(size, started.elapsed()))
        },
    )
}

fn benchmark_skip(
    args: &BenchmarkArgs,
    config: &Config,
    size: u32,
    pattern: &[u8],
    progress: &mut TestProgress,
) -> Result<SampleSummary> {
    run_phase(
        args,
        config,
        progress,
        PhaseOptions {
            name: "skip",
            steps: 1,
            skip_enabled: true,
        },
        size as usize,
        |flasher, _iteration, progress| {
            let started = Instant::now();
            flasher.write_bin_to_flash(args.address, pattern, progress)?;
            Ok(throughput(size, started.elapsed()))
        },
    )
}

fn run_phase<F>(
    args: &BenchmarkArgs,
    config: &Config,
    progress: &mut TestProgress,
    phase: PhaseOptions,
    op_size: usize,
    mut run_iteration: F,
) -> Result<SampleSummary>
where
    F: FnMut(&mut Flasher, usize, &mut TestProgress) -> Result<f64>,
{
    let mut flasher = connect_for_phase(args, config, phase.skip_enabled)?;
    let mut samples = Vec::with_capacity(args.iterations);

    for iteration in 0..args.iterations {
        progress.begin_phase(
            phase.name,
            iteration + 1,
            args.iterations,
            phase.steps,
            op_size,
        );
        let sample = run_iteration(&mut flasher, iteration, progress)?;
        samples.push(sample);
        progress.complete_sample(sample);
    }

    reset_after_benchmark(&args.connect_args, &mut flasher)?;

    summarize(&samples)
}

fn connect_for_phase(args: &BenchmarkArgs, config: &Config, skip_enabled: bool) -> Result<Flasher> {
    let throughput_args = throughput_connect_args(&args.connect_args);
    Ok(connect(&throughput_args, config, true, !skip_enabled)?)
}

fn throughput_connect_args(connect_args: &ConnectArgs) -> ConnectArgs {
    let mut throughput_args = connect_args.clone();
    throughput_args.after = if connect_args.no_stub {
        ResetAfterOperation::NoReset
    } else {
        ResetAfterOperation::NoResetNoStub
    };
    throughput_args
}

impl TestProgress {
    fn new(label: String) -> Self {
        Self {
            label,
            interactive: io::stderr().is_terminal(),
            last_render_at: None,
            latest_sample_bps: None,
            op_size: 1,
            op_started_at: Instant::now(),
            phase: "write",
            rendered_width: 0,
            total_iterations: 1,
            total_steps: 1,
            verifying: false,
            current_iteration: 0,
            current_step: 0,
        }
    }

    fn begin_phase(
        &mut self,
        phase: &'static str,
        iteration: usize,
        total_iterations: usize,
        total_steps: usize,
        op_size: usize,
    ) {
        let phase_changed = self.current_iteration != 0 && phase != self.phase;

        if self.interactive && self.rendered_width > 0 && phase_changed {
            eprintln!();
            self.rendered_width = 0;
        }

        self.phase = phase;
        self.current_iteration = iteration;
        self.total_iterations = total_iterations;
        self.total_steps = total_steps.max(1);
        self.current_step = 0;
        self.op_size = op_size.max(1);
        self.op_started_at = Instant::now();
        self.latest_sample_bps = None;
        self.verifying = false;
        self.render(true);
    }

    fn update_read_step(&mut self, current_step: usize, bytes_read: usize) {
        self.current_step = current_step.min(self.total_steps);
        self.update_live_rate(bytes_read);
        self.render(false);
    }

    fn complete_sample(&mut self, sample_bps: f64) {
        self.current_step = self.total_steps;
        self.latest_sample_bps = Some(sample_bps);
        self.verifying = false;
        self.render(true);
    }

    fn render(&mut self, force: bool) {
        const RENDER_INTERVAL: Duration = Duration::from_millis(50);

        let now = Instant::now();
        if !force
            && self
                .last_render_at
                .is_some_and(|last| now.duration_since(last) < RENDER_INTERVAL)
        {
            return;
        }

        let line = progress_line(
            &self.label,
            self.phase,
            self.current_iteration,
            self.total_iterations,
            progress_percent(self.current_step, self.total_steps),
            self.latest_sample_bps,
            self.verifying,
        );

        if self.interactive {
            if self.rendered_width == 0 {
                eprint!("{line}");
            } else {
                let padding = " ".repeat(self.rendered_width.saturating_sub(line.len()));
                eprint!("\r{line}{padding}");
            }
            let _ = io::stderr().flush();
            self.rendered_width = line.len();
        } else if force {
            info!("{line}");
        }

        self.last_render_at = Some(now);
    }

    fn update_live_rate(&mut self, bytes_done: usize) {
        let elapsed = self.op_started_at.elapsed();
        if !elapsed.is_zero() && bytes_done > 0 {
            self.latest_sample_bps = Some(bytes_done as f64 / elapsed.as_secs_f64());
        }
    }
}

impl ProgressCallbacks for TestProgress {
    fn init(&mut self, _addr: u32, total: usize) {
        self.total_steps = total.max(1);
        self.current_step = 0;
        self.verifying = false;
    }

    fn update(&mut self, current: usize) {
        self.current_step = current.min(self.total_steps);
        let bytes_done = self.op_size.saturating_mul(self.current_step) / self.total_steps;
        self.update_live_rate(bytes_done);
        self.render(false);
    }

    fn verifying(&mut self) {
        self.current_step = self.total_steps;
        self.verifying = true;
        self.render(true);
    }

    fn finish(&mut self, _skipped: bool) {
        self.current_step = self.total_steps;
        self.update_live_rate(self.op_size);
        self.last_render_at = None;
    }
}

impl Drop for TestProgress {
    fn drop(&mut self) {
        if self.interactive && self.rendered_width > 0 {
            eprintln!();
            self.rendered_width = 0;
        }
    }
}

fn reset_after_benchmark(connect_args: &ConnectArgs, flasher: &mut Flasher) -> Result<()> {
    let chip = flasher.chip();
    flasher
        .connection()
        .reset_after(!connect_args.no_stub, chip)?;
    Ok(())
}

fn read_flash_region(
    flasher: &mut Flasher,
    offset: u32,
    size: u32,
    block_size: u32,
    max_in_flight: u32,
    use_stub: bool,
    progress: &mut TestProgress,
) -> Result<Vec<u8>> {
    if use_stub {
        read_flash_region_stub(flasher, offset, size, block_size, max_in_flight, progress)
    } else {
        read_flash_region_rom(flasher, offset, size, block_size, max_in_flight, progress)
    }
}

fn read_flash_region_stub(
    flasher: &mut Flasher,
    offset: u32,
    size: u32,
    block_size: u32,
    max_in_flight: u32,
    progress: &mut TestProgress,
) -> Result<Vec<u8>> {
    let connection = flasher.connection();
    let mut data = Vec::with_capacity(size as usize);
    let total_steps = read_step_count(size, block_size, true);

    connection.with_timeout(CommandType::ReadFlash.timeout(), |connection| {
        connection.command(Command::ReadFlash {
            offset,
            size,
            block_size,
            max_in_flight,
        })
    })?;

    while data.len() < size as usize {
        let current_step = data.len().div_ceil(block_size as usize) + 1;
        let response = connection.read_flash_response()?;
        let chunk: Vec<u8> = if let Some(response) = response {
            response.value.try_into()?
        } else {
            return Err(Error::IncorrectResponse.into());
        };

        data.extend_from_slice(&chunk);
        progress.update_read_step(current_step.min(total_steps), data.len().min(size as usize));

        if data.len() < size as usize && chunk.len() < block_size as usize {
            return Err(Error::CorruptData(block_size as usize, chunk.len()).into());
        }

        connection.write_raw(data.len() as u32)?;
    }

    if data.len() > size as usize {
        return Err(Error::ReadMoreThanExpected.into());
    }

    let response = connection.read_flash_response()?;
    let digest: Vec<u8> = if let Some(response) = response {
        response.value.try_into()?
    } else {
        return Err(Error::IncorrectResponse.into());
    };

    if digest.len() != 16 {
        return Err(Error::IncorrectDigestLength(digest.len()).into());
    }

    let mut md5_hasher = Md5::new();
    md5_hasher.update(&data);
    let checksum_md5 = md5_hasher.finalize();

    if digest != checksum_md5[..] {
        return Err(Error::DigestMismatch(digest, checksum_md5.to_vec()).into());
    }

    Ok(data)
}

fn read_flash_region_rom(
    flasher: &mut Flasher,
    offset: u32,
    size: u32,
    block_size: u32,
    max_in_flight: u32,
    progress: &mut TestProgress,
) -> Result<Vec<u8>> {
    const ROM_BLOCK_LEN: usize = 64;

    let connection = flasher.connection();
    let mut data = Vec::with_capacity(size as usize);
    let total_steps = read_step_count(size, block_size, false);

    while data.len() < size as usize {
        let chunk_len = usize::min(ROM_BLOCK_LEN, size as usize - data.len());
        let chunk_offset = offset + data.len() as u32;
        let current_step = data.len() / ROM_BLOCK_LEN + 1;

        let response =
            connection.with_timeout(CommandType::ReadFlashSlow.timeout(), |connection| {
                connection.command(Command::ReadFlashSlow {
                    offset: chunk_offset,
                    size: chunk_len as u32,
                    block_size,
                    max_in_flight,
                })
            })?;

        let payload: Vec<u8> = response.try_into()?;
        if payload.len() < chunk_len {
            return Err(Error::CorruptData(chunk_len, payload.len()).into());
        }

        data.extend_from_slice(&payload[..chunk_len]);
        progress.update_read_step(current_step.min(total_steps), data.len().min(size as usize));
    }

    Ok(data)
}

fn verify_readback(address: u32, expected: &[u8], actual: &[u8]) -> Result<()> {
    if expected == actual {
        return Ok(());
    }

    if expected.len() != actual.len() {
        return Err(format!(
            "readback length mismatch at 0x{address:08x}: expected {} bytes, got {} bytes",
            expected.len(),
            actual.len()
        )
        .into());
    }

    let mismatch = expected
        .iter()
        .zip(actual.iter())
        .position(|(expected, actual)| expected != actual)
        .expect("equal-length buffers differ");

    Err(format!(
        "readback mismatch at 0x{:08x}: expected 0x{:02x}, got 0x{:02x}",
        address + mismatch as u32,
        expected[mismatch],
        actual[mismatch]
    )
    .into())
}

fn verify_flash_contents(
    flasher: &mut Flasher,
    address: u32,
    size: u32,
    expected: &[u8],
) -> Result<()> {
    let flash_md5 = flasher.checksum_md5(address, size)?.to_be_bytes();
    let expected_md5 = Md5::digest(expected);

    if flash_md5 == expected_md5[..] {
        Ok(())
    } else {
        Err(Error::VerifyFailed.into())
    }
}

fn generate_pattern(size: usize, seed: u32) -> Vec<u8> {
    (0..size)
        .map(|index| {
            let mixed = (index as u32)
                .wrapping_mul(1_664_525)
                .wrapping_add(seed.wrapping_mul(1_013_904_223))
                .rotate_left((index % 31) as u32);

            (mixed ^ (mixed >> 8) ^ (mixed >> 16) ^ (mixed >> 24)) as u8
        })
        .collect()
}

fn max_size(args: &BenchmarkArgs) -> u32 {
    args.sizes
        .iter()
        .copied()
        .max()
        .unwrap_or(FLASH_SECTOR_SIZE)
}

fn progress_line(
    label: &str,
    phase: &str,
    iteration: usize,
    total_iterations: usize,
    percent: usize,
    latest_sample_bps: Option<f64>,
    verifying: bool,
) -> String {
    let iteration_width = total_iterations.to_string().len();
    let sample = latest_sample_bps.map(format_rate).unwrap_or_default();
    let status = if verifying { "verifying" } else { "" };

    format!(
        "Benchmarking {label:>10} region  {phase:<5}  ({iteration:>iteration_width$}/{total_iterations:>iteration_width$}, {percent:>3}%)  {sample:>14}  {status:<9}",
    )
}

fn progress_percent(step: usize, total: usize) -> usize {
    if total == 0 {
        return 100;
    }

    step.saturating_mul(100) / total
}

fn read_step_count(size: u32, block_size: u32, use_stub: bool) -> usize {
    let chunk_size = if use_stub { block_size as usize } else { 64 };
    (size as usize).div_ceil(chunk_size)
}

fn throughput(size: u32, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        f64::INFINITY
    } else {
        size as f64 / elapsed.as_secs_f64()
    }
}

fn summarize(samples: &[f64]) -> Result<SampleSummary> {
    if samples.is_empty() {
        return Err("benchmark produced no samples".into());
    }

    let mean = mean(samples);
    let stddev = std_deviation(samples, mean);

    Ok(SampleSummary { mean, stddev })
}

fn mean(samples: &[f64]) -> f64 {
    samples.iter().sum::<f64>() / samples.len() as f64
}

fn std_deviation(samples: &[f64], mean: f64) -> f64 {
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = mean - sample;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;

    variance.sqrt()
}

fn format_size(bytes: f64) -> String {
    format_scaled(bytes, &["B", "KiB", "MiB", "GiB"])
}

fn format_rate(bytes_per_second: f64) -> String {
    format!(
        "{}/s",
        format_scaled(bytes_per_second, &["B", "KiB", "MiB", "GiB"])
    )
}

fn format_duration(seconds: f64) -> String {
    if seconds >= 1.0 {
        format!("{seconds:.2}s")
    } else {
        format!("{:.2}ms", seconds * 1_000.0)
    }
}

fn format_scaled(mut value: f64, units: &[&str]) -> String {
    let mut unit = 0;

    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    format!("{value:.2} {}", units[unit])
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct TestParser {
        #[command(flatten)]
        args: BenchmarkArgs,
    }

    fn args() -> BenchmarkArgs {
        TestParser::parse_from(["xtask", "--address", "0x1000", "--size", "0x1000,0x2000"]).args
    }

    #[test]
    fn benchmark_args_require_sector_alignment() {
        let mut parsed = args();
        parsed.address = 1;
        assert!(validate_args(&parsed).is_err());

        let mut parsed = args();
        parsed.sizes = vec![0x1800];
        assert!(validate_args(&parsed).is_err());
    }

    #[test]
    fn summarize_calculates_mean_and_stddev() {
        let summary = summarize(&[10.0, 14.0, 18.0]).unwrap();
        assert!((summary.mean - 14.0).abs() < f64::EPSILON);
        assert!((summary.stddev - 3.265_986_323_710_904).abs() < 1e-12);
    }

    #[test]
    fn generate_pattern_is_stable() {
        let pattern = generate_pattern(8, 7);
        assert_eq!(pattern.len(), 8);
        assert_ne!(pattern, generate_pattern(8, 8));
    }

    #[test]
    fn progress_percent_rounds_to_whole_percent() {
        assert_eq!(progress_percent(1, 4), 25);
        assert_eq!(progress_percent(2, 3), 66);
        assert_eq!(progress_percent(4, 4), 100);
    }

    #[test]
    fn progress_line_includes_phase_and_iteration_counts() {
        assert_eq!(
            progress_line("64.00 KiB", "skip", 2, 5, 40, Some(131_072.0), false),
            "Benchmarking  64.00 KiB region  skip   (2/5,  40%)    128.00 KiB/s           "
        );
    }
}
