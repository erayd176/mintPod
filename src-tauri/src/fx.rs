use std::{
    fs,
    io::Write,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};

const ECB_DAILY_RATES_URL: &str = "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml";
const CACHE_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1_000;
const CONSERVATIVE_FALLBACK: f64 = 1.0;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FxCache {
    usd_to_eur: f64,
    fetched_at_epoch_ms: u64,
}

pub async fn usd_to_eur(cache_path: &Path) -> f64 {
    let now = now_epoch_ms();
    let cached = read_cache(cache_path);
    if let Some(cache) = cached.as_ref()
        && now.saturating_sub(cache.fetched_at_epoch_ms) <= CACHE_MAX_AGE_MS
    {
        return cache.usd_to_eur;
    }

    match fetch_rate().await {
        Some(rate) => {
            let _ = write_cache(
                cache_path,
                &FxCache {
                    usd_to_eur: rate,
                    fetched_at_epoch_ms: now,
                },
            );
            rate
        }
        None => cached
            .map(|cache| cache.usd_to_eur)
            .unwrap_or(CONSERVATIVE_FALLBACK),
    }
}

async fn fetch_rate() -> Option<f64> {
    let xml = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(6))
        .build()
        .ok()?
        .get(ECB_DAILY_RATES_URL)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    parse_usd_to_eur(&xml)
}

fn parse_usd_to_eur(xml: &str) -> Option<f64> {
    let marker = if xml.contains("currency='USD'") {
        "currency='USD'"
    } else {
        "currency=\"USD\""
    };
    let usd = xml.get(xml.find(marker)?..)?;
    let (rate_marker, quote) = if usd.contains("rate='") {
        ("rate='", '\'')
    } else {
        ("rate=\"", '"')
    };
    let rate = usd.get(usd.find(rate_marker)? + rate_marker.len()..)?;
    let usd_per_eur = rate.get(..rate.find(quote)?)?.parse::<f64>().ok()?;
    (usd_per_eur.is_finite() && usd_per_eur > 0.0).then_some(1.0 / usd_per_eur)
}

fn read_cache(path: &Path) -> Option<FxCache> {
    let cache = serde_json::from_slice::<FxCache>(&fs::read(path).ok()?).ok()?;
    (cache.usd_to_eur.is_finite() && cache.usd_to_eur > 0.0).then_some(cache)
}

fn write_cache(path: &Path, cache: &FxCache) -> Result<(), std::io::Error> {
    let mut contents = serde_json::to_vec_pretty(cache).expect("an FX cache always serializes");
    contents.push(b'\n');
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&contents)?;
    file.sync_all()?;
    file.commit()
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_ecb_daily_xml_quote() {
        let rate = parse_usd_to_eur(
            "<Cube><Cube time='2026-07-27'><Cube currency='USD' rate='1.1389'/></Cube></Cube>",
        )
        .unwrap();

        assert!((rate - 0.8780402).abs() < 0.000_001);
    }

    #[test]
    fn rejects_missing_or_invalid_usd_rates() {
        assert_eq!(parse_usd_to_eur("<Cube currency='GBP' rate='0.85'/>"), None);
        assert_eq!(parse_usd_to_eur("<Cube currency='USD' rate='zero'/>"), None);
    }
}
