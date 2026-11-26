//! airfork core, split into modules so no file passes 500 lines.
#![allow(ambiguous_glob_reexports)]

mod config;
mod model;
mod network;
mod restore;
mod session;
mod util;
mod watch;
mod wifi;

pub use config::*;
pub use model::*;
pub use network::*;
pub use restore::*;
pub use session::*;
pub use util::*;
pub use watch::*;
pub use wifi::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_from_frequencies() {
        assert_eq!(frequency_to_channel(2412), Some(1));
        assert_eq!(frequency_to_channel(2472), Some(13));
        assert_eq!(frequency_to_channel(2484), Some(14));
        assert_eq!(frequency_to_channel(5220), Some(44));
        assert_eq!(frequency_to_channel(5955), Some(1));
        assert_eq!(frequency_to_channel(100), None);
    }

    #[test]
    fn bands_from_frequencies() {
        assert_eq!(band_for_frequency(2437), Some(Band::B2_4));
        assert_eq!(band_for_frequency(5220), Some(Band::B5));
        assert_eq!(band_for_frequency(5955), Some(Band::B6));
        assert_eq!(band_for_frequency(100), None);
    }

    #[test]
    fn iw_dev_blocks() {
        let sample = "phy#0\n\tInterface wlan0\n\t\tifindex 3\n\t\ttype managed\n\t\tssid Lab\n\t\tchannel 44 (5220 MHz), width: 80 MHz\n";
        let entries = parse_iw_dev(sample);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].phy, "phy0");
        assert_eq!(entries[0].name, "wlan0");
        assert_eq!(entries[0].kind, "managed");
        assert_eq!(entries[0].freq, Some(5220));
        assert_eq!(entries[0].ssid.as_deref(), Some("Lab"));
    }

    #[test]
    fn iwlwifi_combinations() {
        let sample = "\
\tvalid interface combinations:
\t\t * #{ managed } <= 1, #{ P2P-client, P2P-GO } <= 1, #{ P2P-device } <= 1,
\t\t   total <= 3, #channels <= 2
\t\t * #{ managed } <= 1, #{ AP, P2P-client, P2P-GO } <= 1, #{ P2P-device } <= 1,
\t\t   total <= 3, #channels <= 1
\tHT Capability overrides:
";
        let blocks = parse_combinations(sample);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("managed") && !blocks[0].contains("AP"));
        assert!(blocks[1].contains("managed") && blocks[1].contains("AP"));
        assert_eq!(max_channels(&blocks[1]), Some(1));
    }

    #[test]
    fn modes_and_freq_flags() {
        let info = "Supported interface modes:\n\t\t * managed\n\t\t * AP\n\tband 2:\n\t\t* 5220 MHz [44] (20.0 dBm) (no IR)\n";
        assert!(supported_modes(info).iter().any(|mode| mode == "AP"));
        assert_eq!(freq_flags(info, Some(5220)), (true, false));
        assert_eq!(freq_flags(info, Some(2437)), (false, false));
    }

    #[test]
    fn hostapd_conf_enables_80211n_and_wmm() {
        let conf = render_hostapd_conf(6, Band::B2_4, &HotspotConfig::default());
        assert!(conf.contains("channel=6\n"));
        assert!(conf.contains("ieee80211n=1\n"));
        assert!(conf.contains("wmm_enabled=1\n"));
    }

    #[test]
    fn forward_rules_cover_both_directions() {
        let rules = forward_rules();
        assert!(rules[0].contains(&"-s") && rules[0].contains(&CLIENT_SUBNET));
        assert!(rules[1].contains(&"-d") && rules[1].contains(&"RELATED,ESTABLISHED"));
        assert!(
            rules
                .iter()
                .all(|rule| rule[0] == "FORWARD" && rule.contains(&NAT_COMMENT))
        );
    }

    #[test]
    fn channel_info_parser() {
        let info = "Interface wlan0\n\ttype managed\n\tchannel 6 (2437 MHz), width: 20 MHz\n";
        assert_eq!(parse_channel_info(info), Some((2437, 6)));
        assert_eq!(parse_channel_info("no channel here"), None);
    }

    #[test]
    fn derived_ap_mac() {
        let station = "c0:b8:83:68:05:6a";
        let ap = derive_mac(station).unwrap();
        let first = u8::from_str_radix(&ap[..2], 16).unwrap();
        assert_eq!(first & 0x01, 0);
        assert_eq!(first & 0x02, 0x02);
        assert_ne!(ap, station);
        assert_eq!(
            derive_mac("02:00:00:00:01:01").unwrap(),
            "02:00:00:00:5b:01"
        );
        assert!(derive_mac("bad").is_none());
    }

    #[test]
    fn config_checks() {
        let mut config = HotspotConfig::default();
        assert!(config.validate().is_ok());
        config.ssid = "x".repeat(33);
        assert!(config.validate().is_err());
        config.ssid = "lab".into();
        config.password = "short".into();
        assert!(config.validate().is_err());
        config.password = "x".repeat(64);
        assert!(config.validate().is_err());
        config.password = "longenough".into();
        config.ap_interface = "bad-name".into();
        assert!(config.validate().is_err());
    }
}
