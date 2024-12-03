use once_cell::sync::Lazy;
use prometheus::{Gauge, Opts};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env::args_os;
use std::ffi::OsString;
use std::fs::{File, read_dir};
use std::io::{BufReader, BufRead, Error as IoError, ErrorKind as IoErrorKind};
use std::path::PathBuf;
use std::process::exit;
use std::time::{Duration, Instant};
use tracing::{error, info};

fn parse_option<R: std::str::FromStr>(opt: Option<OsString>, flag: &'static str) -> R {
    let opt = match opt {
        Some(o) => o,
        None => {
            eprintln!("Missing value for {}", flag);
            exit(2);
        }
    };
    if let Some(opt) = opt.to_str() {
        if let Ok(opt) = opt.parse() {
            return opt;
        }
    }
    eprintln!("Invalid value for {}", flag);
    exit(2);
}

fn main() {
    // Initialize logging
    pretty_env_logger::init();

    // Parse command line
    let mut interval = 5.0;
    let mut metrics_addr: std::net::SocketAddr = ([0, 0, 0, 0], 8080).into();
    let mut debugfs: PathBuf = "/sys/kernel/debug".into();
    let mut no_ceph_ok = false;

    let mut args = args_os();
    args.next();
    let usage = "\
Usage: ceph-long-request-watcher [options]
Options:
    --interval SECONDS
        Check requests every SECONDS
    --metrics ADDR:PORT
        Expose the statistics on HTTP ADDR:PORT (default: 0.0.0.0:8080)
    --debugfs PATH
        Location to debug filesystem (default: /sys/kernel/debug)
    --no-ceph-ok
        Don't error out if debug/ceph doesn't exist at all
        (e.g. module not loaded)";
    while let Some(arg) = args.next() {
        if &arg == "--help" {
            println!("{}", usage);
            exit(0);
        } else if &arg == "--interval" {
            interval = parse_option(args.next(), "--interval");
        } else if &arg == "--metrics" {
            metrics_addr = parse_option(args.next(), "--metrics");
        } else if &arg == "--debugfs" {
            match args.next() {
                Some(s) => {
                    debugfs.clear();
                    debugfs.push(s);
                }
                None => {
                    eprintln!("Missing argument");
                    exit(2);
                }
            }
        } else if &arg == "--no-ceph-ok" {
            no_ceph_ok = true;
        } else {
            eprintln!("Too many arguments");
            eprintln!("{}", usage);
            exit(2);
        }
    }

    debugfs.push("ceph");

    // Set up Prometheus
    let longest_opts = Opts::new("longest_request_seconds", "Duration of longest request");
    let longest_metric = Gauge::with_opts(longest_opts).unwrap();
    prometheus::default_registry()
        .register(Box::new(longest_metric.clone()))
        .unwrap();

    // Start metrics server thread
    {
        use prometheus::Encoder;
        use tokio::runtime::Builder;
        use warp::Filter;
        use warp::http::Response;

        std::thread::spawn(move || {
            info!("Starting Prometheus HTTP server on {}", metrics_addr);

            let rt = Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let routes = warp::path("metrics").map(move || {
                    let mut buffer = Vec::new();
                    let encoder = prometheus::TextEncoder::new();
                    let metric_families = prometheus::gather();
                    encoder.encode(&metric_families, &mut buffer).unwrap();
                    Response::builder()
                        .header("Content-type", "text/plain")
                        .body(buffer)
                });
                warp::serve(routes).run(metrics_addr).await;
            });
        });
    }

    let mut requests: HashMap<u64, Instant> = HashMap::new();
    let mut seen_tids: HashSet<u64> = HashSet::new();

    loop {
        let now = Instant::now();
        seen_tids.clear();
        let mut longest: f64 = 0.0;

        // Loop on clients
        let dir = match read_dir(&debugfs) {
            Ok(d) => d,
            Err(e) => {
                if e.kind() == IoErrorKind::NotFound && no_ceph_ok {
                    std::thread::sleep(Duration::from_secs_f32(interval));
                    continue;
                } else {
                    error!("Error reading debug filesystem: {}", e);
                    exit(1);
                }
            }
        };
        for client in dir {
            let mut client = match client {
                Ok(c) => c.path(),
                Err(e) => {
                    error!("Error reading debug filesystem: {}", e);
                    exit(1);
                }
            };
            client.push("osdc");
            let osdc = match File::open(client) {
                Ok(f) => f,
                Err(e) => {
                    error!("Error opening osdc on debug filesystem: {}", e);
                    exit(1);
                }
            };
            let osdc = BufReader::new(osdc);
            let osdc = match parse_osdc(osdc) {
                Ok(o) => o,
                Err(e) => {
                    error!("Error parsing osdc on debug filesystem: {}", e);
                    exit(1);
                }
            };

            for request in &osdc.requests {
                match requests.entry(request.tid) {
                    std::collections::hash_map::Entry::Occupied(value) => {
                        let first_seen = *value.get();
                        longest = longest.max(now.duration_since(first_seen).as_secs_f64());
                    }
                    std::collections::hash_map::Entry::Vacant(value) => {
                        value.insert(now);
                    }
                }
                seen_tids.insert(request.tid);
            }
        }

        // Forget unseen requests
        requests.retain(|k, _| seen_tids.contains(k));

        // Set metric
        longest_metric.set(longest);

        // Wait before next measurement
        std::thread::sleep(Duration::from_secs_f32(interval));
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Osdc {
    requests: Vec<Request>,
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    tid: u64,
    pool: u32,
    target: u32,
}

fn parse_osdc<R: BufRead>(mut file: R) -> Result<Osdc, IoError> {
    let mut osdc = Osdc {
        requests: Vec::new(),
    };
    let mut line = String::new();
    file.read_line(&mut line)?;
    if !line.starts_with("REQUESTS ") {
        return Err(IoError::new(IoErrorKind::InvalidData, "Invalid first line"));
    }
    loop {
        line.clear();
        file.read_line(&mut line)?;

        if line.starts_with("LINGER REQUESTS") {
            break;
        }

        static REQUEST_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(
            "^(?P<tid>[0-9]+)\
            \tosd(?P<target>[0-9]+)\
            \t(?P<pool>[0-9]+)\\.(?P<seed>[0-9a-z]+)\
            \t(?P<pgid>[0-9]+\\.[0-9a-z]+)\
            \t\\[(?P<up>[0-9,]+)\\]/[0-9]+\
            \t\\[(?P<acting>[0-9,]+)\\]/[0-9]+\
            \t(?P<epoch>e[0-9]+)\
            \t(?:(?P<namespace>[^ /])/)?(?P<oid>[^ /]+)\
            \t(?P<flags>0x[0-9a-f]+)\
            (?P<paused>\tP)?\
            \t(?P<attempts>[0-9]+)\
            \t",
        ).unwrap());
        let cap = match REQUEST_REGEX.captures(&line) {
            Some(c) => c,
            None => {
                return Err(IoError::new(IoErrorKind::InvalidData, "Invalid request line"));
            }
        };
        fn get_num<F: std::str::FromStr>(m: Option<regex::Match>) -> Result<F, IoError> {
            match m.unwrap().as_str().parse::<F>() {
                Ok(i) => Ok(i),
                Err(_) => return Err(IoError::new(IoErrorKind::InvalidData, "Invalid field")),
            }
        }
        osdc.requests.push(Request {
            tid: get_num(cap.name("tid"))?,
            pool: get_num(cap.name("pool"))?,
            target: get_num(cap.name("target"))?,
        });
    }
    Ok(osdc)
}

#[test]
fn test_parse_osdc() {
    use std::io::Cursor;

    let file = Cursor::new("REQUESTS 1 homeless 0\n8141767\tosd807\t40.3a421a86\t40.286s0\t[807,793,674,167]/807\t[807,793,674,167]/807\te1177852\trbd_data.39.380eda51faf086.0000000000076984\t0x400024\t1\twrite\nLINGER REQUESTS\n");
    let osdc = parse_osdc(file).unwrap();
    assert_eq!(osdc, Osdc {
        requests: vec![
            Request {
                tid: 8141767,
                pool: 40,
                target: 807,
            },
        ],
    });
}
