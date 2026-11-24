//! usbnet-doctor: checks USB port speeds and hubs, USB network adapters,
//! driver errors in the event log, ping quality and internet speed.

mod diag;
mod events;
mod net;
mod ping;
mod speed;
mod usb;
mod util;

use std::net::Ipv4Addr;
use std::process::exit;

use util::*;

const HELP: &str = "\
Diagnose USB ports, hubs and USB network adapters on Windows.

Usage: usbnet-doctor [COMMAND] [OPTIONS]

Commands:
  all      Everything below, then a diagnosis (default)
  usb      USB tree with the speed of every port and device
  net      Network adapters, their USB location and driver settings
  events   Driver errors and network disconnects from the event log
  ping     Packet loss and jitter to the router and the internet
  speed    Download/upload speed and latency under load

Options:
  --days <N>           Days of event log history to scan [default: 3]
  --pings <N>          Pings per target [default: 30]
  --mb <N>             Megabytes per download run in the speed test [default: 50]
  --rounds <N>         Download runs in the speed test [default: 2]
  --no-speed           Skip the internet speed test in `all`
  --provider <NAME>    Extra event-log provider (or part of its name) to watch; can repeat
  --events-shown <N>   Events to list per section [default: 12]
  -v, --verbose        Show every advanced driver setting
  --no-color           Plain output without colors
  -h, --help           Show this help
  -V, --version        Show the version";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmd {
    All,
    Usb,
    Net,
    Events,
    Ping,
    Speed,
}

struct Cli {
    cmd: Option<Cmd>,
    days: u32,
    pings: u32,
    mb: u64,
    rounds: u32,
    no_speed: bool,
    providers: Vec<String>,
    events_shown: usize,
    verbose: bool,
    no_color: bool,
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}\n\nRun `usbnet-doctor --help` for usage.");
    exit(2)
}

impl Cli {
    fn parse() -> Cli {
        let mut cli = Cli {
            cmd: None,
            days: 3,
            pings: 30,
            mb: 50,
            rounds: 2,
            no_speed: false,
            providers: vec![],
            events_shown: 12,
            verbose: false,
            no_color: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (a.clone(), None),
            };
            let mut value = |name: &str| -> String {
                inline.clone().or_else(|| args.next()).unwrap_or_else(|| fail(&format!("{name} needs a value")))
            };
            fn num<T: std::str::FromStr>(name: &str, v: String) -> T {
                v.parse().unwrap_or_else(|_| fail(&format!("{name} expects a number, got `{v}`")))
            }
            match flag.as_str() {
                "-h" | "--help" => {
                    println!("{HELP}");
                    exit(0)
                }
                "-V" | "--version" => {
                    println!("usbnet-doctor {}", env!("CARGO_PKG_VERSION"));
                    exit(0)
                }
                "--days" => cli.days = num("--days", value("--days")),
                "--pings" => cli.pings = num("--pings", value("--pings")),
                "--mb" => cli.mb = num("--mb", value("--mb")),
                "--rounds" => cli.rounds = num("--rounds", value("--rounds")),
                "--events-shown" => cli.events_shown = num("--events-shown", value("--events-shown")),
                "--provider" => cli.providers.push(value("--provider")),
                "--no-speed" => cli.no_speed = true,
                "-v" | "--verbose" => cli.verbose = true,
                "--no-color" => cli.no_color = true,
                s if s.starts_with('-') => fail(&format!("unknown option `{s}`")),
                s => {
                    if cli.cmd.is_some() {
                        fail(&format!("unexpected argument `{s}`"));
                    }
                    cli.cmd = Some(match s {
                        "all" => Cmd::All,
                        "usb" => Cmd::Usb,
                        "net" => Cmd::Net,
                        "events" => Cmd::Events,
                        "ping" => Cmd::Ping,
                        "speed" => Cmd::Speed,
                        _ => fail(&format!("unknown command `{s}`")),
                    });
                }
            }
        }
        cli
    }
}

fn ping_targets(adapters: &[net::Adapter]) -> Vec<(String, Ipv4Addr)> {
    let mut t: Vec<(String, Ipv4Addr)> = Vec::new();
    for a in adapters.iter().filter(|a| a.connected) {
        for gw in &a.gateways {
            if !t.iter().any(|(_, ip)| ip == gw) {
                t.push((format!("gateway ({})", a.alias), *gw));
            }
        }
    }
    t.push(("Cloudflare 1.1.1.1".into(), Ipv4Addr::new(1, 1, 1, 1)));
    t.push(("Google 8.8.8.8".into(), Ipv4Addr::new(8, 8, 8, 8)));
    t
}

fn main() {
    let cli = Cli::parse();
    init_color(cli.no_color);
    let cmd = cli.cmd.unwrap_or(Cmd::All);
    let all = cmd == Cmd::All;

    println!("{}", bold(format!("usbnet-doctor {}", env!("CARGO_PKG_VERSION"))));

    let need_usb = matches!(cmd, Cmd::All | Cmd::Usb | Cmd::Net);
    let need_net = !matches!(cmd, Cmd::Usb);

    let usb_tree = need_usb.then(usb::scan);
    let adapters = if need_net { net::scan(&usb::driver_map()) } else { vec![] };

    if let Some(t) = &usb_tree {
        if matches!(cmd, Cmd::All | Cmd::Usb) {
            t.print();
        }
    }
    if matches!(cmd, Cmd::All | Cmd::Net) {
        if let Some(t) = &usb_tree {
            net::print(&adapters, t, cli.verbose);
        }
    }

    let event_report = matches!(cmd, Cmd::All | Cmd::Events).then(|| {
        let mut providers = cli.providers.clone();
        providers.extend(adapters.iter().filter_map(|a| a.service.clone()));
        let r = events::scan(cli.days, &providers);
        r.print(cli.events_shown);
        r
    });

    let pings = matches!(cmd, Cmd::All | Cmd::Ping | Cmd::Speed).then(|| {
        let targets = ping_targets(&adapters);
        if cmd != Cmd::Speed {
            println!();
            println!("{}", dim(format!("Pinging {} targets, {} times each…", targets.len(), cli.pings)));
        }
        let count = if cmd == Cmd::Speed { 10 } else { cli.pings };
        let r = ping::ping_all(&targets, count);
        if cmd != Cmd::Speed {
            ping::print(&r);
        }
        r
    });

    let speed_report = (cmd == Cmd::Speed || (all && !cli.no_speed)).then(|| {
        println!();
        println!("{}", dim("Running the speed test…"));
        let r = speed::run(cli.mb, cli.rounds, Some(Ipv4Addr::new(1, 1, 1, 1)));
        let idle = pings.as_ref().and_then(|p| p.iter().find(|x| x.target == Ipv4Addr::new(1, 1, 1, 1)));
        speed::print(&r, idle);
        r
    });

    let inputs = diag::Inputs {
        usb: usb_tree.as_ref(),
        adapters: need_net.then_some(adapters.as_slice()),
        events: event_report.as_ref(),
        pings: pings.as_deref(),
        speed: speed_report.as_ref(),
    };
    diag::print(&diag::analyze(&inputs));
}
