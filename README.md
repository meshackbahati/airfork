# airfork

Experimental. Tested on **Intel Wireless-AC 9260** with `iwlwifi`.
MIT licensed.

![airfork GUI](screenshots/gui-cropped.png)

## Function

Runs a station and a virtual AP on one Wi-Fi card, or uses a second adapter
or a cable as the uplink. It reads the driver's
interface combinations, marks the virtual interface unmanaged in NetworkManager,
limits the station to 2.4 GHz because 5 GHz is `PASSIVE-SCAN` on this chip,
disables 802.11v BTM, then starts hostapd with 802.11n and WMM, plus dnsmasq,
source NAT through private nftables tables (iptables fallback) and a policy
route for `192.168.42.0/24`. The AP address is derived from the station MAC; a watcher follows station
channel changes; and a record in `/var/lib/airfork` restores the Wi-Fi profile
after a crash. `stop` reverses all of it.

The GUI polls status and DHCP leases every two seconds, so a session started
from the terminal also shows as running and switches the button to `Running`.
The CLI exposes the same operations.

## Limits

`iwlwifi` reports `#channels <= 1` for station plus AP. The AP cannot transmit
on 5 GHz here, so a session started from 5 GHz moves the station to 2.4 GHz and
waits for DHCP, roughly 40 seconds. Later starts are immediate. Throughput is
shared. This is a lab tool for your own connection, not a jammer.

A NetworkManager dispatcher hook keeps 802.11v BTM disabled on `wlan0` so the
router cannot steer the station between its radios. Drops that the router
initiates itself (deauthentication reason 2) cannot be prevented from the
client; the router must be configured to stop them.

## Build

Requires `iw`, `ip`, `hostapd`, `dnsmasq`, `iptables`, NetworkManager and GTK4
with libadwaita.

```sh
cargo build --release
./install.sh
```

## Use

```sh
airfork doctor                  # is this machine ready, and in which mode?
airfork detect                  # adapter details
airfork sources                 # interfaces with a default route
airfork plan --uplink wlan0
airfork start --dry-run --uplink wlan0
sudo airfork start --ssid LabHotspot --password airfork-2026 --uplink wlan0
# dedicated: sudo airfork start --uplink enp3s0 --channel 6
sudo airfork stop
```

Other flags: `--ap-base IFACE`, `--hidden`, `--channel N`. GUI defaults:
`LabHotspot` / `airfork-2026`. Pick the internet source before starting.

GitHub Actions runs format, check, clippy, tests and a release build on every
push and pull request. Tagging `v*` publishes a tarball with both binaries,
the GUI launcher, dispatcher hook and docs.

## Layout

```
src/lib.rs   detection, start, stop, clients, rollback
src/cli.rs   CLI frontend
src/gui.rs   GTK frontend
```

## License

MIT. Copyright (c) 2026 the airfork contributors. See `LICENSE`.
