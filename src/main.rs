use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use geohash::Coord;
use grindr::{DeviceInfo, GrindrClient, Method, Session};

const MIN_INTERVAL: Duration = Duration::from_secs(2 * 60);
const MAX_INTERVAL: Duration = Duration::from_secs(9 * 60 + 59);

const JIGGLE_METERS: f64 = 1200.0;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = data_dir()?;
    ensure_data_dir(&data_dir)?;

    let base_geohash = std::env::var("GRINDR_GEOHASH")
        .ok()
        .filter(|g| !g.trim().is_empty())
        .ok_or("set GRINDR_GEOHASH")?;

    let device = load_device(&data_dir).unwrap_or_else(|| {
        let d = DeviceInfo::generate();
        save_device(&data_dir, &d);
        println!(
            "generated a new device identity -> {}",
            device_path(&data_dir).display()
        );
        d
    });

    let saved = load_session(&data_dir);
    let had_session = saved.is_some();
    let client = GrindrClient::new(device, saved)?;

    if !had_session {
        let email = std::env::var("GRINDR_EMAIL")
            .map_err(|_| "set GRINDR_EMAIL (and GRINDR_PASSWORD) to log in")?;
        let password = std::env::var("GRINDR_PASSWORD")
            .map_err(|_| "set GRINDR_PASSWORD (and GRINDR_EMAIL) to log in")?;

        println!("logging in as {email} …");
        client.login(&email, &password).await?;
        persist_session(&data_dir, &client);
    }

    let once = std::env::args().any(|a| a == "once");

    loop {
        let cascade_query = jiggled_query(&base_geohash);

        match run_cascade(&client, &cascade_query).await {
            Ok(()) => {}
            Err(e) => eprintln!("[{}] request failed: {e}", unix_now()),
        }

        persist_session(&data_dir, &client);

        if once {
            break;
        }

        let delay = random_delay();
        println!(
            "sleeping {}m{:02}s before the next one …",
            delay.as_secs() / 60,
            delay.as_secs() % 60
        );

        tokio::time::sleep(delay).await;
    }

    Ok(())
}

fn jiggled_query(base_geohash: &str) -> String {
    match jiggle_geohash(base_geohash, JIGGLE_METERS) {
        Ok(jiggled) => format!("nearbyGeoHash={jiggled}"),
        Err(e) => {
            eprintln!("[{}] geohash jiggle failed: {e}; using base", unix_now());
            format!("nearbyGeoHash={base_geohash}")
        }
    }
}

fn jiggle_geohash(base: &str, max_meters: f64) -> Result<String, geohash::GeohashError> {
    const METERS_PER_DEG_LAT: f64 = 111_320.0;

    let (coord, _, _) = geohash::decode(base)?;

    let bearing = rand::random_range(0.0..std::f64::consts::TAU);
    let distance = max_meters * rand::random_range(0.0..=1.0_f64).sqrt();

    let north = distance * bearing.cos();
    let east = distance * bearing.sin();

    let lat_rad = coord.y.to_radians();
    let d_lat = north / METERS_PER_DEG_LAT;
    let d_lon = east / (METERS_PER_DEG_LAT * lat_rad.cos().abs().max(1e-6));

    let jiggled = Coord {
        x: wrap_longitude(coord.x + d_lon),
        y: (coord.y + d_lat).clamp(-90.0, 90.0),
    };

    geohash::encode(jiggled, base.chars().count())
}

/// Wrap a longitude in degrees into `[-180, 180)` so a jiggle across the
/// antimeridian stays a coordinate `encode` accepts.
fn wrap_longitude(lon: f64) -> f64 {
    (lon + 540.0).rem_euclid(360.0) - 180.0
}

async fn run_cascade(
    client: &GrindrClient,
    cascade_query: &str,
) -> Result<(), grindr::GrindrError> {
    let path = if cascade_query.is_empty() {
        "/v4/cascade".to_owned()
    } else {
        format!("/v4/cascade?{cascade_query}")
    };

    let resp = client
        .request_authenticated_raw(Method::GET, &path, None)
        .await?;

    let body = String::from_utf8_lossy(&resp.body);
    let snippet: String = body.chars().take(200).collect();

    println!("[{}] GET {path} -> {}", unix_now(), resp.status);
    println!("    {snippet}");

    Ok(())
}

