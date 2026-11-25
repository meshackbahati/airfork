# airfork

Experimental. Tested on **Intel Wireless-AC 9260** with `iwlwifi`.
MIT, Meshack Bahati.

![airfork GUI](screenshots/gui-cropped.png)

## Function

Runs a station and a virtual AP on one Wi-Fi card. It reads the driver's
interface combinations, marks the virtual interface unmanaged in NetworkManager,
limits the station to 2.4 GHz because 5 GHz is `PASSIVE-SCAN` on this chip,
disables 802.11v BTM during the session, then starts hostapd and dnsmasq and
adds source NAT plus a policy route for `192.168.42.0/24`. The AP address is
derived from the station MAC, and a watcher follows station channel changes.
`stop` reverses all of it.

The GUI polls status and DHCP leases every two seconds; the CLI exposes the
same operations.

## Limits

`iwlwifi` reports `#channels <= 1` for station plus AP. The AP cannot transmit
on 5 GHz here, so a session started from 5 GHz moves the station to 2.4 GHz and
waits for DHCP, roughly 40 seconds. Later starts are immediate. Throughput is
shared. This is a lab tool for your own connection, not a jammer.

## Build

Requires `iw`, `ip`, `hostapd`, `dnsmasq`, `iptables`, NetworkManager and GTK4
with libadwaita.

```sh
cargo build --release
./install.sh
```

## Use

```sh
airfork detect
airfork sources
airfork plan --uplink wlan0
sudo airfork start --ssid LabHotspot --password airfork-2026 --uplink wlan0
airfork status
sudo airfork stop
```

GUI defaults: `LabHotspot` / `airfork-2026`. Pick the internet source before
starting.

## Layout

```
src/lib.rs   detection, start, stop, clients, rollback
src/cli.rs   CLI frontend
src/gui.rs   GTK frontend
```

## License

MIT. Copyright (c) 2026 Meshack Bahati. See `LICENSE`.
