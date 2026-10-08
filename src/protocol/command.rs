//! Parsing helpers for the commands that carry addresses or sizes in their
//! replies: `PASV`, `EPSV`, `SIZE`, `MDTM`.

use std::net::{IpAddr, Ipv4Addr};

use crate::error::{Error, Result};

/// Where a passive data connection should be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassiveAddr {
    pub host: Option<IpAddr>,
    pub port: u16,
}

/// Parse the parenthesised payload of a reply, e.g. `(192,168,1,10,195,44)`.
fn parenthesised(reply: &str) -> Option<&str> {
    let open = reply.find('(')?;
    let close = reply[open..].find(')')? + open;
    Some(&reply[open + 1..close])
}

/// `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`.
///
/// Some servers omit the parentheses or pad with text, so scan for the six
/// trailing numbers rather than trusting the exact wording.
pub fn parse_pasv(reply: &str) -> Result<PassiveAddr> {
    let payload = parenthesised(reply).unwrap_or(reply);
    let numbers: Vec<u16> = payload
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();

    if numbers.len() < 6 {
        return Err(Error::Protocol(format!("malformed PASV reply: {reply}")));
    }
    let tail = &numbers[numbers.len() - 6..];
    if tail[..4].iter().any(|n| *n > 255) || tail[4] > 255 || tail[5] > 255 {
        return Err(Error::Protocol(format!("PASV octet out of range: {reply}")));
    }
    let host = Ipv4Addr::new(tail[0] as u8, tail[1] as u8, tail[2] as u8, tail[3] as u8);
    let port = tail[4] * 256 + tail[5];
    Ok(PassiveAddr {
        host: Some(IpAddr::V4(host)),
        port,
    })
}

/// `229 Entering Extended Passive Mode (|||6446|)`.
///
/// The protocol family and address fields are usually empty, so take the port
/// from the fourth `|`-separated field.
pub fn parse_epsv(reply: &str) -> Result<PassiveAddr> {
    let payload = parenthesised(reply)
        .ok_or_else(|| Error::Protocol(format!("malformed EPSV reply: {reply}")))?;
    let fields: Vec<&str> = payload.split('|').collect();
    let port_field = fields
        .get(3)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Protocol(format!("EPSV reply has no port: {reply}")))?;
    let port: u16 = port_field
        .parse()
        .map_err(|_| Error::Protocol(format!("EPSV port not a number: {reply}")))?;
    if port == 0 {
        return Err(Error::Protocol(format!("EPSV port is zero: {reply}")));
    }
    Ok(PassiveAddr { host: None, port })
}

/// `213 1234567` -> `1234567`.
///
/// Callers may hand over either a raw reply line or its already stripped text,
/// so a leading reply code is tolerated but never mistaken for the size.
pub fn parse_size(reply: &str) -> Result<u64> {
    let text = crate::protocol::reply::strip_code_prefix(reply);
    let token = text
        .split_whitespace()
        .find(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| Error::Protocol(format!("SIZE reply has no number: {reply}")))?;
    token
        .parse::<u64>()
        .map_err(|_| Error::Protocol(format!("SIZE reply not a number: {reply}")))
}

/// `213 20240102030405` -> `20240102030405`.
pub fn parse_mdtm(reply: &str) -> Result<String> {
    let text = crate::protocol::reply::strip_code_prefix(reply);
    let token = text
        .split_whitespace()
        .find(|t| t.len() >= 14 && t.chars().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| Error::Protocol(format!("MDTM reply has no timestamp: {reply}")))?;
    Ok(token.to_string())
}

/// Decide which address to actually connect to for a passive transfer.
///
/// Servers behind NAT frequently hand out their internal address; when that
/// address is obviously unusable from here we fall back to the address of the
/// control connection.
pub fn resolve_pasv_target(advertised: &PassiveAddr, control_peer: Option<IpAddr>) -> PassiveAddr {
    let unusable = match (advertised.host, control_peer) {
        (None, _) => true,
        (Some(IpAddr::V4(v4)), _) if v4.is_unspecified() => true,
        (Some(IpAddr::V4(v4)), Some(peer)) if v4.is_loopback() && !peer.is_loopback() => true,
        _ => false,
    };
    if unusable {
        PassiveAddr {
            host: control_peer,
            port: advertised.port,
        }
    } else {
        advertised.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pasv() {
        let a = parse_pasv("227 Entering Passive Mode (192,168,1,10,195,44)").unwrap();
        assert_eq!(a.host.unwrap().to_string(), "192.168.1.10");
        assert_eq!(a.port, 195 * 256 + 44);
    }

    #[test]
    fn parses_pasv_without_parens() {
        let a = parse_pasv("227 =127,0,0,1,4,1").unwrap();
        assert_eq!(a.host.unwrap().to_string(), "127.0.0.1");
        assert_eq!(a.port, 4 * 256 + 1);
    }

    #[test]
    fn rejects_short_pasv() {
        assert!(parse_pasv("227 Entering Passive Mode (1,2,3)").is_err());
    }

    #[test]
    fn rejects_out_of_range_pasv() {
        assert!(parse_pasv("227 Entering Passive Mode (300,1,1,1,1,1)").is_err());
    }

    #[test]
    fn parses_epsv() {
        let a = parse_epsv("229 Entering Extended Passive Mode (|||6446|)").unwrap();
        assert_eq!(a.port, 6446);
        assert!(a.host.is_none());
    }

    #[test]
    fn parses_epsv_with_family() {
        let a = parse_epsv("229 Entering Extended Passive Mode (|1|10.0.0.1|51000|)").unwrap();
        assert_eq!(a.port, 51000);
    }

    #[test]
    fn parses_size_and_mdtm() {
        assert_eq!(parse_size("213 1234567").unwrap(), 1234567);
        assert_eq!(parse_size("213 0").unwrap(), 0);
        // Bare numbers work too, for callers that already stripped the code.
        assert_eq!(parse_size("1234567").unwrap(), 1234567);
        assert_eq!(parse_mdtm("213 20240102030405").unwrap(), "20240102030405");
        assert!(parse_size("213 no digits here").is_err());
    }

    #[test]
    fn falls_back_to_control_peer_for_nat_addresses() {
        let peer: IpAddr = "203.0.113.9".parse().unwrap();
        let advertised = parse_pasv("227 Entering Passive Mode (10,0,0,5,4,1)").unwrap();
        // A private address is still usable in principle, so keep it.
        assert_eq!(
            resolve_pasv_target(&advertised, Some(peer)).host,
            advertised.host
        );

        let zero = parse_pasv("227 Entering Passive Mode (0,0,0,0,4,1)").unwrap();
        assert_eq!(resolve_pasv_target(&zero, Some(peer)).host, Some(peer));

        let loopback = parse_pasv("227 Entering Passive Mode (127,0,0,1,4,1)").unwrap();
        assert_eq!(resolve_pasv_target(&loopback, Some(peer)).host, Some(peer));

        let local_peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            resolve_pasv_target(&loopback, Some(local_peer)).host,
            Some(local_peer)
        );
    }
}
