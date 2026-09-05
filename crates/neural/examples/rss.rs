//! How much resident memory each gallery model costs once loaded, and
//! how long a load takes (what re-warming after a release would cost).

fn rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with("VmRSS"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn main() {
    let mut last = rss_mb();
    println!("baseline: {last:.0} MB");
    for id in ["nsfw", "embed-image", "embed-text", "face", "face-embed"] {
        let started = std::time::Instant::now();
        let loaded = schist_neural::get(id).is_some();
        let now = rss_mb();
        println!(
            "{id}: loaded={loaded} in {:?}, +{:.0} MB (total {now:.0} MB)",
            started.elapsed(),
            now - last
        );
        last = now;
    }
}
