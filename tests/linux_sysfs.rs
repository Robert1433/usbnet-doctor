//! Runs the Linux build against a fake /sys: an xHCI controller with a USB 3
//! hub, a USB 3 Ethernet adapter behind it, and a USB 3 drive stuck at USB 2.0
//! speed. Checks the merged USB tree, the adapter's USB location and the
//! diagnosis.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sys(PathBuf);

impl Sys {
    fn file(&self, path: &str, content: &str) {
        let p = self.0.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, format!("{content}\n")).unwrap();
    }

    fn link(&self, path: &str, target: &str) {
        let p = self.0.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink(self.0.join(target), p).unwrap();
    }

    fn dir(&self, path: &str) {
        fs::create_dir_all(self.0.join(path)).unwrap();
    }

    /// A USB device at `path` (under the controller), listed in bus/usb/devices.
    #[allow(clippy::too_many_arguments)]
    fn device(&self, path: &str, name: &str, vid: &str, pid: &str, class: &str, speed: &str, version: &str, product: &str) {
        let d = format!("devices/pci0000:00/0000:00:14.0/{path}");
        for (k, v) in [
            ("idVendor", vid),
            ("idProduct", pid),
            ("bDeviceClass", class),
            ("speed", speed),
            ("version", version),
            ("product", product),
            ("bmAttributes", "e0"),
            ("busnum", &name.trim_start_matches("usb")[..1]),
        ] {
            self.file(&format!("{d}/{k}"), v);
        }
        self.link(&format!("bus/usb/devices/{name}"), &d);
    }

    /// A hub port; `peer` is the twin port's path under the controller.
    fn port(&self, hub_path: &str, iface: &str, port: &str, connect_type: &str, peer: Option<&str>) {
        let p = format!("devices/pci0000:00/0000:00:14.0/{hub_path}/{iface}/{port}");
        self.file(&format!("{p}/connect_type"), connect_type);
        self.file(&format!("{p}/over_current_count"), "0");
        if let Some(peer) = peer {
            self.link(&format!("{p}/peer"), &format!("devices/pci0000:00/0000:00:14.0/{peer}"));
        }
    }
}

