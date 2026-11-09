//! waypost-builder: turns prepared OSM extracts into one waypost index file.
//!
//! Input: the files `scripts/prep.sh` writes per extract:
//! `<prefix>.streets.pbf`, `<prefix>.areas.geojsonseq`, `<prefix>.date`.

mod areas;
mod streets;
mod tags;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use waypost_core::format::{BuildSettings, IndexWriter, Meta};
use waypost_core::model::{NameTable, StringTable, sec};

#[derive(Parser)]
#[command(version, about = "Builds a waypost index from prepared OSM extracts")]
struct Args {
    /// Output index file.
    #[arg(long)]
    out: PathBuf,
    /// Name languages to store besides `name`, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "en")]
    langs: Vec<String>,
    /// Street grid cell side in degrees.
    #[arg(long, default_value_t = 0.005)]
    street_cell: f64,
    /// Admin grid cell side in degrees.
    #[arg(long, default_value_t = 0.05)]
    admin_cell: f64,
    /// Prepared extract prefixes, for example data/prep/switzerland-latest.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
}

/// Shared tables every pass writes into.
pub struct Tables {
    pub strings: StringTable,
    pub names: NameTable,
    pub langs: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let t0 = Instant::now();
    let settings = BuildSettings {
        street_cell: (args.street_cell * 1e7).round() as i32,
        admin_cell: (args.admin_cell * 1e7).round() as i32,
        langs: args.langs.clone(),
    };
    anyhow::ensure!(
        settings.street_cell > 0 && settings.admin_cell > 0,
        "cell sizes must be positive"
    );

    let mut tables = Tables {
        strings: StringTable::default(),
        names: NameTable::default(),
        langs: args.langs.clone(),
    };
    let mut st = streets::Streets::new(&settings);
    let mut ar = areas::Areas::new(&settings);
    let mut sources = Vec::new();
    let mut dates = Vec::new();

    for prefix in &args.inputs {
        let name = prefix
            .file_name()
            .context("input has no file name")?
            .to_string_lossy()
            .to_string();
        let with = |ext: &str| PathBuf::from(format!("{}{}", prefix.display(), ext));
        st.read(&with(".streets.pbf"), &mut tables)
            .with_context(|| format!("reading streets of {name}"))?;
        ar.read(&with(".areas.geojsonseq"), &mut tables)
            .with_context(|| format!("reading areas of {name}"))?;
        if let Ok(d) = std::fs::read_to_string(with(".date")) {
            let d = d.trim();
            if !d.is_empty() {
                dates.push(d.to_string());
            }
        }
        sources.push(format!("{name}.osm.pbf"));
        eprintln!("[{:>6.1}s] read {name}", t0.elapsed().as_secs_f64());
    }

    let mut w = IndexWriter::create(&args.out)?;
    st.write(&mut w)?;
    eprintln!(
        "[{:>6.1}s] streets written: {}",
        t0.elapsed().as_secs_f64(),
        st.summary()
    );
    let area_summary = ar.summary();
    ar.write(&mut w)?;
    eprintln!(
        "[{:>6.1}s] areas written: {area_summary}",
        t0.elapsed().as_secs_f64()
    );
    w.add(sec::NAMES, &tables.names.names)?;
    w.add(sec::NAME_ALTS, &tables.names.alts)?;
    w.add_bytes(sec::STRINGS, &tables.strings.into_bytes())?;
    dates.sort();
    let meta = Meta {
        builder_version: env!("CARGO_PKG_VERSION").to_string(),
        built_at: now_rfc3339(),
        sources,
        data_date: dates.pop(),
        settings,
        ..Default::default()
    };
    let path = w.finish(meta)?;
    let size = std::fs::metadata(&path)?.len();
    eprintln!(
        "[{:>6.1}s] wrote {} ({:.1} MB)",
        t0.elapsed().as_secs_f64(),
        path.display(),
        size as f64 / 1e6
    );
    Ok(())
}

/// Current UTC time as RFC 3339, without a date crate.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}
