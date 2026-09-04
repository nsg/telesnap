use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::error::AppError;

pub fn snap_name(value: &str) -> Result<(), AppError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 40
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && bytes.iter().any(u8::is_ascii_lowercase)
        && !value.ends_with('-')
        && !value.contains("--");
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest("invalid snap name".to_owned()))
    }
}

pub fn config_key(value: &str) -> Result<(), AppError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 64
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest("invalid configuration key".to_owned()))
    }
}

pub fn service_name(value: &str) -> Result<(), AppError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest("invalid service name".to_owned()))
    }
}

pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => forbidden_v4(ip),
        IpAddr::V6(ip) => forbidden_v6(ip),
    }
}

fn forbidden_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240
}

fn forbidden_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[..6] == [0, 0, 0, 0, 0, 0] && segments[6..] != [0, 1])
        || ip.to_ipv4_mapped().is_some_and(forbidden_v4)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{config_key, is_forbidden_ip, service_name, snap_name};

    #[test]
    fn validates_snap_names() {
        for valid in ["hello", "hello-world", "a1"] {
            assert!(snap_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "-hello", "hello-", "hello--world", "123", "Hello"] {
            assert!(snap_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn validates_keys_and_services() {
        assert!(config_key("ports.http-1").is_ok());
        assert!(config_key("--help").is_err());
        assert!(config_key("ports..http").is_err());
        assert!(service_name("web-worker").is_ok());
        assert!(service_name("../unit").is_err());
    }

    #[test]
    fn blocks_non_public_addresses() {
        for address in [
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(100, 64, 0, 1),
            Ipv4Addr::new(192, 0, 2, 1),
        ] {
            assert!(is_forbidden_ip(IpAddr::V4(address)), "{address}");
        }
        assert!(!is_forbidden_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        for address in [
            Ipv6Addr::LOCALHOST,
            "fe80::1".parse().unwrap(),
            "fd00::1".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
            "::ffff:127.0.0.1".parse().unwrap(),
        ] {
            assert!(is_forbidden_ip(IpAddr::V6(address)), "{address}");
        }
        assert!(!is_forbidden_ip(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));
    }
}
