//! `kith://` links, which open Kith from outside: the Watch button on a
//! Windows notification, and anything else that can open a link.
//!
//! The system hands a clicked link to `kith open <link>`, which passes it to
//! the running Kith (or starts one).

use std::str::FromStr;

use anyhow::{Result, bail};
use iroh::EndpointId;

pub const SCHEME: &str = "kith";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// `kith://open`: show the window.
    Open,
    /// `kith://watch/<code>`: open a friend's stream.
    Watch(EndpointId),
}

impl FromStr for Link {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        let rest = text
            .strip_prefix(SCHEME)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(|rest| rest.trim_start_matches('/'));
        let Some(rest) = rest else {
            bail!("{text:?} isn't a Kith link (they start with {SCHEME}://)");
        };
        let mut parts = rest.trim_end_matches('/').split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("open" | ""), None, None) => Ok(Self::Open),
            (Some("watch"), Some(code), None) => match EndpointId::from_str(code) {
                Ok(code) => Ok(Self::Watch(code)),
                Err(_) => bail!("the link {text:?} names no friend's code"),
            },
            _ => bail!("Kith doesn't know what to do with {text:?}; is it up to date?"),
        }
    }
}

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "{SCHEME}://open"),
            Self::Watch(code) => write!(f, "{SCHEME}://watch/{code}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "9f79d3dea4117e8337f2780e871a3d10b2b1106ac655f97447f78b090b6c58c3";

    #[test]
    fn links_round_trip() {
        let watch: Link = format!("kith://watch/{CODE}").parse().unwrap();
        assert_eq!(watch, Link::Watch(CODE.parse().unwrap()));
        assert_eq!(watch.to_string().parse::<Link>().unwrap(), watch);
        assert_eq!("kith://open".parse::<Link>().unwrap(), Link::Open);
    }

    #[test]
    fn systems_spell_links_loosely() {
        // Windows hands over what was clicked; some launchers add or drop slashes.
        assert_eq!("kith:open".parse::<Link>().unwrap(), Link::Open);
        assert_eq!("kith://".parse::<Link>().unwrap(), Link::Open);
        let watch = format!("kith:///watch/{CODE}/");
        assert!(matches!(watch.parse::<Link>().unwrap(), Link::Watch(_)));
    }

    #[test]
    fn strangers_are_refused() {
        assert!("https://example.com".parse::<Link>().is_err());
        assert!("kith://watch/sam".parse::<Link>().is_err());
        assert!("kith://format/c".parse::<Link>().is_err());
    }
}
