// SPDX-FileCopyrightText: 2026 Marcus Baw and Baw Medical Ltd
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `sct filter` - filter a SNOMED CT NDJSON file and remap GTINs to kept ancestor concepts.

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;

use crate::schema::ConceptRecord;

#[derive(Parser, Debug)]
pub struct Args {
    /// Path to the input SNOMED CT NDJSON file.
    #[arg(long, short)]
    pub input: PathBuf,

    /// Path to write the filtered NDJSON file.
    #[arg(long, short)]
    pub output: PathBuf,

    /// Keep only concepts matching this ECL expression (requires a SQLite database via --db).
    #[arg(long)]
    pub keep_ecl: Option<String>,

    /// Path to a text file containing concept IDs to keep (one per line).
    #[arg(long)]
    pub keep_ids: Option<PathBuf>,

    /// Path to a SQLite database. Required if evaluating --keep-ecl.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Path to a CSV/TSV file mapping GTINs to concepts (format: gtin,concept_id).
    /// If provided, GTINs mapping to filtered-out concepts (e.g. AMPPs)
    /// will be remapped to their nearest kept ancestor (e.g. VMP/VTM).
    #[arg(long)]
    pub gtin_map: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<()> {
    // 1. Determine the set of concept IDs to keep
    let mut kept_ids = HashSet::new();

    if let Some(ecl_expr) = &args.keep_ecl {
        let db_path = crate::paths::resolve_db(args.db.as_deref())?.path;
        let conn = crate::commands::open_db_readonly(&db_path, None)?;
        crate::ecl::warn_if_no_tct(&conn);
        let ids = crate::ecl::expand(&conn, ecl_expr)?;
        kept_ids.extend(ids);
    }

    if let Some(ids_file) = &args.keep_ids {
        let file = std::fs::File::open(ids_file)
            .with_context(|| format!("opening keep-ids file {}", ids_file.display()))?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                kept_ids.insert(trimmed.to_string());
            }
        }
    }

    anyhow::ensure!(
        !kept_ids.is_empty(),
        "No concepts specified to keep. Provide --keep-ecl and/or --keep-ids."
    );

    // 2. Load the original NDJSON to build the hierarchy and get concepts
    eprintln!("Reading input NDJSON...");
    let file = std::fs::File::open(&args.input)
        .with_context(|| format!("opening input file {}", args.input.display()))?;
    let input_len = file.metadata()?.len();
    let reader = std::io::BufReader::new(file);

    let mut concepts = HashMap::new();
    let mut parents_map = HashMap::new();
    let mut provenance_line = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if line.contains("sct_provenance") {
            provenance_line = Some(line.clone());
            continue;
        }
        let record: ConceptRecord =
            serde_json::from_str(&line).context("deserializing ConceptRecord from NDJSON")?;

        let p_ids: Vec<String> = record.parents.iter().map(|p| p.id.clone()).collect();
        parents_map.insert(record.id.clone(), p_ids);
        concepts.insert(record.id.clone(), record);
    }

    // 3. If a GTIN map is provided, load and remap the GTINs
    let mut gtin_remaps: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(gtin_map_path) = &args.gtin_map {
        eprintln!("Loading and remapping GTINs...");
        let file = std::fs::File::open(gtin_map_path)
            .with_context(|| format!("opening GTIN map file {}", gtin_map_path.display()))?;
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(std::io::BufReader::new(file));

        let mut total_gtins = 0;
        let mut mapped_gtins = 0;

        for result in rdr.records() {
            let record = result?;
            if record.len() < 2 {
                continue;
            }
            let gtin = record.get(0).unwrap_or("").trim().to_string();
            let concept_id = record.get(1).unwrap_or("").trim().to_string();
            if gtin.is_empty() || concept_id.is_empty() {
                continue;
            }

            total_gtins += 1;

            // Find the nearest ancestor of concept_id that is in kept_ids
            if let Some(target_id) = find_kept_ancestor(&concept_id, &parents_map, &kept_ids) {
                gtin_remaps.entry(target_id).or_default().push(gtin);
                mapped_gtins += 1;
            }
        }
        eprintln!(
            "Mapped {} of {} GTINs to kept concepts.",
            mapped_gtins, total_gtins
        );
    }

    // 4. Filter concepts and apply GTIN mappings
    eprintln!("Filtering and writing output NDJSON...");
    let out_file = std::fs::File::create(&args.output)
        .with_context(|| format!("creating output file {}", args.output.display()))?;
    let mut writer = BufWriter::new(out_file);

    if let Some(prov) = provenance_line {
        writer.write_all(prov.as_bytes())?;
        writer.write_all(b"\n")?;
    }

    let mut kept_count = 0;
    let initial_count = concepts.len();

    // Iterate in sorted order of concept IDs to preserve stable NDJSON ordering
    let mut concept_ids: Vec<String> = concepts.keys().cloned().collect();
    concept_ids.sort_by(|a, b| {
        let a_num = a.parse::<u64>().ok();
        let b_num = b.parse::<u64>().ok();
        match (a_num, b_num) {
            (Some(an), Some(bn)) => an.cmp(&bn),
            _ => a.cmp(b),
        }
    });

    for id in &concept_ids {
        if kept_ids.contains(id) {
            let mut record = concepts.remove(id).unwrap();
            // Assign remapped GTINs
            if let Some(gtins) = gtin_remaps.remove(id) {
                record.gtin_codes = gtins;
            }
            let serialized = serde_json::to_string(&record)?;
            writer.write_all(serialized.as_bytes())?;
            writer.write_all(b"\n")?;
            kept_count += 1;
        }
    }
    writer.flush()?;

    let output_len = std::fs::metadata(&args.output)?.len();

    // 5. Print space savings summary
    println!("\nDatabase Filtering Summary");
    println!("==========================");
    println!(
        "Concepts kept:     {} / {} ({:.1}%)",
        fmt_count(kept_count as u64),
        fmt_count(initial_count as u64),
        (kept_count as f64 / initial_count as f64) * 100.0
    );
    println!("Initial file size:  {}", human_size(input_len));
    println!("Filtered file size: {}", human_size(output_len));
    let saved_bytes = input_len.saturating_sub(output_len);
    println!(
        "Space saved:        {} ({:.1}%)",
        human_size(saved_bytes),
        (saved_bytes as f64 / input_len as f64) * 100.0
    );

    Ok(())
}

fn find_kept_ancestor(
    concept_id: &str,
    parents_map: &HashMap<String, Vec<String>>,
    kept_ids: &HashSet<String>,
) -> Option<String> {
    if kept_ids.contains(concept_id) {
        return Some(concept_id.to_string());
    }

    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();

    queue.push_back(concept_id.to_string());
    visited.insert(concept_id.to_string());

    while let Some(current) = queue.pop_front() {
        if kept_ids.contains(&current) {
            return Some(current);
        }
        if let Some(parents) = parents_map.get(&current) {
            for parent in parents {
                if visited.insert(parent.clone()) {
                    queue.push_back(parent.clone());
                }
            }
        }
    }
    None
}

fn human_size(bytes: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}
