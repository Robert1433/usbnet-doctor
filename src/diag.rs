//! Turns the raw measurements into plain-language findings.

use crate::events::EventReport;
use crate::net::{is_on, Adapter};
use crate::ping::PingStats;
use crate::speed::SpeedReport;
use crate::usb::UsbTree;
use crate::util::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sev {
    Problem,
    Warn,
    Info,
}

pub struct Finding {
    pub sev: Sev,
    pub text: String,
}

#[derive(Default)]
pub struct Inputs<'a> {
    pub usb: Option<&'a UsbTree>,
    pub adapters: Option<&'a [Adapter]>,
    pub events: Option<&'a EventReport>,
    pub pings: Option<&'a [PingStats]>,
    pub speed: Option<&'a SpeedReport>,
}

pub fn analyze(i: &Inputs) -> Vec<Finding> {
    let mut f = Vec::new();
    let mut add = |sev, text: String| f.push(Finding { sev, text });

    if let Some(usb) = i.usb {
        for issue in &usb.issues {
            add(if issue.problem { Sev::Problem } else { Sev::Warn }, issue.text.clone());
        }
    }

    if let Some(adapters) = i.adapters {
        // Disconnected adapters (e.g. a hub's unused built-in port) are not worth hints.
        for a in adapters.iter().filter(|a| !a.wireless && a.connected) {
            let name = format!("{} ({})", a.alias, a.description);
            if a.gigabit_capable() && a.link_bps > 0 && a.link_bps <= 100_000_000 {
                add(
                    Sev::Problem,
                    format!(
                        "{name} is a gigabit adapter but the link came up at {}: check the cable (Cat5e or better) and the router port",
                        fmt_bps(a.link_bps as f64)
                    ),
                );
            }
            if a.fast_ethernet_only() {
                add(Sev::Info, format!("{name} is Fast Ethernet: it can never go above 100 Mbps"));
            }
            let errs = a.in_errors + a.out_errors;
            let discards = a.in_discards + a.out_discards;
            if errs > 0 {
                add(Sev::Warn, format!("{name} has {errs} packet errors since it was connected"));
            }
            if discards > 100 {
                add(Sev::Info, format!("{name} has dropped {discards} packets (discards)"));
            }

            if let (Some(usb), Some(inst)) = (i.usb, &a.instance) {
                if a.is_usb() {
                    if let Some(loc) = usb.locate(inst) {
                        if loc.external_hubs > 0 {
                            let sev = if loc.bus_powered_hub { Sev::Warn } else { Sev::Info };
                            add(
                                sev,
                                format!(
                                    "{name} goes through {} USB hub(s): {}{}. A direct laptop port is more reliable",
                                    loc.external_hubs,
                                    loc.chain.join(" → "),
                                    if loc.bus_powered_hub { " (bus-powered hub: it shares the laptop port's power)" } else { "" }
                                ),
                            );
                        }
                        if a.gigabit_capable() && !loc.superspeed {
                            add(
                                Sev::Warn,
                                format!(
                                    "{name} runs on a USB 2.0 link ({}): real throughput tops out around 300 Mbps",
                                    loc.speed
                                ),
                            );
                        }
                    }
                }
            }

            if a.is_usb() {
                let eee = a.setting("*EEE").or_else(|| a.setting("EEE"));
                if eee.map(is_on).unwrap_or(false) {
                    add(Sev::Info, crate::sys::eee_hint(&name));
                }
                if a.setting("*SelectiveSuspend").map(is_on).unwrap_or(false) {
                    add(Sev::Info, crate::sys::suspend_hint(&name));
                }
            }
        }
    }

    if let Some(ev) = i.events {
        // The most recent time a connected USB network adapter was plugged in.
        let arrival = i
            .adapters
            .unwrap_or(&[])
            .iter()
            .filter(|a| a.connected && a.is_usb())
            .filter_map(|a| a.instance.as_deref().and_then(last_arrival).map(|t| (t, a.alias.clone())))
            .max_by_key(|(t, _)| *t);

        for g in ev.groups().into_iter().filter(|g| g.problem) {
            let what = if g.network_driver { "Network driver" } else { "USB stack" };
            let mut text = format!(
                "{what} {}: \"{}\" {}× (last {})",
                g.provider,
                g.text.chars().take(90).collect::<String>(),
                g.count,
                g.last.short()
            );
            let t = g.text.to_lowercase();
            if t.contains("pipe") || t.contains("urb") || t.contains("bulk") || t.contains("transfer") {
                text.push_str(". These are USB transfer errors: usually a weak or overloaded hub, low power, or a bad cable/port");
            }
            let mut sev = Sev::Warn;
            if let Some((at, alias)) = &arrival {
                // Errors logged within 30 s of plugging in are start-up noise.
                let after = ev
                    .driver_events
                    .iter()
                    .filter(|e| e.provider == g.provider && crate::events::fold_key(&e.text) == g.key && e.time.secs_since(*at) > 30)
                    .count();
                if after == 0 {
                    sev = Sev::Info;
                    if g.last < *at {
                        text.push_str(&format!(". None since {alias} was last plugged in ({})", at.short()));
                    } else {
                        text.push_str(". Only at plug-in, harmless");
                    }
                }
            }
            add(sev, text);
        }
        let d = ev.disconnects();
        if d as u32 > ev.days * 3 {
            add(Sev::Warn, format!("{} lost the network {d} times in the last {} days", crate::sys::OS_NAME, ev.days));
        }
    }

    if let Some(pings) = i.pings {
        let gw = pings.iter().filter(|p| p.label.starts_with("gateway")).collect::<Vec<_>>();
        let inet = pings.iter().filter(|p| !p.label.starts_with("gateway")).collect::<Vec<_>>();
        let gw_loss = gw.iter().any(|p| p.loss_pct() > 0.0);
        for p in &gw {
            if p.loss_pct() > 0.0 {
                add(
                    Sev::Problem,
                    format!(
                        "{:.0}% packet loss to your router ({}): the problem is on your side (cable, adapter, USB hub or Wi-Fi)",
                        p.loss_pct(),
                        p.target
                    ),
                );
            }
            if p.jitter() > 5.0 || p.max() > 50.0 {
                add(
                    Sev::Warn,
                    format!(
                        "Unstable latency to your router ({}): jitter {:.1} ms, max {:.0} ms (should be ~1 ms on a cable)",
                        p.target,
                        p.jitter(),
                        p.max()
                    ),
                );
            }
        }
        for p in &inet {
            if p.loss_pct() >= 2.0 && !gw_loss {
                add(
                    Sev::Warn,
                    format!("{:.0}% loss to {} but none to the router: the loss is past your network (ISP)", p.loss_pct(), p.target),
                );
            }
            if p.jitter() > 15.0 {
                add(Sev::Warn, format!("High jitter to {}: {:.1} ms", p.target, p.jitter()));
            }
        }

        if let Some(sp) = i.speed {
            if let (Some(loaded), Some(idle)) = (&sp.loaded_ping, inet.first()) {
                if !loaded.rtts.is_empty() && !idle.rtts.is_empty() {
                    let extra = loaded.avg() - idle.avg();
                    if extra > 100.0 {
                        add(
                            Sev::Warn,
                            format!(
                                "Latency rises by {extra:.0} ms while downloading (bufferbloat): calls and games will lag during downloads"
                            ),
                        );
                    }
                }
            }
        }
    }

    if let Some(sp) = i.speed {
        if let (Some(down), Some(adapters)) = (sp.best_download(), i.adapters) {
            if let Some(a) = adapters.iter().find(|a| a.connected && !a.wireless) {
                if a.link_bps > 0 && down > a.link_bps as f64 * 0.85 {
                    add(
                        Sev::Info,
                        format!(
                            "Download ({}) is close to the {} link speed: the adapter is the limit, not your internet",
                            fmt_bps(down),
                            fmt_bps(a.link_bps as f64)
                        ),
                    );
                }
            }
        }
    }

    f.sort_by_key(|x| x.sev);
    f
}

pub fn print(findings: &[Finding]) {
    heading("Diagnosis");
    if !findings.iter().any(|f| f.sev != Sev::Info) {
        println!("{}", green("✔ No problems found."));
    }
    for f in findings {
        let mark = match f.sev {
            Sev::Problem => red("✖"),
            Sev::Warn => yellow("⚠"),
            Sev::Info => cyan("•"),
        };
        println!("{mark} {}", f.text);
    }
}
