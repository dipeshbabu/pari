use std::{env, error::Error, path::PathBuf};

use pari_bench::{
    compare_reports, read_report, run_benchmark, run_storage_benchmark, write_comparison,
    write_report, BenchmarkConfig, MetricDirection,
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".into());
    let arguments: Vec<String> = arguments.collect();
    match command.as_str() {
        "run" => run_command(&arguments),
        "storage" => storage_command(&arguments),
        "compare" => compare_command(&arguments),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command {other:?}; use `pari-bench help`").into()),
    }
}

fn run_command(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    if has_help(arguments) {
        print_run_help();
        return Ok(());
    }
    let (config, output) = parse_benchmark_options(arguments, "pari-benchmark.json")?;
    let report = run_benchmark(config)?;
    write_report(&output, &report)?;
    println!("wrote {}", output.display());
    print_metric_summary(&report);
    Ok(())
}

fn storage_command(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    if has_help(arguments) {
        print_storage_help();
        return Ok(());
    }
    let (config, output) = parse_benchmark_options(arguments, "pari-storage-benchmark.json")?;
    let report = run_storage_benchmark(&config)?;
    write_report(&output, &report)?;
    println!("wrote {}", output.display());
    print_metric_summary(&report);
    Ok(())
}

fn parse_benchmark_options(
    arguments: &[String],
    default_output: &str,
) -> Result<(BenchmarkConfig, PathBuf), Box<dyn Error>> {
    let mut config = BenchmarkConfig {
        items: 5_000,
        queries: 100,
        set_size: 100,
        overlap: 90,
        threshold: 0.8,
        num_perm: 128,
        seed: 7,
        dataset: None,
    };
    let mut output = PathBuf::from(default_output);

    let mut index = 0;
    while index < arguments.len() {
        let flag = &arguments[index];
        index += 1;
        match flag.as_str() {
            "--items" => config.items = parse_value(arguments, &mut index, flag)?,
            "--queries" => config.queries = parse_value(arguments, &mut index, flag)?,
            "--set-size" => config.set_size = parse_value(arguments, &mut index, flag)?,
            "--overlap" => config.overlap = parse_value(arguments, &mut index, flag)?,
            "--threshold" => config.threshold = parse_value(arguments, &mut index, flag)?,
            "--num-perm" => config.num_perm = parse_value(arguments, &mut index, flag)?,
            "--seed" => config.seed = parse_value(arguments, &mut index, flag)?,
            "--dataset" => config.dataset = Some(next_string(arguments, &mut index, flag)?),
            "--output" => output = PathBuf::from(next_string(arguments, &mut index, flag)?),
            other => return Err(format!("unknown benchmark option {other:?}").into()),
        }
    }
    Ok((config, output))
}

fn compare_command(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    if arguments.len() < 2 {
        return Err("usage: pari-bench compare BASELINE.json CURRENT.json [--output PATH]".into());
    }
    let baseline_path = PathBuf::from(&arguments[0]);
    let current_path = PathBuf::from(&arguments[1]);
    let mut output = PathBuf::from("pari-benchmark-comparison.json");

    let mut index = 2;
    while index < arguments.len() {
        let flag = &arguments[index];
        index += 1;
        match flag.as_str() {
            "--output" => output = PathBuf::from(next_string(arguments, &mut index, flag)?),
            "--help" | "-h" => {
                print_compare_help();
                return Ok(());
            }
            other => return Err(format!("unknown compare option {other:?}").into()),
        }
    }

    let baseline = read_report(&baseline_path)?;
    let current = read_report(&current_path)?;
    let comparison = compare_reports(&baseline, &current);
    write_comparison(&output, &comparison)?;
    println!("wrote {}", output.display());
    for (name, delta) in &comparison.metrics {
        let improvement = delta
            .improvement_percent
            .map_or_else(|| "n/a".into(), |value| format!("{value:+.2}%"));
        println!(
            "{name}: {:.6} -> {:.6} {} ({improvement} improvement)",
            delta.baseline, delta.current, delta.unit
        );
    }
    Ok(())
}

fn has_help(arguments: &[String]) -> bool {
    arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
}

fn parse_value<T>(arguments: &[String], index: &mut usize, flag: &str) -> Result<T, Box<dyn Error>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = next_string(arguments, index, flag)?;
    raw.parse::<T>()
        .map_err(|error| format!("invalid value for {flag}: {raw:?}: {error}").into())
}

fn next_string(
    arguments: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, Box<dyn Error>> {
    let value = arguments
        .get(*index)
        .ok_or_else(|| format!("missing value after {flag}"))?
        .clone();
    *index += 1;
    Ok(value)
}

fn print_metric_summary(report: &pari_bench::BenchmarkReport) {
    println!(
        "engine={} git={}",
        report.engine, report.environment.git_sha
    );
    for (name, metric) in &report.metrics {
        let direction = match metric.direction {
            MetricDirection::Higher => "higher",
            MetricDirection::Lower => "lower",
            MetricDirection::Neutral => "neutral",
        };
        println!("{name}={:.6} {} ({direction})", metric.value, metric.unit);
    }
}

fn print_help() {
    println!(
        "Pari benchmark harness\n\n  pari-bench run [OPTIONS]\n  pari-bench storage [OPTIONS]\n  pari-bench compare BASELINE.json CURRENT.json [OPTIONS]\n\nUse a command with --help for details."
    );
}

fn print_run_help() {
    print_benchmark_help("run", "pari-benchmark.json");
}

fn print_storage_help() {
    print_benchmark_help("storage", "pari-storage-benchmark.json");
}

fn print_benchmark_help(command: &str, output: &str) {
    println!(
        "Usage: pari-bench {command} [OPTIONS]\n\nOptions:\n  --items N          corpus items or real-dataset row limit (default 5000)\n  --queries N        query count (default 100)\n  --set-size N       synthetic features per item (default 100)\n  --overlap N        source features retained per query (default 90)\n  --threshold X      LSH target threshold (default 0.8)\n  --num-perm N       MinHash permutations (default 128)\n  --seed N           deterministic seed (default 7)\n  --dataset PATH     whitespace-separated integer-set dataset\n  --output PATH      report JSON (default {output})"
    );
}

fn print_compare_help() {
    println!(
        "Usage: pari-bench compare BASELINE.json CURRENT.json [--output PATH]\n\nShared metrics are compared only when their units and optimization directions match."
    );
}