fn data_dir() -> Result<PathBuf, std::env::VarError> {
    match std::env::var("GRINDR_DATA_DIR") {
        Ok(dir) => Ok(PathBuf::from(dir)),
        Err(std::env::VarError::NotPresent) => Ok(PathBuf::from("./data")),
        Err(e) => Err(e),
    }
}

fn device_path(data_dir: &Path) -> PathBuf {
    data_dir.join("device.json")
}

fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("session.json")
}

fn ensure_data_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }

    Ok(())
}

fn persist_session(data_dir: &Path, client: &GrindrClient) {
    if let Some(session) = client.session_receiver().borrow().clone() {
        save_session(data_dir, &session);
    }
}

fn load_device(data_dir: &Path) -> Option<DeviceInfo> {
    serde_json::from_slice(&fs::read(device_path(data_dir)).ok()?).ok()
}

fn save_device(data_dir: &Path, device: &DeviceInfo) {
    match serde_json::to_vec_pretty(device) {
        Ok(bytes) => {
            let _ = write_secure_file(&device_path(data_dir), &bytes);
        }
        Err(e) => eprintln!("could not serialize device: {e}"),
    }
}

fn load_session(data_dir: &Path) -> Option<Session> {
    serde_json::from_slice(&fs::read(session_path(data_dir)).ok()?).ok()
}

fn save_session(data_dir: &Path, session: &Session) {
    match serde_json::to_vec_pretty(session) {
        Ok(bytes) => {
            let _ = write_secure_file(&session_path(data_dir), &bytes);
        }
        Err(e) => eprintln!("could not serialize session: {e}"),
    }
}

fn write_secure_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;

        file.write_all(bytes)?;
        file.sync_all()?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        fs::write(path, bytes)?;
        return Ok(());
    }
}

fn random_delay() -> Duration {
    let secs = rand::random_range(MIN_INTERVAL.as_secs()..=MAX_INTERVAL.as_secs());
    Duration::from_secs(secs)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn haversine_meters(a: Coord<f64>, b: Coord<f64>) -> f64 {
        const R: f64 = 6_371_000.0;
        let (lat1, lat2) = (a.y.to_radians(), b.y.to_radians());
        let d_lat = (b.y - a.y).to_radians();
        let d_lon = (b.x - a.x).to_radians();
        let h = (d_lat / 2.0).sin().powi(2)
            + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
        2.0 * R * h.sqrt().asin()
    }

    #[test]
    fn jiggle_stays_within_bounds_and_varies() {
        let base = "gcpvj0duq2yk";
        let (origin, _, _) = geohash::decode(base).unwrap();

        let mut moved = 0;
        let mut max_dist = 0.0f64;
        for _ in 0..10_000 {
            let jiggled = jiggle_geohash(base, JIGGLE_METERS).unwrap();
            assert_eq!(jiggled.chars().count(), base.chars().count());

            let (point, _, _) = geohash::decode(&jiggled).unwrap();
            let dist = haversine_meters(origin, point);
            max_dist = max_dist.max(dist);
            if jiggled != base {
                moved += 1;
            }
        }

        assert!(max_dist <= 5.5, "max displacement {max_dist} m exceeded budget");
        assert!(moved > 9_000, "geohash rarely changed ({moved}/10000)");
    }

    #[test]
    fn jiggle_across_antimeridian_stays_valid_and_close() {
        // Base geohash a fraction of a meter west of +180 longitude.
        let base = geohash::encode(Coord { x: 179.999_99, y: 0.0 }, 12).unwrap();
        let (origin, _, _) = geohash::decode(&base).unwrap();

        for _ in 0..10_000 {
            // Never falls back to Err: the wrap keeps longitude in range.
            let jiggled = jiggle_geohash(&base, JIGGLE_METERS).unwrap();
            let (point, _, _) = geohash::decode(&jiggled).unwrap();
            assert!((-180.0..180.0).contains(&point.x), "lon {} out of range", point.x);
            assert!(
                haversine_meters(origin, point) <= 5.5,
                "displacement exceeded budget across antimeridian"
            );
        }
    }

    #[test]
    fn wrap_longitude_maps_into_range() {
        assert!((wrap_longitude(180.00004) - -179.99996).abs() < 1e-6);
        assert!((wrap_longitude(-180.00004) - 179.99996).abs() < 1e-6);
        assert_eq!(wrap_longitude(0.0), 0.0);
        assert!((wrap_longitude(-0.09) - -0.09).abs() < 1e-9);
    }
}
