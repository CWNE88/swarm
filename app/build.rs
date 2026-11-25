//! Stamps the build with when it was made (`swarm_app::BUILT`): shown at
//! the top of the Help tab and by `swarm --version`. Made again whenever
//! the source of any part of Swarm changes, so a build says when its code
//! was last built.

fn main() {
    for dir in ["src", "manual", "Cargo.toml", "../core/src", "../cli/src", "../desktop/src", "../desktop/ui", "../android/src"] {
        println!("cargo:rerun-if-changed={dir}");
    }
    let local = std::process::Command::new("date").arg("+%Y-%m-%d %H:%M").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let built = local.filter(|s| !s.is_empty()).unwrap_or_else(utc_now);
    println!("cargo:rustc-env=SWARM_BUILT={built}");
}

fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}
