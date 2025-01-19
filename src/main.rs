use futures;
use once_cell::sync::Lazy;
use prometheus::{Gauge, Opts};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env::args_os;
use std::ffi::OsString;
use std::fs::{File, read_dir};
use std::io::{BufReader, BufRead, Error as IoError, ErrorKind as IoErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::{Arc, Mutex};
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

trait OrExitExt<T> {
    fn or_exit(self, message: &str) -> T;
}

impl<T, E: std::fmt::Display> OrExitExt<T> for Result<T, E> {
    fn or_exit(self, message: &str) -> T {
        match self {
            Ok(value) => value,
            Err(e) => {
                error!("{}: {}", message, e);
                exit(1);
            }
        }
    }
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

    let data: Arc<Mutex<PromData>> = Arc::new(Mutex::new(PromData {
        updated: Instant::now(),
        osd_requests: HashMap::new(),
        mds_requests: HashMap::new(),
    }));

    // Set up Prometheus
    let longest_opts = Opts::new("longest_request_seconds", "Duration of longest request");
    let longest_metric = Gauge::with_opts(longest_opts).unwrap();
    prometheus::default_registry()
        .register(Box::new(longest_metric.clone()))
        .unwrap();
    let longest_mds_opts = Opts::new("longest_mds_request_seconds", "Duration of longest MDS request");
    let longest_mds_metric = Gauge::with_opts(longest_mds_opts).unwrap();
    prometheus::default_registry()
        .register(Box::new(longest_mds_metric.clone()))
        .unwrap();

    let listener = std::net::TcpListener::bind(metrics_addr).or_exit("Can't create listener for metrics");

    // Start metrics server thread
    let prom_data: Arc<_> = data.clone();
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
                })
                .or(warp::path("requests").map(move || {
                    let data: &Mutex<_> = &*prom_data;
                    let data = data.lock().unwrap();

                    let mut buffer = Vec::new();

                    writeln!(buffer, "OSD:").unwrap();
                    let mut requests: Vec<_> = data.osd_requests.values().collect();
                    requests.sort_by(|&(target1, since1), &(target2, since2)| {
                        since2.cmp(since1).then(target1.cmp(target2))
                    });
                    for (target, since) in requests {
                        writeln!(
                            buffer, "osd.{:<5} {:>5.1} second",
                            target,
                            data.updated.duration_since(*since).as_secs_f64(),
                        ).unwrap();
                    }

                    writeln!(buffer, "MDS:").unwrap();
                    let mut requests: Vec<_> = data.mds_requests.values().collect();
                    requests.sort_by(|&(target1, since1), &(target2, since2)| {
                        since2.cmp(since1).then(target1.cmp(target2))
                    });
                    for (target, since) in requests {
                        match target {
                            Some(target) => writeln!(
                                buffer, "mds{} {:.1} second",
                                target,
                                data.updated.duration_since(*since).as_secs_f64(),
                            ).unwrap(),
                            None => writeln!(
                                buffer, "(none) {:.1} second",
                                data.updated.duration_since(*since).as_secs_f64(),
                            ).unwrap(),
                        }
                    }

                    buffer
                }));
                listener.set_nonblocking(true).unwrap();
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let incoming = futures::stream::unfold(listener, |listener| async move {
                    let client = listener.accept().await.map(|(c, _)| c);
                    Some((client, listener))
                });
                warp::serve(routes).run_incoming(incoming).await;
            });
        });
    }

    let data: &Mutex<_> = &*data;

    loop {
        update_data(
            &mut data.lock().unwrap(),
            &longest_metric,
            &longest_mds_metric,
            &debugfs,
            no_ceph_ok,
        );

        // Wait before next measurement
        std::thread::sleep(Duration::from_secs_f32(interval));
    }
}

struct PromData {
    updated: Instant,
    osd_requests: HashMap<u64, (u32, Instant)>,
    mds_requests: HashMap<u64, (Option<u32>, Instant)>,
}

fn update_data(
    data: &mut PromData,
    longest_metric: &Gauge,
    longest_mds_metric: &Gauge,
    debugfs: &Path,
    no_ceph_ok: bool,
) {
    let mut seen_osd_tids: HashSet<u64> = HashSet::new();
    let mut seen_mds_tids: HashSet<u64> = HashSet::new();
    let &mut PromData {
        ref mut updated,
        ref mut osd_requests,
        ref mut mds_requests,
    } = &mut *data;

    *updated = Instant::now();
    let mut longest_osd: f64 = 0.0;
    let mut longest_mds: f64 = 0.0;

    // Loop on clients
    let dir = match read_dir(&debugfs) {
        Err(e) if e.kind() == IoErrorKind::NotFound && no_ceph_ok => return,
        o => o,
    }.or_exit("Error reading debug filesystem");
    for client in dir {
        let client = client.or_exit("Error reading debug filesystem").path();

        // Read OSD requests from osdc
        {
            let osdc = File::open(client.join("osdc")).or_exit("Error opening osdc on debug filesystem");
            let osdc = BufReader::new(osdc);
            let osdc = parse_osdc(osdc).or_exit("Error parsing osdc on debug filesystem");

            for request in &osdc.requests {
                match osd_requests.entry(request.tid) {
                    std::collections::hash_map::Entry::Occupied(value) => {
                        let (_, first_seen) = *value.get();
                        longest_osd = longest_osd.max(updated.duration_since(first_seen).as_secs_f64());
                    }
                    std::collections::hash_map::Entry::Vacant(value) => {
                        value.insert((request.target, *updated));
                    }
                }
                seen_osd_tids.insert(request.tid);
            }
        }

        // Read MDS requests from mdsc
        {
            let mdsc = match File::open(client.join("mdsc")) {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.kind() == IoErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }.or_exit("Error opening mdsc on debug filesystem");
            if let Some(mdsc) = mdsc {
                let mdsc = BufReader::new(mdsc);
                let mdsc = parse_mdsc(mdsc).or_exit("Error parsing mdsc on debug filesystem");

                for request in &mdsc.requests {
                    match mds_requests.entry(request.tid) {
                        std::collections::hash_map::Entry::Occupied(value) => {
                            let (_, first_seen) = *value.get();
                            longest_mds = longest_mds.max(updated.duration_since(first_seen).as_secs_f64());
                        }
                        std::collections::hash_map::Entry::Vacant(value) => {
                            value.insert((request.target, *updated));
                        }
                    }
                    seen_mds_tids.insert(request.tid);
                }
            }
        }
    }

    // Forget unseen requests
    osd_requests.retain(|k, _| seen_osd_tids.contains(k));
    mds_requests.retain(|k, _| seen_mds_tids.contains(k));

    // Set metrics
    longest_metric.set(longest_osd);
    longest_mds_metric.set(longest_mds);
}

