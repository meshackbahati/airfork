use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub fn uplink_gateway(interface: &str) -> Result<String> {
    let routes = stdout("ip", &["-4", "route", "show", "default", "dev", interface])?;
    routes
        .split_whitespace()
        .skip_while(|word| *word != "via")
        .nth(1)
        .map(str::to_string)
        .context("could not find the gateway for the uplink")
}

pub fn route_clients(gateway: &str, interface: &str) -> Result<()> {
    let existing = stdout("ip", &["rule", "show"])
        .unwrap_or_default()
        .contains(CLIENT_SUBNET);
    if !existing {
        run(
            "ip",
            &[
                "rule",
                "add",
                "from",
                CLIENT_SUBNET,
                "table",
                ROUTE_TABLE,
                "priority",
                "100",
            ],
        )?;
    }
    let args = [
        "route",
        "replace",
        "default",
        "via",
        gateway,
        "dev",
        interface,
        "table",
        ROUTE_TABLE,
    ];
    run("ip", &args)?;
    // a reconnect can wipe the table while the session is being set up
    for _ in 0..4 {
        let table = stdout("ip", &["route", "show", "table", ROUTE_TABLE]).unwrap_or_default();
        if table.contains("default") {
            return Ok(());
        }
        run("ip", &args)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

pub fn unroute_clients(gateway: &str, interface: &str) {
    let _ = run(
        "ip",
        &[
            "rule",
            "del",
            "from",
            CLIENT_SUBNET,
            "table",
            ROUTE_TABLE,
            "priority",
            "100",
        ],
    );
    let _ = run(
        "ip",
        &[
            "route",
            "del",
            "default",
            "via",
            gateway,
            "dev",
            interface,
            "table",
            ROUTE_TABLE,
        ],
    );
}

/// A device joined to the virtual access point, with its DHCP address if known.
pub fn run_stdin(program: &str, args: &[&str], input: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("could not execute {program}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

// private nftables tables are safer than touching the user's iptables chains
pub fn nft_rules(ap: &str) -> String {
    format!(
        "table ip airfork_nat {{\n  chain post {{\n    type nat hook postrouting priority 100;\n    ip saddr {CLIENT_SUBNET} oifname != \"{ap}\" masquerade\n  }}\n}}\n\
         table inet airfork_fw {{\n  chain input {{\n    type filter hook input priority -10;\n    iifname \"{ap}\" udp dport {{ 53, 67 }} accept\n    iifname \"{ap}\" tcp dport 53 accept\n  }}\n\
           chain forward {{\n    type filter hook forward priority -10;\n    iifname \"{ap}\" accept\n    oifname \"{ap}\" ct state established,related accept\n  }}\n}}\n"
    )
}

pub fn firewall_up(ap: &str) -> Result<String> {
    if tool_available("nft") {
        let _ = run("nft", &["delete", "table", "ip", "airfork_nat"]);
        let _ = run("nft", &["delete", "table", "inet", "airfork_fw"]);
        run_stdin("nft", &["-f", "-"], &nft_rules(ap))?;
        return Ok("nft".into());
    }
    let nat_args = [
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        CLIENT_SUBNET,
        "-j",
        "MASQUERADE",
        "-m",
        "comment",
        "--comment",
        NAT_COMMENT,
    ];
    run("iptables", &nat_args)?;
    allow_forwarding()?;
    Ok("iptables".into())
}

pub fn firewall_down(method: &str, ap: &str) {
    if method == "nft" {
        let _ = run("nft", &["delete", "table", "ip", "airfork_nat"]);
        let _ = run("nft", &["delete", "table", "inet", "airfork_fw"]);
        return;
    }
    let nat_args = [
        "-t",
        "nat",
        "-D",
        "POSTROUTING",
        "-s",
        CLIENT_SUBNET,
        "-j",
        "MASQUERADE",
        "-m",
        "comment",
        "--comment",
        NAT_COMMENT,
    ];
    let _ = run("iptables", &nat_args);
    remove_forwarding();
    let _ = ap;
}

// MASQUERADE alone is not enough when something (Docker, ufw, firewalld) has
// set the FORWARD policy to DROP: clients join but get no internet.
pub fn forward_rules() -> [Vec<&'static str>; 2] {
    [
        vec![
            "FORWARD",
            "-s",
            CLIENT_SUBNET,
            "-j",
            "ACCEPT",
            "-m",
            "comment",
            "--comment",
            NAT_COMMENT,
        ],
        vec![
            "FORWARD",
            "-d",
            CLIENT_SUBNET,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
            "-m",
            "comment",
            "--comment",
            NAT_COMMENT,
        ],
    ]
}

pub fn allow_forwarding() -> Result<()> {
    for rule in forward_rules() {
        let mut check = vec!["-C"];
        check.extend(rule.iter().copied());
        let present = Command::new("iptables")
            .args(&check)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !present {
            // insert at the top so it sits ahead of a DROP policy's chains
            let mut insert = vec!["-I", rule[0], "1"];
            insert.extend(rule[1..].iter().copied());
            run("iptables", &insert)?;
        }
    }
    Ok(())
}

pub fn remove_forwarding() {
    for rule in forward_rules() {
        let mut delete = vec!["-D"];
        delete.extend(rule.iter().copied());
        let _ = run("iptables", &delete);
    }
}

// the router periodically sends 802.11v "disassociation imminent" requests.
// ignoring them keeps the uplink steady while the AP shares the same radio.
