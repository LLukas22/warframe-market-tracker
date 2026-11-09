//! Scans riven screenshots and prints what was read.
//!
//! ```text
//! ocr/scripts/fetch-models.sh
//! cargo run --release -p wfmt-ocr --features engine --example scan -- [--tier small] [--lines] IMAGE...
//! ```

#[path = "../tests/support/mod.rs"]
mod support;

use std::path::Path;
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut tier = "tiny".to_string();
    if let Some(i) = args.iter().position(|a| a == "--tier") {
        tier = args.remove(i + 1);
        args.remove(i);
    }
    let show_lines = args.iter().any(|a| a == "--lines");
    args.retain(|a| a != "--lines");
    if args.is_empty() {
        eprintln!("usage: scan [--tier tiny|small] [--lines] IMAGE...");
        std::process::exit(2);
    }

    let started = Instant::now();
    let scanner = support::scanner(&tier);
    println!(
        "PP-OCRv6 {tier} loaded in {} ms",
        started.elapsed().as_millis()
    );

    for path in &args {
        let image = support::read_image(Path::new(path));
        let started = Instant::now();
        let scan = match scanner.scan(&image) {
            Ok(scan) => scan,
            Err(e) => {
                println!("\n{path}: {e}");
                continue;
            }
        };
        let r = &scan.riven;
        println!("\n{path} ({} ms)", started.elapsed().as_millis());
        if show_lines {
            for l in &scan.lines {
                println!(
                    "  | {:.2} [{:4.0},{:4.0} {:3.0}x{:2.0}] {}",
                    l.confidence, l.x, l.y, l.width, l.height, l.text
                );
            }
        }
        println!(
            "  weapon  {} ({:?}, {:.2})",
            r.weapon.as_deref().unwrap_or("?"),
            r.weapon_name.as_deref().unwrap_or(""),
            r.weapon_score
        );
        println!(
            "  name    {} ({:.2})",
            r.name.as_deref().unwrap_or("?"),
            r.name_score
        );
        println!("  MR {:?}, rerolls {:?}", r.mastery_rank, r.rerolls);
        for a in &r.attributes {
            println!(
                "  {} {:>8} {:<32} {:.2}  {}",
                if a.positive { '+' } else { '-' },
                a.value,
                a.slug,
                a.score,
                a.text
            );
        }
        println!(
            "  name agrees with stats: {:?}; stat model: {}",
            r.name_matches_attributes,
            support::check_fit(r)
        );
        for w in &r.warnings {
            println!("  warning: {w}");
        }
    }
}