fn get_num_from_regex<F: std::str::FromStr>(m: Option<regex::Match>) -> Result<F, IoError> {
    match m.unwrap().as_str().parse::<F>() {
        Ok(i) => Ok(i),
        Err(_) => return Err(IoError::new(IoErrorKind::InvalidData, "Invalid field")),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Osdc {
    requests: Vec<OsdRequest>,
}

#[derive(Debug, PartialEq, Eq)]
struct OsdRequest {
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

        // https://github.com/torvalds/linux/blob/7d4050728c83aa63828494ad0f4d0eb4faf5f97a/net/ceph/debugfs.c#L183
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
            None => return Err(IoError::new(IoErrorKind::InvalidData, format!("Invalid request line: {}", line))),
        };
        osdc.requests.push(OsdRequest {
            tid: get_num_from_regex(cap.name("tid"))?,
            pool: get_num_from_regex(cap.name("pool"))?,
            target: get_num_from_regex(cap.name("target"))?,
        });
    }
    Ok(osdc)
}

#[derive(Debug, PartialEq, Eq)]
struct Mdsc {
    requests: Vec<MdsRequest>,
}

#[derive(Debug, PartialEq, Eq)]
struct MdsRequest {
    tid: u64,
    op: String,
    target: Option<u32>,
}

fn parse_mdsc<R: BufRead>(mut file: R) -> Result<Mdsc, IoError> {
    let mut mdsc = Mdsc {
        requests: Vec::new(),
    };
    let mut line = String::new();
    loop {
        line.clear();
        match file.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                let mut sample = [0u8; 512];
                match file.read(&mut sample) {
                    Ok(l) => error!("mdsc: {}: {:?}", e, &sample[0..l]),
                    Err(_) => error!("mdsc: {}", e),
                }
                return Err(e);
            }
        }

        // https://github.com/torvalds/linux/blob/7d4050728c83aa63828494ad0f4d0eb4faf5f97a/fs/ceph/debugfs.c#L52
        static REQUEST_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(
            "^(?P<tid>[0-9]+)\
            \t(\\(no request\\)|mds(?P<target>[0-9]+))\
            \t(?P<op>[^ ]+)\
            \t"
        ).unwrap());
        let cap = match REQUEST_REGEX.captures(&line) {
            Some(c) => c,
            None => return Err(IoError::new(IoErrorKind::InvalidData, format!("Invalid request line: {}", line))),
        };
        mdsc.requests.push(MdsRequest {
            tid: get_num_from_regex(cap.name("tid"))?,
            op: cap.name("op").unwrap().as_str().to_owned(),
            target: match cap.name("target") {
                Some(c) => Some(get_num_from_regex(Some(c))?),
                None => None,
            },
        });
    }
    Ok(mdsc)
}

#[test]
fn test_parse_osdc() {
    use std::io::Cursor;

    let file = Cursor::new("REQUESTS 1 homeless 0\n8141767\tosd807\t40.3a421a86\t40.286s0\t[807,793,674,167]/807\t[807,793,674,167]/807\te1177852\trbd_data.39.380eda51faf086.0000000000076984\t0x400024\t1\twrite\nLINGER REQUESTS\n");
    let osdc = parse_osdc(file).unwrap();
    assert_eq!(osdc, Osdc {
        requests: vec![
            OsdRequest {
                tid: 8141767,
                pool: 40,
                target: 807,
            },
        ],
    });
}

#[test]
fn test_parse_mdsc() {
    use std::io::Cursor;

    let file = Cursor::new(concat!(
        "3923\tmds0\treaddir\t#10004c8cd1d\n",
        "8317\t(no request)\tgetattr\t#1\n",
    ));
    let mdsc = parse_mdsc(file).unwrap();
    assert_eq!(mdsc, Mdsc {
        requests: vec![
            MdsRequest {
                tid: 3923,
                op: "readdir".to_owned(),
                target: Some(0),
            },
            MdsRequest {
                tid: 8317,
                op: "getattr".to_owned(),
                target: None,
            },
        ],
    });
}