fn fake_sys() -> PathBuf {
    let root = std::env::temp_dir().join(format!("usbnet-doctor-sysfs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let s = Sys(root.clone());
    s.file("devices/pci0000:00/0000:00:14.0/vendor", "0x8086");
    s.file("devices/pci0000:00/0000:00:14.0/device", "0xa0ed");

    // Root hubs: usb1 is the USB 2.0 bus, usb2 the USB 3 bus of the same controller.
    s.device("usb1", "usb1", "1d6b", "0002", "09", "480", " 2.00", "xHCI Host Controller");
    s.device("usb2", "usb2", "1d6b", "0003", "09", "10000", " 3.10", "xHCI Host Controller");
    s.port("usb1", "1-0:1.0", "usb1-port1", "hotplug", Some("usb2/2-0:1.0/usb2-port1"));
    s.port("usb1", "1-0:1.0", "usb1-port2", "hotplug", Some("usb2/2-0:1.0/usb2-port2"));
    s.port("usb1", "1-0:1.0", "usb1-port3", "hardwired", None);
    s.port("usb1", "1-0:1.0", "usb1-port4", "not used", None);
    s.port("usb2", "2-0:1.0", "usb2-port1", "hotplug", Some("usb1/1-0:1.0/usb1-port1"));
    s.port("usb2", "2-0:1.0", "usb2-port2", "hotplug", Some("usb1/1-0:1.0/usb1-port2"));

    // A USB 3 hub on port 1: 2-1 on the USB 3 bus, 1-1 on the USB 2.0 bus.
    s.device("usb2/2-1", "2-1", "2109", "0817", "09", "5000", " 3.20", "USB3.0 Hub");
    s.device("usb1/1-1", "1-1", "2109", "2817", "09", "480", " 2.10", "USB2.0 Hub");
    s.file("devices/pci0000:00/0000:00:14.0/usb2/2-1/bmAttributes", "a0"); // bus-powered
    for n in 1..=4 {
        s.port("usb2/2-1", "2-1:1.0", &format!("2-1-port{n}"), "unknown", Some(&format!("usb1/1-1/1-1:1.0/1-1-port{n}")));
        s.port("usb1/1-1", "1-1:1.0", &format!("1-1-port{n}"), "unknown", Some(&format!("usb2/2-1/2-1:1.0/2-1-port{n}")));
    }

    // A gigabit USB 3 Ethernet adapter on hub port 3, linked at only 100 Mbps.
    s.device("usb2/2-1/2-1.3", "2-1.3", "0bda", "8153", "00", "5000", " 3.00", "USB 10/100/1000 LAN");
    s.file("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/manufacturer", "Realtek");
    s.file("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/power/control", "auto");
    s.file("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/power/autosuspend_delay_ms", "2000");
    s.file("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/power/connected_duration", "60000");
    s.dir("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/2-1.3:1.0");
    s.dir("drivers/r8152");
    s.link("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/2-1.3:1.0/driver", "drivers/r8152");
    let net = "devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/2-1.3:1.0/net/enxfake0";
    for (k, v) in [("type", "1"), ("carrier", "1"), ("operstate", "up"), ("speed", "100"), ("duplex", "full"), ("mtu", "1500"), ("address", "00:e0:4c:00:00:01")] {
        s.file(&format!("{net}/{k}"), v);
    }
    for k in ["rx_bytes", "tx_bytes", "rx_errors", "tx_errors", "rx_dropped", "tx_dropped"] {
        s.file(&format!("{net}/statistics/{k}"), "0");
    }
    s.link(&format!("{net}/device"), "devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/2-1.3:1.0");
    s.link("class/net/enxfake0", net);

    // A USB 3 drive on root port 2 that only came up at USB 2.0 speed.
    s.device("usb1/1-2", "1-2", "174c", "55aa", "00", "480", " 3.20", "Portable SSD");
    // A built-in camera on the hard-wired port.
    s.device("usb1/1-3", "1-3", "13d3", "56ff", "ef", "480", " 2.01", "Integrated Camera");
    root
}

fn run(sys: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_usbnet-doctor"))
        .args(args)
        .arg("--no-color")
        .env("USBNET_DOCTOR_SYSFS", sys)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn usb_tree_and_adapter_location() {
    let sys = fake_sys();
    let usb = run(&sys, &["usb"]);
    println!("{usb}");
    // One controller heading for both root hubs.
    assert_eq!(usb.matches("[0000:00:14.0]").count(), 1, "{usb}");
    // USB 3 port 1 merged with its USB 2.0 twin, hub behind it.
    assert!(usb.contains("Port 1/1") && usb.contains("USB3.0 Hub [2109:0817]") && usb.contains("USB 2.0 companion [2109:2817]"), "{usb}");
    assert!(usb.contains("USB 10/100/1000 LAN (Realtek)") && usb.contains("SuperSpeed 5 Gbps"), "{usb}");
    assert!(usb.contains("Portable SSD") && usb.contains("USB 3 device running at USB 2.0 speed"), "{usb}");
    assert!(usb.contains("internal") && usb.contains("Integrated Camera"), "{usb}");
    // The unused port is hidden; the USB 2.0 hub twin is not shown on its own.
    assert!(!usb.contains("Port 4  ") && !usb.contains("not linked"), "{usb}");

    let net = run(&sys, &["net"]);
    println!("{net}");
    assert!(net.contains("enxfake0") && net.contains("connected, link 100 Mbps"), "{net}");
    assert!(net.contains("USB  SuperSpeed 5 Gbps  via Root hub Port 1/1 → USB3.0 Hub Port 3/3"), "{net}");
    assert!(net.contains("USB autosuspend: on (after 2000 ms idle)"), "{net}");
    assert!(net.contains("Plugged in at"), "{net}");
    assert!(net.contains("is a gigabit adapter but the link came up at 100 Mbps"), "{net}");
    assert!(net.contains("goes through 1 USB hub(s)") && net.contains("bus-powered hub"), "{net}");
    assert!(net.contains("USB autosuspend is on"), "{net}");

    let _ = fs::remove_dir_all(&sys);
}
